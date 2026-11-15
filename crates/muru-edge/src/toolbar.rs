//! The development toolbar (design specification section 4.20, ADR
//! 0062): the reserved prefix `/_muru/`, which the edge answers from the
//! run's request records, and the `<script>` tag it inserts into HTML
//! pages when the run asks for the toolbar.

use std::collections::HashSet;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use http::header::{
    CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ETAG, HeaderMap, HeaderValue,
};
use http::{Method, Response, StatusCode};
use http_body::{Body, Frame};
use http_body_util::BodyExt as _;
use http_body_util::combinators::UnsyncBoxBody;

use muru_proto::admin::v1 as admin;
use percent_encoding::percent_decode_str;
use serde::Serialize;

use crate::development::Links;
use crate::panels::Renderer;
use crate::recorder::Recorder;
use crate::{BoxError, ResponseBody};

/// The prefix the edge answers itself in a development run.
pub const PREFIX: &str = "/_muru/";

/// The toolbar's script, embedded in the binary.
pub const SCRIPT: &str = include_str!("../toolbar/toolbar.js");

/// The toolbar's stylesheet, with the panels' classes after its own.
pub fn stylesheet() -> String {
    format!(
        "{}\n{}",
        include_str!("../toolbar/toolbar.css"),
        crate::panels::STYLESHEET
    )
}

/// What marks a response whose body is a file the edge sends itself
/// (section 4.9), which the toolbar leaves alone.
#[derive(Debug, Clone, Copy)]
pub struct EdgeFile;

/// The most requests `requests?ids=` answers for at once.
pub const MAX_BATCH: usize = 100;

/// The closing tag the toolbar goes before.
const BODY_END: &[u8] = b"</body>";

/// Whether a request names the reserved prefix.
pub fn reserved(path: &str) -> bool {
    path.starts_with(PREFIX)
}

/// Answer a request under the reserved prefix: the toolbar's files, a
/// record as JSON, one of its panels drawn, or the summaries of several
/// records. Only GET and HEAD, only from a loopback host or one of the
/// run's own names, and never from another site's page.
pub fn answer(
    parts: &http::request::Parts,
    links: &Links,
    recorder: &Recorder,
    renderer: &Renderer,
) -> Response<ResponseBody> {
    if parts.method != Method::GET && parts.method != Method::HEAD {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "only GET is served here\n");
    }

    if let Err(why) = allowed(&parts.headers, links.hosts()) {
        return plain(StatusCode::FORBIDDEN, why);
    }

    let within = parts.uri.path().strip_prefix(PREFIX).unwrap_or("");

    let response = match within {
        "toolbar.js" => content("text/javascript", Bytes::from_static(SCRIPT.as_bytes())),
        "toolbar.css" => content("text/css", Bytes::from(stylesheet())),
        "requests" => batch(parts.uri.query(), links, recorder),
        _ => record_path(within, recorder, renderer),
    };

    if parts.method == Method::HEAD {
        let (head, _) = response.into_parts();
        return Response::from_parts(head, empty());
    }

    response
}

/// `requests/ID.json` and `requests/ID/panels/NAME.html`.
fn record_path(within: &str, recorder: &Recorder, renderer: &Renderer) -> Response<ResponseBody> {
    let Some(rest) = within.strip_prefix("requests/") else {
        return plain(StatusCode::NOT_FOUND, "not found\n");
    };

    if let Some(request_id) = rest.strip_suffix(".json") {
        return match recorder.get(request_id) {
            Some(record) => match serde_json::to_vec(&record) {
                Ok(json) => content("application/json", Bytes::from(json)),
                Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "encoding failed\n"),
            },
            None => not_kept(request_id),
        };
    }

    let Some((request_id, panel)) = rest
        .split_once("/panels/")
        .and_then(|(id, panel)| Some((id, panel.strip_suffix(".html")?)))
    else {
        return plain(StatusCode::NOT_FOUND, "not found\n");
    };

    let Some(record) = recorder.get(request_id) else {
        return not_kept(request_id);
    };

    match record
        .panels
        .iter()
        .find(|candidate| candidate.name == panel)
    {
        Some(found) => content(
            "text/html; charset=utf-8",
            Bytes::from(renderer.render(found)),
        ),
        None => plain(StatusCode::NOT_FOUND, "the record has no such panel\n"),
    }
}

