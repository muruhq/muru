//! What a development run tells the browser about each request (design
//! specification section 4.20): `Server-Timing` with the queue and
//! application times and the request ID, and `Muru-Request-Link` naming
//! the request's page on the dashboard. Neither is sent outside
//! development mode. Also the connection a request arrived on, which
//! the access log names in every mode, and the joining of the two
//! halves of a request's record (section 11.7).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use muru_proto::admin::v1 as admin;
use muru_proto::worker::v1::WorkerRecord;

use crate::access::Record;
use crate::recorder::{Recorder, Slot};

use http::header::{HeaderName, HeaderValue};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// The header carrying the queue and application times and the request
/// ID.
pub const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");

/// The header naming the request's page on the dashboard.
pub const REQUEST_LINK: HeaderName = HeaderName::from_static("muru-request-link");

/// The characters a request ID keeps in a path: the unreserved ones.
const PATH: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The times a worker or an upstream took over a request, noted when
/// its response head arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// The wait for a worker connection.
    pub queue: Duration,
    /// The time from dispatch until the response head arrived.
    pub application: Duration,
}

/// One connection a listener accepted: its ID, given at accept from
/// the request ID generator, and what its requests' records say of it.
#[derive(Debug)]
pub struct Connection {
    /// The connection's ID.
    pub id: String,
    /// How long the TLS handshake took, from accept, on a TLS connection.
    pub handshake: Option<Duration>,
    /// Requests that have arrived on it.
    arrived: AtomicU64,
    /// Requests on it whose records are still open, for the idle time.
    active: AtomicUsize,
    /// When the connection was last idle from: the end of its handshake,
    /// or of its last request.
    idle_since: Mutex<Instant>,
}

/// A request's place on its connection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Arrival {
    /// 1 for the connection's first request.
    pub position: u64,
    /// How long the connection was idle before the request arrived:
    /// zero while another of its requests was still open.
    pub idle: Duration,
}

impl Connection {
    /// A connection accepted at `accepted`, ready for its first request
    /// now, after its handshake on a TLS connection.
    pub fn new(id: String, accepted: Instant, tls: bool) -> Self {
        let now = Instant::now();

        Self {
            id,
            handshake: tls.then(|| now.saturating_duration_since(accepted)),
            arrived: AtomicU64::new(0),
            active: AtomicUsize::new(0),
            idle_since: Mutex::new(now),
        }
    }

    /// A request arrived: its position, and, when `tracked` (a
    /// development run, whose record ends it), how long the connection
    /// was idle before it.
    pub fn arrive(&self, tracked: bool) -> Arrival {
        let position = self.arrived.fetch_add(1, Ordering::Relaxed) + 1;

        let idle = if tracked && self.active.fetch_add(1, Ordering::AcqRel) == 0 {
            self.idle_since
                .lock()
                .expect("connection idle time poisoned")
                .elapsed()
        } else {
            Duration::ZERO
        };

        Arrival { position, idle }
    }

    /// A tracked request's record is done with: the connection is idle
    /// from now once no other is open.
    fn done(&self) {
        if self.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            *self
                .idle_since
                .lock()
                .expect("connection idle time poisoned") = Instant::now();
        }
    }
}

/// What a development run learns of one client request as it is
/// handled, shared by every hop of it (section 4.10) so that the final
/// response can carry it, and the request's record while its two halves
/// come together.
#[derive(Debug, Default)]
pub struct Request {
    timing: Mutex<Option<Timing>>,
    joined: Mutex<Joined>,
    recorder: Option<Arc<Recorder>>,
    /// Told when the request is done with, for its idle time.
    connection: Option<Arc<Connection>>,
}

/// How far a record has come together: its head, made as the response's
/// head goes out, the access log's record when the response ends, and
/// the worker's part after it, which may come in any order.
#[derive(Debug, Default)]
struct Joined {
    /// The record in the ring, once its head is in.
    slot: Option<Slot>,
    /// The access log's record, when it came before the head.
    logged: Option<Box<Record>>,
    /// The worker's part, when it came before the head.
    worker: Option<WorkerRecord>,
    /// The worker was asked for its part.
    expected: bool,
    /// The worker's part will not come.
    missing: bool,
}

impl Drop for Request {
    fn drop(&mut self) {
        if let Some(connection) = &self.connection {
            connection.done();
        }
    }
}

impl Request {
    /// A request whose record goes to `recorder`, if the run keeps them,
    /// on a connection that is told when it is done with.
    pub fn new(recorder: Option<Arc<Recorder>>, connection: Arc<Connection>) -> Self {
        Self {
            timing: Mutex::default(),
            joined: Mutex::default(),
            recorder,
            connection: Some(connection),
        }
    }

    /// Whether the run keeps the request's record.
    pub fn recorded(&self) -> bool {
        self.recorder.is_some()
    }

    /// The access log's record of the request, written when its response
    /// ended: it completes the record if the head is in, or waits for it.
    /// A later hop's replaces an earlier one's.
    pub fn logged(&self, record: &Record) {
        let Some(recorder) = &self.recorder else {
            return;
        };

        let mut joined = self.joined.lock().expect("request record poisoned");

        match &joined.slot {
            Some(slot) => recorder.complete(slot, record),
            None => joined.logged = Some(Box::new(record.clone())),
        }
    }

