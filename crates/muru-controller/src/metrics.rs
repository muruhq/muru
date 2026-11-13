//! The metrics listener (design specification section 11.1): plain HTTP
//! on an internal address, answering GET /metrics with the node's
//! Prometheus text exposition, and `/livez`, `/readyz` and `/startupz`
//! with the daemon's probes (section 13.9), for something outside a
//! container that can only make HTTP requests.

use bytes::Bytes;
use http::header::{CACHE_CONTROL, CONTENT_TYPE};
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::daemon::Command;

/// The content type of the Prometheus text exposition.
pub const TEXT_FORMAT: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Serve `/metrics` on the listener until accepting fails, asking the
/// daemon for the exposition on each request, and the probes.
pub async fn serve(
    listener: TcpListener,
    commands: mpsc::Sender<Command>,
    probes: crate::probes::Probes,
) -> anyhow::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let commands = commands.clone();
        let probes = probes.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let commands = commands.clone();
                let probes = probes.clone();
                async move {
                    Ok::<_, std::convert::Infallible>(respond(&request, &commands, &probes).await)
                }
            });

            if let Err(error) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!(%error, "metrics connection ended with an error");
            }
        });
    }
}

async fn respond(
    request: &Request<Incoming>,
    commands: &mpsc::Sender<Command>,
    probes: &crate::probes::Probes,
) -> Response<Full<Bytes>> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "only GET is served\n");
    }

    let probe = match request.uri().path() {
        "/livez" => Some("live"),
        "/readyz" => Some("ready"),
        "/startupz" => Some("startup"),
        _ => None,
    };

    if let Some(probe) = probe {
        let (status, text) = match probes.answer(probe, "", 1) {
            Ok(()) => (StatusCode::OK, "ok\n".to_owned()),
            Err(why) => (StatusCode::SERVICE_UNAVAILABLE, format!("{why}\n")),
        };

        let mut response = Response::new(Full::new(Bytes::from(text)));
        *response.status_mut() = status;
        response.headers_mut().insert(
            CONTENT_TYPE,
            "text/plain; charset=utf-8".parse().expect("static"),
        );
        response
            .headers_mut()
            .insert(CACHE_CONTROL, "no-store".parse().expect("static"));
        return response;
    }

    if request.uri().path() != "/metrics" {
        return plain(
            StatusCode::NOT_FOUND,
            "not found; the metrics are at /metrics, the probes at /livez, /readyz and /startupz\n",
        );
    }

    let (done_tx, done_rx) = oneshot::channel();

    if commands
        .send(Command::Metrics { done: done_tx })
        .await
        .is_err()
    {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "the daemon is stopping\n");
    }

    let Ok(text) = done_rx.await else {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "the daemon is stopping\n");
    };

    let mut response = Response::new(Full::new(Bytes::from(text)));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, TEXT_FORMAT.parse().expect("static"));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, "no-store".parse().expect("static"));
    response
}

fn plain(status: StatusCode, text: &'static str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from_static(text.as_bytes())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        "text/plain; charset=utf-8".parse().expect("static"),
    );
    response
}