/// What the page view shows of one request (section 4.20): enough to
/// fill its row without the whole record.
#[derive(Debug, Serialize)]
struct Summary {
    request_id: String,
    in_progress: bool,
    method: String,
    target: String,
    status: u32,
    total_ms: f64,
    queue_ms: f64,
    app_ms: f64,
    /// No worker or upstream answered: a static file, a redirect or an
    /// error of the edge's own.
    served_by_edge: bool,
    group: String,
    worker: Option<u32>,
    /// The log records kept and those dropped past the limit.
    logs: u64,
    /// 1 when the request failed, as the badge counts it.
    errors: u32,
    connection: String,
    connection_position: u64,
    /// The request's page on the dashboard, when the dashboard is on.
    link: Option<String>,
}

impl Summary {
    fn of(record: &admin::RequestRecord, links: &Links) -> Self {
        let worker = record.from_worker.as_ref();
        let logs = worker.map_or(0, |worker| {
            worker.logs.len() as u64 + u64::from(worker.logs_dropped)
        });
        let failed = worker
            .is_some_and(|worker| !worker.outcome.is_empty() && worker.outcome != "completed")
            || record.status >= 500;

        Self {
            request_id: record.request_id.clone(),
            in_progress: record.in_progress,
            method: record.method.clone(),
            target: record.target.clone(),
            status: record.status,
            total_ms: record.total_ms,
            queue_ms: record.queue_ms,
            app_ms: record.app_ms,
            served_by_edge: record.status != 0
                && record.worker.is_none()
                && record.queue_ms == 0.0
                && record.app_ms == 0.0,
            group: record.group.clone(),
            worker: record.worker,
            logs,
            errors: u32::from(failed),
            connection: record
                .connection
                .as_ref()
                .map(|connection| connection.id.clone())
                .unwrap_or_default(),
            connection_position: record
                .connection
                .as_ref()
                .map_or(0, |connection| connection.position),
            link: links.request_page(&record.request_id),
        }
    }
}

/// `requests?ids=ID,ID`: the summaries of the records named, in the order
/// first asked, and the IDs no longer kept. Each ID is percent-decoded
/// after the list is split, so one may hold an encoded comma.
fn batch(query: Option<&str>, links: &Links, recorder: &Recorder) -> Response<ResponseBody> {
    let mut seen = HashSet::new();
    let request_ids: Vec<String> = query
        .unwrap_or("")
        .split('&')
        .filter_map(|pair| pair.strip_prefix("ids="))
        .flat_map(|list| list.split(','))
        .map(|id| percent_decode_str(id).decode_utf8_lossy().into_owned())
        .filter(|id| !id.is_empty() && seen.insert(id.clone()))
        .collect();

    if request_ids.is_empty() {
        return plain(
            StatusCode::BAD_REQUEST,
            "name the requests with ids=ID,ID\n",
        );
    }

    if request_ids.len() > MAX_BATCH {
        return plain(
            StatusCode::BAD_REQUEST,
            "at most 100 requests are answered at once\n",
        );
    }

    let mut found = recorder.read_each(&request_ids, |record| Summary::of(record, links));
    let mut requests = Vec::with_capacity(found.len());
    let mut not_kept = Vec::new();

    for request_id in request_ids {
        match found.remove(&request_id) {
            Some(summary) => requests.push(summary),
            None => not_kept.push(request_id),
        }
    }

    let body = serde_json::json!({ "requests": requests, "not_kept": not_kept });
    content("application/json", Bytes::from(body.to_string()))
}

fn not_kept(request_id: &str) -> Response<ResponseBody> {
    let body = serde_json::json!({
        "error": format!(
            "request {request_id} is no longer kept: the run keeps the records of its last {} requests",
            crate::recorder::CAPACITY
        )
    });
    let mut response = content("application/json", Bytes::from(body.to_string()));
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

/// Why a request to the reserved prefix is refused, if it is: a Host
/// that is neither loopback nor one of the run's names (DNS rebinding),
/// or a fetch from another site's page.
pub fn allowed(headers: &HeaderMap, hosts: &[String]) -> Result<(), &'static str> {
    let host = headers
        .get(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(crate::routing::host_only)
        .unwrap_or_default();

    let named = hosts.iter().any(|name| name.eq_ignore_ascii_case(&host));

    if !named && !loopback(&host) {
        return Err("the toolbar answers only to loopback and the run's own host names\n");
    }

    // Section 4.20: the toolbar's own requests are same-origin; a page
    // the address bar opened says none, and a client that is not a
    // browser sends nothing.

    let site = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok());

    match site {
        None | Some("same-origin" | "none") => Ok(()),
        Some(_) => Err("the toolbar answers only its own pages\n"),
    }
}