    /// The head of the request's record, made as the response's head
    /// goes out: it is kept in the ring with whatever else of the record
    /// came first, and in progress until the rest is in.
    pub fn opened(&self, mut head: admin::RequestRecord) {
        let Some(recorder) = &self.recorder else {
            return;
        };

        let mut joined = self.joined.lock().expect("request record poisoned");

        if joined.slot.is_some() {
            return;
        }

        head.in_progress = true;

        if joined.expected {
            head.worker_part = crate::recorder::WORKER_EXPECTED.to_owned();
        }

        if let Some(part) = joined.worker.take() {
            crate::recorder::worker(&mut head, part);
        } else if joined.missing {
            crate::recorder::worker_missing(&mut head);
        }

        if let Some(record) = joined.logged.take() {
            crate::recorder::fill(&mut head, &record);
        }

        joined.slot = Some(recorder.open(head));
    }

    /// The worker was asked for its part of the record, which follows the
    /// response.
    pub fn expect_worker(&self) {
        let mut joined = self.joined.lock().expect("request record poisoned");
        joined.expected = true;
        joined.missing = false;

        if let (Some(recorder), Some(slot)) = (&self.recorder, &joined.slot) {
            recorder.update(slot, |record| {
                record.worker_part = crate::recorder::WORKER_EXPECTED.to_owned();
                record.in_progress = true;
            });
        }
    }

    /// The worker's part of the record arrived.
    pub fn worker_part(&self, part: WorkerRecord) {
        let Some(recorder) = &self.recorder else {
            return;
        };

        let mut joined = self.joined.lock().expect("request record poisoned");

        match &joined.slot {
            Some(slot) => recorder.update(slot, |record| crate::recorder::worker(record, part)),
            None => joined.worker = Some(part),
        }
    }

    /// The worker's part of the record will not come.
    pub fn worker_missing(&self) {
        let Some(recorder) = &self.recorder else {
            return;
        };

        let mut joined = self.joined.lock().expect("request record poisoned");
        joined.missing = true;

        if let Some(slot) = &joined.slot {
            recorder.update(slot, crate::recorder::worker_missing);
        }
    }

    /// Note the times of the hop whose response head just arrived; a
    /// later hop's replace an earlier one's.
    pub fn note(&self, timing: Timing) {
        *self.timing.lock().expect("request timing poisoned") = Some(timing);
    }

    /// The times of the last hop a worker or an upstream answered, if
    /// any did.
    pub fn timing(&self) -> Option<Timing> {
        *self.timing.lock().expect("request timing poisoned")
    }
}

/// What every response of a development run is given: where the
/// dashboard's request pages are, once the dashboard has started.
#[derive(Debug, Default)]
pub struct Links {
    dashboard: OnceLock<String>,
}

impl Links {
    /// Name the dashboard's address, `http://HOST:PORT/` or a URL it is
    /// reached at, under which `request/ID` is a request's page. The
    /// first call wins.
    pub fn set_dashboard(&self, address: &str) {
        let base = if address.ends_with('/') {
            address.to_owned()
        } else {
            format!("{address}/")
        };

        let _ = self.dashboard.set(base);
    }

    /// The dashboard's page for a request, when the dashboard is on.
    pub fn request_page(&self, request_id: &str) -> Option<String> {
        let base = self.dashboard.get()?;
        Some(format!(
            "{base}request/{}",
            utf8_percent_encode(request_id, PATH)
        ))
    }
}

/// The value of `Server-Timing` for a response: `queue` and `app` in
/// milliseconds when a worker or an upstream answered, and the request
/// ID as `muru`'s description in every case. There is no total, since
/// the head goes out before the body is complete.
pub fn server_timing(request_id: &str, timing: Option<Timing>) -> String {
    let mut value = String::new();

    if let Some(timing) = timing {
        value.push_str(&format!(
            "queue;dur={}, app;dur={}, ",
            millis(timing.queue),
            millis(timing.application)
        ));
    }

    value.push_str("muru;desc=\"");

    // A quoted string (RFC 9110): a quote or a backslash is escaped.

    for character in request_id.chars() {
        if character == '"' || character == '\\' {
            value.push('\\');
        }

        value.push(character);
    }

    value.push('"');
    value
}

/// Add `Server-Timing` and, when the dashboard is on, `Muru-Request-Link`
/// to a response's headers. An application's own `Server-Timing` is
/// kept beside Muru's.
pub fn annotate(
    headers: &mut http::HeaderMap,
    request_id: &str,
    request: Option<&Arc<Request>>,
    links: &Links,
) {
    let timing = request.and_then(|request| request.timing());

    if let Ok(value) = HeaderValue::from_str(&server_timing(request_id, timing)) {
        headers.append(SERVER_TIMING, value);
    }

    if let Some(page) = links.request_page(request_id)
        && let Ok(value) = HeaderValue::from_str(&page)
    {
        headers.insert(REQUEST_LINK, value);
    }
}

