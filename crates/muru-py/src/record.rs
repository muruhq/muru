//! The worker's part of a development run's request record (design
//! specification section 11.7): what the worker saw of one request, kept
//! only when the request head asked for a record, and sent to the edge on
//! a RECORD frame after the response.
//!
//! The times are taken where the worker already handles each piece of the
//! response: start_response or the head, the body as the link's writer
//! takes it, and the application's end. The log records are kept by the
//! runtime library's record factory, by the request's id.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use muru_proto::worker::v1 as pb;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;

use crate::wsgi::Outgoing;

/// The runtime library's functions that keep a request's log records.
static CAPTURE: PyOnceLock<(Py<PyAny>, Py<PyAny>)> = PyOnceLock::new();

/// What the worker saw of one request, for its record.
#[derive(Debug)]
pub struct Trace {
    /// When the worker took the request.
    started: Instant,
    /// The same moment on the wall clock, for the log records' times.
    started_wall: SystemTime,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The body's source when nothing more particular is known:
    /// `iterable` for WSGI, `asgi_body` for ASGI.
    default_source: &'static str,
    thread: String,
    start_response: Option<Duration>,
    first_byte: Option<Duration>,
    last_byte: Option<Duration>,
    close: Option<Duration>,
    body_bytes: u64,
    body_source: Option<&'static str>,
    sent_by_edge: bool,
    error: Option<Failure>,
}

#[derive(Debug)]
struct Failure {
    error: String,
    reason: String,
    traceback: String,
    phase: &'static str,
}

impl Trace {
    /// A trace of a request the worker took at `started`, whose body comes
    /// from `default_source` unless something more particular is seen.
    pub fn new(started: Instant, default_source: &'static str) -> Self {
        let elapsed = started.elapsed();

        Self {
            started,
            started_wall: SystemTime::now() - elapsed,
            state: Mutex::new(State {
                default_source,
                ..State::default()
            }),
        }
    }

    /// The request runs on the event loop, as ASGI: its body is the
    /// application's `http.response.body` messages.
    pub fn on_loop(&self) {
        let mut state = self.state();
        state.default_source = "asgi_body";
        state.thread = "event loop".to_owned();
    }

    fn at(&self) -> Duration {
        self.started.elapsed()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("request trace poisoned")
    }

    /// The thread that runs the application, by its name.
    pub fn thread(&self, name: &str) {
        self.state().thread = name.to_owned();
    }

    /// The application called start_response; the first call counts.
    pub fn start_response(&self) {
        let at = self.at();
        self.state().start_response.get_or_insert(at);
    }

    /// The body comes from this source, unless one was named before.
    pub fn body_source(&self, source: &'static str) {
        self.state().body_source.get_or_insert(source);
    }

    /// The link's writer took an item of the response from the
    /// application: note the head, the body's bytes, a failure and the end.
    pub fn observe(&self, item: Option<&Outgoing>) {
        let at = self.at();
        let mut state = self.state();

        match item {
            Some(Outgoing::Head { .. }) => {
                state.start_response.get_or_insert(at);
            }
            Some(Outgoing::Body(data)) if !data.is_empty() => {
                state.first_byte.get_or_insert(at);
                state.last_byte = Some(at);
                state.body_bytes += data.len() as u64;
            }
            Some(Outgoing::File { length, .. }) => {
                state.first_byte.get_or_insert(at);
                state.last_byte = Some(at);
                state.body_bytes += length;
                state.sent_by_edge = true;

                let source = if state.default_source == "asgi_body" {
                    "pathsend"
                } else {
                    "file_wrapper"
                };
                state.body_source.get_or_insert(source);
            }
            Some(Outgoing::Abort {
                error,
                reason,
                detail,
            }) => {
                let phase = if state.first_byte.is_some() {
                    "iteration"
                } else {
                    "call"
                };

                state.error.get_or_insert(Failure {
                    error: error.clone(),
                    reason: reason.clone(),
                    traceback: detail.clone(),
                    phase,
                });
            }
            Some(Outgoing::Finished) | None => {
                state.close.get_or_insert(at);
            }
            Some(_) => {}
        }
    }

    /// The worker's part of the record, with the request's log records:
    /// `cancelled` when the edge gave the request up before it ended.
    pub fn to_message(&self, cancelled: bool, logs: Logs) -> pb::WorkerRecord {
        let at = self.at();
        let mut state = self.state();

        // A response that failed ends the writer before the application's
        // end reaches it; the record is made once the application is done.

        state.close.get_or_insert(at);
        let millis = |at: Option<Duration>| at.map_or(0.0, |at| at.as_secs_f64() * 1000.0);

        let (outcome, reason) = match (&state.error, cancelled) {
            (_, true) => ("client_disconnected", "client_disconnected".to_owned()),
            (Some(failure), _) if failure.reason == "soft_timeout" => {
                ("timeout", failure.reason.clone())
            }
            (Some(failure), _) => ("failed", failure.reason.clone()),
            (None, false) => ("completed", String::new()),
        };

        let started_wall = self.started_wall;
        let lines = logs
            .kept
            .into_iter()
            .map(|(created, level, logger, message)| {
                let created = UNIX_EPOCH + Duration::from_secs_f64(created.max(0.0));
                let at = created
                    .duration_since(started_wall)
                    .map_or(0.0, |since| since.as_secs_f64() * 1000.0);

                pb::LogLine {
                    at,
                    level,
                    logger,
                    message,
                }
            })
            .collect();

        pb::WorkerRecord {
            thread: state.thread.clone(),
            to_start_response: millis(state.start_response),
            to_first_byte: millis(state.first_byte),
            to_last_byte: millis(state.last_byte),
            to_close: millis(state.close),
            body_bytes: state.body_bytes,
            body_source: if state.body_bytes == 0 && state.body_source.is_none() {
                String::new()
            } else {
                state.body_source.unwrap_or(state.default_source).to_owned()
            },
            sent_by_edge: state.sent_by_edge,
            outcome: outcome.to_owned(),
            reason,
            error_phase: state
                .error
                .as_ref()
                .map(|failure| failure.phase.to_owned())
                .unwrap_or_default(),
            error: state
                .error
                .as_ref()
                .map(|failure| failure.error.clone())
                .unwrap_or_default(),
            traceback: state
                .error
                .as_ref()
                .map(|failure| failure.traceback.clone())
                .unwrap_or_default(),
            logs: lines,
            logs_dropped: logs.dropped,
        }
    }
}