/// Whether a host name is a loopback name or address.
fn loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');

    host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Why a response gets no toolbar, when it does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// Not a GET for a document, or a response that is not an HTML page,
    /// or one whose bytes the toolbar must not change.
    NotAPage,
    /// The page's content security policy allows scripts only by hash or
    /// from elsewhere, so a tag would be refused.
    Policy,
}

/// Whether a response gets the toolbar, and the nonce its tag carries
/// when the page's policy gives scripts one.
pub fn insertable(
    method: &Method,
    request: &HeaderMap,
    response: &Response<ResponseBody>,
) -> Result<Option<String>, Skip> {
    // Only a browser loading a page: a fetch or htmx request says
    // otherwise, and a client that sends no Sec-Fetch-Dest is taken as
    // loading one.

    let document = request
        .get("sec-fetch-dest")
        .and_then(|value| value.to_str().ok())
        .is_none_or(|dest| dest.eq_ignore_ascii_case("document"));

    let status = response.status();
    let headers = response.headers();
    let html = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/html"))
        });
    let no_transform = headers.get_all(CACHE_CONTROL).iter().any(|value| {
        value.to_str().is_ok_and(|text| {
            text.split(',')
                .any(|directive| directive.trim().eq_ignore_ascii_case("no-transform"))
        })
    });

    if *method != Method::GET
        || !document
        || !html
        || matches!(
            status,
            StatusCode::NO_CONTENT | StatusCode::PARTIAL_CONTENT | StatusCode::NOT_MODIFIED
        )
        || status.is_informational()
        || headers.contains_key(CONTENT_ENCODING)
        || no_transform
        || response.extensions().get::<EdgeFile>().is_some()
    {
        return Err(Skip::NotAPage);
    }

    policy_nonce(headers)
}

/// Read the page's content security policies for what a script tag of
/// the same origin needs: nothing, a nonce, or no tag at all. Every
/// policy given must allow it: by a nonce, or by `'self'` (or a wildcard)
/// when `'strict-dynamic'` does not set `'self'` aside.
fn policy_nonce(headers: &HeaderMap) -> Result<Option<String>, Skip> {
    let mut nonce = None;

    for policy in headers
        .get_all("content-security-policy")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
    {
        let directives: Vec<(&str, Vec<&str>)> = policy
            .split(';')
            .filter_map(|directive| {
                let mut words = directive.split_ascii_whitespace();
                Some((words.next()?, words.collect()))
            })
            .collect();

        let sources = ["script-src-elem", "script-src", "default-src"]
            .iter()
            .find_map(|name| {
                directives
                    .iter()
                    .find(|(directive, _)| directive.eq_ignore_ascii_case(name))
            })
            .map(|(_, sources)| sources);

        let Some(sources) = sources else {
            continue;
        };

        let found = sources.iter().find_map(|source| {
            source
                .strip_prefix("'nonce-")
                .and_then(|rest| rest.strip_suffix('\''))
        });

        if let Some(found) = found {
            nonce.get_or_insert_with(|| found.to_owned());
            continue;
        }

        let dynamic = sources
            .iter()
            .any(|source| source.eq_ignore_ascii_case("'strict-dynamic'"));
        let own = sources.iter().any(|source| {
            source.eq_ignore_ascii_case("'self'")
                || *source == "*"
                || source.eq_ignore_ascii_case("http:")
                || source.eq_ignore_ascii_case("https:")
        });

        if dynamic || !own {
            return Err(Skip::Policy);
        }
    }

    Ok(nonce)
}

/// The tag inserted into a page: the script under the prefix a trusted
/// proxy forwarded, the request it shows, the dashboard's page for it,
/// and the page's nonce.
pub fn tag(prefix: &str, request_id: &str, dashboard: Option<&str>, nonce: Option<&str>) -> Bytes {
    let attribute = |text: &str| {
        text.replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
    };

    let mut tag = format!(
        "<script src=\"{}{PREFIX}toolbar.js\" data-request=\"{}\"",
        attribute(prefix.trim_end_matches('/')),
        attribute(request_id)
    );

    if let Some(dashboard) = dashboard {
        tag.push_str(&format!(" data-dashboard=\"{}\"", attribute(dashboard)));
    }

    if let Some(nonce) = nonce {
        tag.push_str(&format!(" nonce=\"{}\"", attribute(nonce)));
    }

    tag.push_str(" async></script>");
    Bytes::from(tag)
}