/// Milliseconds with two decimals, as `Server-Timing` durations are
/// written.
fn millis(duration: Duration) -> String {
    format!("{:.2}", duration.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(recorder: &Arc<Recorder>) -> Request {
        let connection = Arc::new(Connection::new("C1".to_owned(), Instant::now(), false));
        Request::new(Some(Arc::clone(recorder)), connection)
    }

    fn head(request_id: &str) -> admin::RequestRecord {
        admin::RequestRecord {
            request_id: request_id.to_owned(),
            ..Default::default()
        }
    }

    fn part() -> WorkerRecord {
        WorkerRecord {
            thread: "muru-wsgi-0".to_owned(),
            outcome: "completed".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn the_head_the_access_record_and_the_workers_part_join_in_any_order() {
        let recorder = Arc::new(Recorder::default());

        // In the usual order: asked, head, access record, worker's part.
        // The record is in progress until the last of them.

        let usual = request(&recorder);
        usual.expect_worker();
        usual.opened(head("A"));
        usual.logged(&crate::recorder::tests::logged("A-2"));
        let record = recorder.get("A").unwrap();
        assert!(record.in_progress);
        assert_eq!(record.worker_part, crate::recorder::WORKER_EXPECTED);
        assert_eq!(record.status, 200);
        usual.worker_part(part());
        let record = recorder.get("A").unwrap();
        assert!(!record.in_progress);
        assert_eq!(record.worker_part, crate::recorder::WORKER_ARRIVED);
        assert_eq!(record.from_worker.unwrap().thread, "muru-wsgi-0");

        // The access record and the worker's part before the head.

        let early = request(&recorder);
        early.expect_worker();
        early.logged(&crate::recorder::tests::logged("B"));
        early.worker_part(part());
        early.opened(head("B"));
        let record = recorder.get("B").unwrap();
        assert!(!record.in_progress);
        assert_eq!(record.request_id, "B");
        assert!(record.from_worker.is_some());

        // A worker's part that never comes leaves the record complete,
        // and says so.

        let lost = request(&recorder);
        lost.expect_worker();
        lost.opened(head("C"));
        lost.logged(&crate::recorder::tests::logged("C"));
        lost.worker_missing();
        let record = recorder.get("C").unwrap();
        assert!(!record.in_progress);
        assert_eq!(record.worker_part, crate::recorder::WORKER_MISSING);

        // Nothing asked of the worker, nothing waited for.

        let plain = request(&recorder);
        plain.opened(head("D"));
        plain.logged(&crate::recorder::tests::logged("D"));
        let record = recorder.get("D").unwrap();
        assert!(!record.in_progress);
        assert!(record.worker_part.is_empty());
    }

    #[test]
    fn server_timing_names_the_queue_the_application_and_the_request() {
        let timing = Timing {
            queue: Duration::from_micros(210),
            application: Duration::from_micros(17_930),
        };

        assert_eq!(
            server_timing("01JCZ8X4R3T5V6W7Y8Z9A0B1C2", Some(timing)),
            "queue;dur=0.21, app;dur=17.93, muru;desc=\"01JCZ8X4R3T5V6W7Y8Z9A0B1C2\""
        );

        // A response the edge produced itself names only the request,
        // and a description is a quoted string.

        assert_eq!(server_timing("A1", None), "muru;desc=\"A1\"");
        assert_eq!(server_timing("a\"b\\c", None), "muru;desc=\"a\\\"b\\\\c\"");
    }

    #[test]
    fn the_link_is_the_dashboards_page_once_it_is_named() {
        let links = Links::default();
        assert_eq!(links.request_page("A1"), None);

        links.set_dashboard("https://example.com/proxy/8081");
        links.set_dashboard("http://127.0.0.1:8081/");
        assert_eq!(
            links.request_page("A1").as_deref(),
            Some("https://example.com/proxy/8081/request/A1")
        );
        assert_eq!(
            links.request_page("a b/c").as_deref(),
            Some("https://example.com/proxy/8081/request/a%20b%2Fc")
        );
    }

    #[test]
    fn the_last_hop_answered_gives_the_times_and_an_application_header_stays() {
        let request = Arc::new(Request::default());
        let first = Timing {
            queue: Duration::from_millis(1),
            application: Duration::from_millis(2),
        };
        let last = Timing {
            queue: Duration::from_millis(3),
            application: Duration::from_millis(4),
        };
        request.note(first);
        request.note(last);

        let mut headers = http::HeaderMap::new();
        headers.insert(SERVER_TIMING, HeaderValue::from_static("db;dur=1"));
        let links = Links::default();
        links.set_dashboard("http://127.0.0.1:8081");
        annotate(&mut headers, "A1", Some(&request), &links);

        let timings: Vec<_> = headers.get_all(SERVER_TIMING).iter().collect();
        assert_eq!(timings.len(), 2);
        assert_eq!(timings[0], "db;dur=1");
        assert_eq!(timings[1], "queue;dur=3.00, app;dur=4.00, muru;desc=\"A1\"");
        assert_eq!(
            headers.get(REQUEST_LINK).unwrap(),
            "http://127.0.0.1:8081/request/A1"
        );
    }
}