/// A request's log records as the runtime library kept them: each one's
/// time on the wall clock, level, logger and message, and how many were
/// past the limit.
#[derive(Debug, Default)]
pub struct Logs {
    kept: Vec<(f64, String, String, String)>,
    dropped: u32,
}

fn capture(py: Python<'_>) -> PyResult<&(Py<PyAny>, Py<PyAny>)> {
    CAPTURE.get_or_try_init(py, || {
        let module = py.import("muru.logging")?;
        Ok((
            module.getattr("_capture_start")?.unbind(),
            module.getattr("_capture_take")?.unbind(),
        ))
    })
}

/// Start keeping a request's log records.
pub fn start_logs(request_id: &str) {
    Python::attach(|py| {
        if let Err(error) = capture(py).and_then(|(start, _)| start.call1(py, (request_id,))) {
            tracing::debug!(%error, "the request's log records cannot be kept");
        }
    });
}

/// Stop keeping a request's log records, and take them.
pub fn take_logs(request_id: &str) -> Logs {
    Python::attach(|py| {
        capture(py)
            .and_then(|(_, take)| take.call1(py, (request_id,)))
            .and_then(|taken| taken.extract::<(Vec<(f64, String, String, String)>, u32)>(py))
            .map(|(kept, dropped)| Logs { kept, dropped })
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    #[test]
    fn the_record_follows_the_response_as_the_writer_takes_it() {
        let trace = Trace::new(Instant::now(), "iterable");
        trace.thread("muru-wsgi-0");
        trace.start_response();
        trace.observe(Some(&Outgoing::Head {
            status: 200,
            reason: String::new(),
            headers: Vec::new(),
            flush: pb::FlushPolicy::Coalesce,
        }));
        trace.observe(Some(&Outgoing::Body(Bytes::from_static(b"hello "))));
        trace.observe(Some(&Outgoing::Body(Bytes::new())));
        trace.observe(Some(&Outgoing::Body(Bytes::from_static(b"world"))));
        trace.observe(None);

        let record = trace.to_message(false, Logs::default());
        assert_eq!(record.thread, "muru-wsgi-0");
        assert_eq!(record.body_bytes, 11);
        assert_eq!(record.body_source, "iterable");
        assert_eq!(record.outcome, "completed");
        assert!(record.to_start_response <= record.to_first_byte);
        assert!(record.to_first_byte <= record.to_last_byte);
        assert!(record.to_last_byte <= record.to_close);
        assert!(record.error_phase.is_empty());
    }

    #[test]
    fn a_failure_names_its_phase_and_a_gone_client_wins() {
        let failed = Trace::new(Instant::now(), "asgi_body");
        failed.observe(Some(&Outgoing::Abort {
            error: "the application raised an exception".to_owned(),
            reason: "app_error".to_owned(),
            detail: "Traceback ...".to_owned(),
        }));
        let record = failed.to_message(false, Logs::default());
        assert_eq!(
            (record.outcome.as_str(), record.error_phase.as_str()),
            ("failed", "call")
        );
        assert_eq!(record.traceback, "Traceback ...");
        assert_eq!(record.body_source, "");

        let timed_out = Trace::new(Instant::now(), "iterable");
        timed_out.observe(Some(&Outgoing::Body(Bytes::from_static(b"x"))));
        timed_out.observe(Some(&Outgoing::Abort {
            error: String::new(),
            reason: "soft_timeout".to_owned(),
            detail: String::new(),
        }));
        let record = timed_out.to_message(false, Logs::default());
        assert_eq!(
            (record.outcome.as_str(), record.error_phase.as_str()),
            ("timeout", "iteration")
        );

        let gone = Trace::new(Instant::now(), "iterable");
        assert_eq!(
            gone.to_message(true, Logs::default()).outcome,
            "client_disconnected"
        );
    }

    #[test]
    fn a_file_the_edge_sends_is_named_by_the_interface() {
        let (read, _write) = std::os::unix::net::UnixStream::pair().unwrap();
        let trace = Trace::new(Instant::now(), "iterable");
        trace.observe(Some(&Outgoing::File {
            fd: read.into(),
            offset: 0,
            length: 4096,
            more: false,
        }));
        let record = trace.to_message(false, Logs::default());
        assert_eq!(record.body_source, "file_wrapper");
        assert!(record.sent_by_edge);
        assert_eq!(record.body_bytes, 4096);
    }

    #[test]
    fn log_records_are_placed_on_the_requests_clock() {
        let trace = Trace::new(Instant::now(), "iterable");
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            + 0.05;
        let logs = Logs {
            kept: vec![(
                created,
                "WARNING".to_owned(),
                "shop".to_owned(),
                "low stock".to_owned(),
            )],
            dropped: 3,
        };
        let record = trace.to_message(false, logs);
        assert_eq!(record.logs.len(), 1);
        assert!(
            (40.0..1000.0).contains(&record.logs[0].at),
            "{}",
            record.logs[0].at
        );
        assert_eq!(record.logs_dropped, 3);
    }
}