/// Insert `tag` before the page's first `</body>`, changing the headers
/// that no longer hold: the length goes, and a strong ETag is made weak.
pub fn insert(response: Response<ResponseBody>, tag: Bytes) -> Response<ResponseBody> {
    let (mut parts, body) = response.into_parts();
    parts.headers.remove(CONTENT_LENGTH);

    if let Some(etag) = parts
        .headers
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        && etag.starts_with('"')
        && let Ok(weak) = HeaderValue::from_str(&format!("W/{etag}"))
    {
        parts.headers.insert(ETAG, weak);
    }

    Response::from_parts(
        parts,
        UnsyncBoxBody::new(Inserting {
            inner: body,
            tag: Some(tag),
            held: BytesMut::new(),
            trailers: None,
            ended: false,
        }),
    )
}

/// A body that passes the page through with the tag inserted before its
/// first `</body>`. It holds back only the bytes that could begin the
/// tag across a chunk boundary, so a streamed page keeps streaming.
struct Inserting {
    inner: ResponseBody,
    tag: Option<Bytes>,
    held: BytesMut,
    trailers: Option<HeaderMap>,
    ended: bool,
}

impl Inserting {
    /// Take what can go out of the bytes held: all of them with the tag
    /// in place once `</body>` is found, otherwise all but a tail that
    /// could be its start.
    fn release(&mut self) -> Option<Bytes> {
        if self.tag.is_none() {
            return (!self.held.is_empty()).then(|| self.held.split().freeze());
        }

        if let Some(at) = find(&self.held, BODY_END) {
            let mut out = BytesMut::with_capacity(self.held.len() + 128);
            out.extend_from_slice(&self.held[..at]);
            out.extend_from_slice(&self.tag.take().unwrap_or_default());
            out.extend_from_slice(&self.held[at..]);
            self.held.clear();
            return Some(out.freeze());
        }

        let keep = (BODY_END.len() - 1).min(self.held.len());
        let ready = self.held.len() - keep;
        (ready > 0).then(|| self.held.split_to(ready).freeze())
    }
}

/// Where `needle` starts in `haystack`, ignoring ASCII case.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

impl Body for Inserting {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;

        loop {
            if this.ended {
                // Whatever was held back goes, then the trailers.

                if !this.held.is_empty() {
                    return Poll::Ready(Some(Ok(Frame::data(this.held.split().freeze()))));
                }

                return Poll::Ready(
                    this.trailers
                        .take()
                        .map(|trailers| Ok(Frame::trailers(trailers))),
                );
            }

            match std::task::ready!(Pin::new(&mut this.inner).poll_frame(context)) {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => {
                        this.held.extend_from_slice(&data);

                        if let Some(out) = this.release() {
                            return Poll::Ready(Some(Ok(Frame::data(out))));
                        }
                    }
                    Err(frame) => {
                        this.trailers = frame.into_trailers().ok();
                        this.ended = true;
                    }
                },
                Some(Err(error)) => return Poll::Ready(Some(Err(error))),
                None => this.ended = true,
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.ended && self.held.is_empty() && self.trailers.is_none()
    }
}

/// The paths a page's policy kept the toolbar from, each said once.
#[derive(Debug, Default)]
pub struct Warned {
    paths: Mutex<HashSet<String>>,
}

impl Warned {
    /// Whether this is the first time for `path`.
    pub fn first(&self, path: &str) -> bool {
        self.paths
            .lock()
            .expect("toolbar warnings poisoned")
            .insert(path.to_owned())
    }
}

fn empty() -> ResponseBody {
    http_body_util::Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn content(kind: &'static str, body: Bytes) -> Response<ResponseBody> {
    let mut response = Response::new(
        http_body_util::Full::new(body)
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(kind));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn plain(status: StatusCode, text: &'static str) -> Response<ResponseBody> {
    let mut response = content(
        "text/plain; charset=utf-8",
        Bytes::from_static(text.as_bytes()),
    );
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use http_body_util::StreamBody;

    use super::*;

    fn streamed(chunks: &[&'static str]) -> ResponseBody {
        let frames: Vec<Result<Frame<Bytes>, BoxError>> = chunks
            .iter()
            .map(|chunk| Ok(Frame::data(Bytes::from_static(chunk.as_bytes()))))
            .collect();
        StreamBody::new(futures::stream::iter(frames)).boxed_unsync()
    }

    fn page(chunks: &[&'static str]) -> Response<ResponseBody> {
        let mut response = Response::new(streamed(chunks));
        response.headers_mut().insert(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        response
    }

    async fn inserted(chunks: &[&'static str]) -> String {
        let response = insert(page(chunks), Bytes::from_static(b"<TAG>"));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn the_tag_goes_before_the_first_body_end_across_chunks() {
        assert_eq!(
            inserted(&["<html><body>hi</body></html>"]).await,
            "<html><body>hi<TAG></body></html>"
        );

        // Split inside the closing tag, in any case.

        assert_eq!(
            inserted(&["<body>hi</BO", "DY></html>"]).await,
            "<body>hi<TAG></BODY></html>"
        );
        assert_eq!(
            inserted(&["<body>a", "<", "/", "body>", "</body>"]).await,
            "<body>a<TAG></body></body>"
        );

        // A page without one goes out as it came.

        assert_eq!(inserted(&["<p>no end", "</bod"]).await, "<p>no end</bod");
    }

    #[test]
    fn the_headers_that_no_longer_hold_are_changed() {
        let mut response = page(&["x"]);
        response
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("1"));
        response
            .headers_mut()
            .insert(ETAG, HeaderValue::from_static("\"abc\""));
        let changed = insert(response, Bytes::from_static(b"<TAG>"));
        assert!(changed.headers().get(CONTENT_LENGTH).is_none());
        assert_eq!(changed.headers().get(ETAG).unwrap(), "W/\"abc\"");
    }

    #[test]
    fn only_a_page_a_browser_loads_gets_the_toolbar() {
        let document = |dest: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert("sec-fetch-dest", HeaderValue::from_static(dest));
            headers
        };

        assert_eq!(
            insertable(&Method::GET, &document("document"), &page(&["x"])),
            Ok(None)
        );
        assert_eq!(
            insertable(&Method::GET, &HeaderMap::new(), &page(&["x"])),
            Ok(None)
        );
        assert_eq!(
            insertable(&Method::GET, &document("empty"), &page(&["x"])),
            Err(Skip::NotAPage),
            "a fetch or an htmx request"
        );
        assert_eq!(
            insertable(&Method::HEAD, &document("document"), &page(&["x"])),
            Err(Skip::NotAPage)
        );

        // Each response the toolbar must leave alone.

        let changed = |change: fn(&mut Response<ResponseBody>)| {
            let mut response = page(&["x"]);
            change(&mut response);
            insertable(&Method::GET, &document("document"), &response)
        };

        assert_eq!(
            changed(|response| *response.status_mut() = StatusCode::NOT_MODIFIED),
            Err(Skip::NotAPage)
        );
        assert_eq!(
            changed(|response| *response.status_mut() = StatusCode::PARTIAL_CONTENT),
            Err(Skip::NotAPage)
        );
        assert_eq!(
            changed(|response| {
                response
                    .headers_mut()
                    .insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            }),
            Err(Skip::NotAPage)
        );
        assert_eq!(
            changed(|response| {
                response.headers_mut().insert(
                    CACHE_CONTROL,
                    HeaderValue::from_static("public, no-transform"),
                );
            }),
            Err(Skip::NotAPage)
        );
        assert_eq!(
            changed(|response| {
                response
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            }),
            Err(Skip::NotAPage)
        );
        assert_eq!(
            changed(|response| {
                response.extensions_mut().insert(EdgeFile);
            }),
            Err(Skip::NotAPage)
        );
    }

    #[test]
    fn the_pages_policy_decides_the_tag() {
        let policy = |text: &'static str| {
            let mut response = page(&["x"]);
            response
                .headers_mut()
                .insert("content-security-policy", HeaderValue::from_static(text));
            insertable(&Method::GET, &HeaderMap::new(), &response)
        };

        assert_eq!(policy("default-src 'self'"), Ok(None));
        assert_eq!(policy("img-src *"), Ok(None), "no script directive");
        assert_eq!(
            policy("default-src 'self'; script-src 'nonce-r4nd0m' 'strict-dynamic'"),
            Ok(Some("r4nd0m".to_owned()))
        );
        assert_eq!(
            policy("script-src 'sha256-abc='"),
            Err(Skip::Policy),
            "hashes only"
        );
        assert_eq!(
            policy("script-src 'self' 'strict-dynamic' 'sha256-abc='"),
            Err(Skip::Policy)
        );
        assert_eq!(
            policy("default-src https://cdn.example.com"),
            Err(Skip::Policy)
        );
    }

    #[tokio::test]
    async fn the_batch_path_summarises_each_record_named_in_one_answer() {
        let recorder = Recorder::default();

        let slot = recorder.open(admin::RequestRecord {
            request_id: "A".to_owned(),
            ..Default::default()
        });
        recorder.complete(&slot, &crate::recorder::tests::logged("A"));
        recorder.update(&slot, |record| {
            crate::recorder::worker(
                record,
                muru_proto::worker::v1::WorkerRecord {
                    outcome: "failed".to_owned(),
                    logs: vec![Default::default(); 2],
                    logs_dropped: 1,
                    ..Default::default()
                },
            );
        });

        // A static file the edge answered itself, and an ID holding an
        // encoded comma.

        let slot = recorder.open(admin::RequestRecord {
            request_id: "B,1".to_owned(),
            ..Default::default()
        });
        let mut file = crate::recorder::tests::logged("B,1");
        file.worker = None;
        file.queue = std::time::Duration::ZERO;
        file.application = std::time::Duration::ZERO;
        recorder.complete(&slot, &file);

        let links = Links::default();
        links.set_dashboard("http://127.0.0.1:8081/");
        let ask = |target: &str| {
            let (parts, ()) = http::Request::get(target)
                .header(http::header::HOST, "127.0.0.1:8080")
                .body(())
                .unwrap()
                .into_parts();
            answer(&parts, &links, &recorder, &Renderer::new(None))
        };

        let response = ask("/_muru/requests?ids=B%2C1,NOSUCH,A,B%2C1");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let found: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let requests = found["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 2, "{found}");
        assert_eq!(found["not_kept"], serde_json::json!(["NOSUCH"]));

        let file = &requests[0];
        assert_eq!(file["request_id"], "B,1");
        assert_eq!(file["served_by_edge"], true);
        assert_eq!(file["worker"], serde_json::Value::Null);

        let failed = &requests[1];
        assert_eq!(failed["request_id"], "A");
        assert_eq!(failed["status"], 200);
        assert_eq!(failed["served_by_edge"], false);
        assert_eq!((&failed["logs"], &failed["errors"]), (&3.into(), &1.into()));
        assert_eq!(failed["link"], "http://127.0.0.1:8081/request/A");

        // None named, or too many, is refused.

        assert_eq!(ask("/_muru/requests").status(), StatusCode::BAD_REQUEST);
        let many = vec!["X"; MAX_BATCH + 1].join(",");
        assert_eq!(
            ask(&format!("/_muru/requests?ids={many}")).status(),
            StatusCode::OK,
            "the same ID many times is one"
        );
        let many: Vec<String> = (0..=MAX_BATCH).map(|n| n.to_string()).collect();
        assert_eq!(
            ask(&format!("/_muru/requests?ids={}", many.join(","))).status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn the_tag_names_the_prefix_the_request_and_the_nonce() {
        assert_eq!(
            tag(
                "/proxy/8080/",
                "A1",
                Some("http://127.0.0.1:8081/request/A1"),
                Some("n\"1")
            ),
            "<script src=\"/proxy/8080/_muru/toolbar.js\" data-request=\"A1\" \
             data-dashboard=\"http://127.0.0.1:8081/request/A1\" nonce=\"n&quot;1\" async></script>"
        );
        assert_eq!(
            tag("", "A1", None, None),
            "<script src=\"/_muru/toolbar.js\" data-request=\"A1\" async></script>"
        );
    }

    #[test]
    fn only_a_loopback_or_named_host_and_its_own_pages_are_answered() {
        let headers = |host: &'static str, site: Option<&'static str>| {
            let mut headers = HeaderMap::new();
            headers.insert(http::header::HOST, HeaderValue::from_static(host));

            if let Some(site) = site {
                headers.insert("sec-fetch-site", HeaderValue::from_static(site));
            }

            headers
        };
        let hosts = vec!["shop.test".to_owned()];

        assert!(allowed(&headers("127.0.0.1:8080", Some("same-origin")), &hosts).is_ok());
        assert!(allowed(&headers("[::1]:8080", None), &hosts).is_ok());
        assert!(allowed(&headers("localhost:8080", Some("none")), &hosts).is_ok());
        assert!(allowed(&headers("app.localhost", None), &hosts).is_ok());
        assert!(allowed(&headers("shop.test:8443", None), &hosts).is_ok());
        assert!(
            allowed(&headers("attacker.example", None), &hosts).is_err(),
            "rebinding"
        );
        assert!(allowed(&headers("127.0.0.1", Some("cross-site")), &hosts).is_err());
        assert!(allowed(&headers("127.0.0.1", Some("same-site")), &hosts).is_err());
    }
}
