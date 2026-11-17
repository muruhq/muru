//! A development run's record of each request (design specification
//! section 11.7): a ring in the run's process, shared by the edge, the
//! dashboard and the admin socket, bounded by count and by bytes, held
//! in memory only and gone with the run.
//!
//! The edge's part of a record comes in two halves that may arrive in
//! either order: the head, when the response's head goes out with its
//! final headers, and the access log's record, when the response ends.
//! [`crate::development::Request`] joins them. Bodies are never kept,
//! and header values that may hold secrets are masked as they are taken.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use muru_proto::admin::v1 as admin;
use prost::Message as _;

use crate::access::Record;

/// The most records kept.
pub const CAPACITY: usize = 1000;

/// The most bytes of records kept, measured as their encoded size.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

/// What a masked value reads.
pub const MASKED: &str = "***";

/// The most captures one record keeps (issue 706); later ones are left
/// out, since the first say what failed.
pub const MAX_CAPTURES: usize = 8;

/// The most other threads a capture keeps on each record it reaches.
pub const MAX_CAPTURE_THREADS: usize = 32;

/// Issue 715: the most captures kept for requests whose records have not
/// opened yet, such as those still in flight on a worker killed while
/// stopped, and how long each waits for its record.
const MAX_WAITING: usize = 64;
const WAITING_FOR: Duration = Duration::from_secs(120);

/// Headers whose values are masked whatever they hold.
const ALWAYS_MASKED: &[&str] = &[
    "cookie",
    "set-cookie",
    "authorization",
    "proxy-authorization",
];

/// Words in a header's name that make its value a secret, as
/// `muructl debug inspect` judges an environment variable's.
const SECRET_WORDS: &[&str] = &[
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "KEY",
    "CREDENTIAL",
    "AUTH",
];

/// Query parameters whose values are secrets, by normalised name (see
/// [`secret_parameter`]), aligned with what wrapture masks in the query
/// it records for the same request.
const SECRET_PARAMETERS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "apikey",
    "key",
    "clientsecret",
    "auth",
    "jwt",
    "code",
    "oauthtoken",
    "privatetoken",
    "session",
    "sessionid",
    "sessid",
    "jsessionid",
    "phpsessid",
    "sig",
    "signature",
    "xamzsignature",
    "xamzsecuritytoken",
    "xgoogsignature",
];

/// Endings of a normalised parameter name that make its value a secret:
/// `csrf_token`, `github_token`, `new_password`.
const SECRET_PARAMETER_ENDINGS: &[&str] = &["token", "secret", "password", "passwd"];

/// Headers whose values are URLs, whose query is masked as the target's.
const URL_HEADERS: &[&str] = &["referer", "location", "content-location"];

/// One record in the ring, shared with the request that is filling it.
pub type Slot = Arc<Mutex<admin::RequestRecord>>;

struct Kept {
    slot: Slot,
    bytes: usize,
}

struct Ring {
    kept: VecDeque<Kept>,
    bytes: usize,
    /// Captures for requests whose records are not open yet, oldest
    /// first, with when each was taken (issue 715).
    waiting: VecDeque<(String, admin::RequestCapture, Instant)>,
}

/// The ring of request records.
pub struct Recorder {
    ring: Mutex<Ring>,
    capacity: usize,
    max_bytes: usize,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (count, bytes) = self.usage();
        formatter
            .debug_struct("Recorder")
            .field("count", &count)
            .field("bytes", &bytes)
            .finish()
    }
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new(CAPACITY, MAX_BYTES)
    }
}

impl Recorder {
    /// A ring keeping at most `capacity` records of at most `max_bytes`
    /// in all.
    pub fn new(capacity: usize, max_bytes: usize) -> Self {
        Self {
            ring: Mutex::new(Ring {
                kept: VecDeque::new(),
                bytes: 0,
                waiting: VecDeque::new(),
            }),
            capacity: capacity.max(1),
            max_bytes,
        }
    }

    /// Keep a record, dropping the oldest while the ring is over either
    /// bound, and give back its slot for the rest of it.
    pub fn open(&self, mut record: admin::RequestRecord) -> Slot {
        let mut ring = self.ring.lock().expect("recorder poisoned");

        // Issue 715: a capture taken while the request was still in
        // flight joins its record as it opens.

        let now = Instant::now();
        ring.waiting
            .retain(|(_, _, at)| now.saturating_duration_since(*at) < WAITING_FOR);

        while let Some(index) = ring
            .waiting
            .iter()
            .position(|(request_id, _, _)| *request_id == record.request_id)
        {
            let (_, capture, _) = ring.waiting.remove(index).expect("found");

            if record.captures.len() < MAX_CAPTURES {
                record.captures.push(capture);
            }
        }

        let bytes = record.encoded_len();
        let slot = Arc::new(Mutex::new(record));
        ring.bytes += bytes;
        ring.kept.push_back(Kept {
            slot: Arc::clone(&slot),
            bytes,
        });
        self.trim(&mut ring);
        slot
    }

    /// Complete a record with the access log's record of its request,
    /// once its response has ended.
    pub fn complete(&self, slot: &Slot, logged: &Record) {
        self.update(slot, |record| fill(record, logged));
    }

    /// Change a record kept in the ring, and count its new size.
    pub fn update(&self, slot: &Slot, change: impl FnOnce(&mut admin::RequestRecord)) {
        let bytes = {
            let mut record = slot.lock().expect("request record poisoned");
            change(&mut record);
            record.encoded_len()
        };

        // The record may have been dropped from the ring meanwhile, and
        // then counts for nothing.

        let mut ring = self.ring.lock().expect("recorder poisoned");
        let found = ring
            .kept
            .iter_mut()
            .rev()
            .find(|kept| Arc::ptr_eq(&kept.slot, slot))
            .map(|kept| std::mem::replace(&mut kept.bytes, bytes));

        if let Some(before) = found {
            ring.bytes = ring.bytes - before + bytes;
            self.trim(&mut ring);
        }
    }

    /// The record of a request by its ID, if it is still kept, with its
    /// built-in panels' data.
    pub fn get(&self, request_id: &str) -> Option<admin::RequestRecord> {
        let ring = self.ring.lock().expect("recorder poisoned");

        let mut record = ring.kept.iter().rev().find_map(|kept| {
            let record = kept.slot.lock().expect("request record poisoned");
            (record.request_id == request_id).then(|| record.clone())
        })?;

        record.panels = crate::panels::all(&record);
        Some(record)
    }

    /// Read the records of several requests in one pass over the ring,
    /// each the newest kept under its ID, by what `read` takes from it.
    /// An ID not kept is absent from the answer.
    pub fn read_each<T>(
        &self,
        request_ids: &[String],
        read: impl Fn(&admin::RequestRecord) -> T,
    ) -> HashMap<String, T> {
        let wanted: HashSet<&str> = request_ids.iter().map(String::as_str).collect();
        let mut found = HashMap::with_capacity(wanted.len());
        let ring = self.ring.lock().expect("recorder poisoned");

        for kept in ring.kept.iter().rev() {
            if found.len() == wanted.len() {
                break;
            }

            let record = kept.slot.lock().expect("request record poisoned");

            if wanted.contains(record.request_id.as_str())
                && !found.contains_key(&record.request_id)
            {
                found.insert(record.request_id.clone(), read(&record));
            }
        }

        found
    }

    /// Join a capture to the record of each request it names that is
    /// still kept (issue 706): the newest record under each ID, whatever
    /// its state, takes the capture with its own thread's stack as the
    /// request's, the worker's other threads beside it, and the IDs of
    /// the other records it reached. A request with no record yet, still
    /// in flight, takes it when its record opens (issue 715). Returns the
    /// IDs reached, with those that wait.
    pub fn capture(&self, evidence: &Evidence) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut reached: Vec<(String, Slot)> = Vec::new();
        let mut later: Vec<String> = Vec::new();

        {
            let ring = self.ring.lock().expect("recorder poisoned");

            for request_id in &evidence.requests {
                if !seen.insert(request_id.as_str()) {
                    continue;
                }

                let found = ring.kept.iter().rev().find(|kept| {
                    kept.slot
                        .lock()
                        .expect("request record poisoned")
                        .request_id
                        == *request_id
                });

                match found {
                    Some(kept) => reached.push((request_id.clone(), Arc::clone(&kept.slot))),
                    None => later.push(request_id.clone()),
                }
            }
        }

        let ids: Vec<String> = reached
            .iter()
            .map(|(id, _)| id.clone())
            .chain(later.iter().cloned())
            .collect();

        if !later.is_empty() {
            let mut ring = self.ring.lock().expect("recorder poisoned");

            for request_id in &later {
                if ring.waiting.len() >= MAX_WAITING {
                    ring.waiting.pop_front();
                }

                let capture = evidence.for_request(request_id, &ids);
                ring.waiting
                    .push_back((request_id.clone(), capture, Instant::now()));
            }
        }

        for (request_id, slot) in &reached {
            let capture = evidence.for_request(request_id, &ids);

            self.update(slot, |record| {
                if record.captures.len() < MAX_CAPTURES {
                    record.captures.push(capture);
                }
            });
        }

        ids
    }

    /// Read every record kept, oldest first, by what `read` takes from
    /// each, leaving out those it gives nothing for.
    pub fn read_all<T>(&self, mut read: impl FnMut(&admin::RequestRecord) -> Option<T>) -> Vec<T> {
        let ring = self.ring.lock().expect("recorder poisoned");

        ring.kept
            .iter()
            .filter_map(|kept| read(&kept.slot.lock().expect("request record poisoned")))
            .collect()
    }

    /// How many records are kept, and their bytes.
    pub fn usage(&self) -> (usize, usize) {
        let ring = self.ring.lock().expect("recorder poisoned");
        (ring.kept.len(), ring.bytes)
    }

    fn trim(&self, ring: &mut Ring) {
        while ring.kept.len() > self.capacity
            || (ring.bytes > self.max_bytes && ring.kept.len() > 1)
        {
            let Some(oldest) = ring.kept.pop_front() else {
                break;
            };
            ring.bytes -= oldest.bytes;
        }
    }
}

/// Evidence of a timeout or a killed worker (issue 706), as the host
/// captured it: the event, the worker, and the stacks of its threads
/// serving requests, with the requests whose records take it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evidence {
    /// The event that captured it.
    pub kind: String,
    /// The event's message.
    pub message: String,
    /// When it was captured, in Unix milliseconds.
    pub at_unix_ms: u64,
    /// The worker's name, empty when the capture names none.
    pub worker: String,
    /// The worker's pid.
    pub pid: u32,
    /// Why a worker was killed.
    pub reason: String,
    /// The requests whose records take the capture, by ID.
    pub requests: Vec<String>,
    /// The worker's threads that were serving requests.
    pub threads: Vec<EvidenceThread>,
}

/// One thread of a capture, with the request it was serving.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvidenceThread {
    /// The thread's name.
    pub name: String,
    /// The request it was serving, empty for none.
    pub request_id: String,
    /// Its stack, as text.
    pub stack: String,
}

impl Evidence {
    /// The capture as one request's record keeps it: that request's own
    /// thread's stack, the other threads, and the other records reached.
    fn for_request(&self, request_id: &str, reached: &[String]) -> admin::RequestCapture {
        let stack = self
            .threads
            .iter()
            .find(|thread| thread.request_id == request_id)
            .map(|thread| thread.stack.clone())
            .unwrap_or_default();

        admin::RequestCapture {
            kind: self.kind.clone(),
            at_unix_ms: self.at_unix_ms,
            message: self.message.clone(),
            worker: self.worker.clone(),
            pid: self.pid,
            reason: self.reason.clone(),
            stack,
            threads: self
                .threads
                .iter()
                .filter(|thread| thread.request_id != request_id)
                .take(MAX_CAPTURE_THREADS)
                .map(|thread| admin::CaptureThread {
                    name: thread.name.clone(),
                    request_id: thread.request_id.clone(),
                    stack: thread.stack.clone(),
                })
                .collect(),
            related: reached
                .iter()
                .filter(|other| *other != request_id)
                .cloned()
                .collect(),
        }
    }
}

/// What the edge knows of a request as its response's head goes out.
pub struct Head<'a> {
    /// The client's request ID.
    pub request_id: &'a str,
    /// The method the client sent.
    pub method: &'a str,
    /// The target the client asked for.
    pub target: &'a str,
    /// The HTTP version.
    pub version: http::Version,
    /// The client's address.
    pub client: std::net::IpAddr,
    /// The request's headers as they arrived, masked.
    pub request_headers: Vec<admin::RecordedHeader>,
    /// The response's headers as the client gets them.
    pub response_headers: &'a http::HeaderMap,
    /// What compression did with the response.
    pub compressed: crate::compression::Outcome,
    /// The connection the request arrived on.
    pub connection: &'a crate::development::Connection,
    /// The request's place on it.
    pub arrival: crate::development::Arrival,
    /// The edge closes the connection after this response, its share of
    /// requests reached.
    pub closed_after: bool,
}

/// The head of a request's record, in progress until the access log's
/// record completes it.
pub fn head(head: Head<'_>) -> admin::RequestRecord {
    let compression = match head.compressed {
        Ok(encoding) => admin::RequestCompression {
            compressed: true,
            encoding: encoding.token().to_owned(),
            skipped: String::new(),
        },
        Err(skipped) => admin::RequestCompression {
            compressed: false,
            encoding: head
                .response_headers
                .get(http::header::CONTENT_ENCODING)
                .filter(|_| skipped == crate::compression::Skipped::Encoded)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
            skipped: skipped.as_str().to_owned(),
        },
    };

    // The handshake's time belongs to the connection's first request.

    let tls_handshake_ms = head
        .connection
        .handshake
        .filter(|_| head.arrival.position == 1)
        .map(|handshake| handshake.as_secs_f64() * 1000.0);

    admin::RequestRecord {
        request_id: head.request_id.to_owned(),
        in_progress: true,
        started_unix_ms: now_unix_ms(),
        client: head.client.to_string(),
        method: head.method.to_owned(),
        target: mask_query(head.target),
        version: format!("{:?}", head.version),
        connection: Some(admin::RequestConnection {
            id: head.connection.id.clone(),
            position: head.arrival.position,
            idle_ms: head.arrival.idle.as_secs_f64() * 1000.0,
            tls_handshake_ms,
            closed_after: head.closed_after,
        }),
        request_headers: head.request_headers,
        response_headers: headers(head.response_headers),
        compression: Some(compression),
        ..Default::default()
    }
}

/// Lay the access log's record of a request over its record, which then
/// is in progress only while the worker's part is still to come.
pub fn fill(record: &mut admin::RequestRecord, logged: &Record) {
    record.in_progress = record.worker_part == WORKER_EXPECTED;
    record.started_unix_ms = logged
        .started
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    record.client = logged.client.to_string();
    record.app = logged.app.clone();
    record.release = logged.release.clone();
    record.host = logged.host.clone();
    record.virtual_host = logged.virtual_host.clone();
    record.group = logged.group.clone();
    record.mount = logged.mount.clone();
    record.snapshot = logged.snapshot;
    record.method = logged.method.clone();
    record.target = mask_query(&logged.target);
    record.version = logged.version.clone();
    record.listener = logged.listener.clone();
    record.status = u32::from(logged.status);
    record.reason = logged.reason.clone();
    record.bytes = logged.bytes;
    record.received = logged.received;
    record.worker = logged.worker.map(|worker| worker as u32);
    record.retried = logged.retried;
    record.queue_ms = logged.queue.as_secs_f64() * 1000.0;
    record.app_ms = logged.application.as_secs_f64() * 1000.0;
    record.total_ms = logged.total.as_secs_f64() * 1000.0;
    record.redirects = logged
        .redirects
        .iter()
        .map(|redirect| mask_query(redirect))
        .collect();
    record.client_class = logged.client_class.clone();
    record.handoff = logged.handoff.unwrap_or_default().to_owned();
}

/// The worker's part of a record is still to come.
pub const WORKER_EXPECTED: &str = "expected";

/// The worker's part of a record arrived.
pub const WORKER_ARRIVED: &str = "arrived";

/// The worker's part of a record did not come: the worker died, or the
/// connection to it failed.
pub const WORKER_MISSING: &str = "missing";

/// Lay the worker's part over a record, which is then in progress only
/// while the access log's record is still to come.
pub fn worker(record: &mut admin::RequestRecord, part: muru_proto::worker::v1::WorkerRecord) {
    record.worker_part = WORKER_ARRIVED.to_owned();
    record.in_progress = record.status == 0;

    // What the worker captured itself, such as its stack at a soft
    // timeout, comes before what the host captured later.

    let group = if record.group.is_empty() {
        "default"
    } else {
        record.group.as_str()
    };
    let worker = record
        .worker
        .map(|index| muru_common::worker::WorkerName::new(group, index).to_string())
        .unwrap_or_default();
    let captured: Vec<admin::RequestCapture> = part
        .captures
        .into_iter()
        .map(|capture| admin::RequestCapture {
            kind: capture.kind,
            at_unix_ms: capture.at_unix_ms,
            message: capture.message,
            worker: worker.clone(),
            stack: capture.stack,
            ..Default::default()
        })
        .collect();
    record.captures.splice(0..0, captured);
    record.captures.truncate(MAX_CAPTURES);

    record.from_worker = Some(admin::WorkerPart {
        thread: part.thread,
        to_start_response: part.to_start_response,
        to_first_byte: part.to_first_byte,
        to_last_byte: part.to_last_byte,
        to_close: part.to_close,
        body_bytes: part.body_bytes,
        body_source: part.body_source,
        sent_by_edge: part.sent_by_edge,
        outcome: part.outcome,
        reason: part.reason,
        error_phase: part.error_phase,
        error: part.error,
        traceback: part.traceback,
        logs: part
            .logs
            .into_iter()
            .map(|line| admin::RequestLog {
                at_ms: line.at,
                level: line.level,
                logger: line.logger,
                message: line.message,
            })
            .collect(),
        logs_dropped: part.logs_dropped,
        thread_wait: part.thread_wait,
        gil_wait: part.gil_wait,
        gil_waits: part.gil_waits,
        gil_wait_first: part.gil_wait_first,
        gc: part
            .gc
            .into_iter()
            .map(|generation| admin::RequestGc {
                generation: generation.generation,
                collections: generation.collections,
                ms: generation.ms,
            })
            .collect(),
        loop_lag: part.loop_lag,
        loop_lag_at: part.loop_lag_at,
        panels: part.panels.into_iter().map(panel).collect(),
        gil_wait_stretches: part.gil_wait_stretches.into_iter().map(stretch).collect(),
        loop_held_stretches: part.loop_held_stretches.into_iter().map(stretch).collect(),
        module_loads: part
            .module_loads
            .into_iter()
            .map(|load| admin::ModuleLoad {
                module: load.module,
                at_ms: load.at,
                ms: load.ms,
                depth: load.depth,
                trigger: load.trigger,
                deferred: load.deferred,
            })
            .collect(),
        modules_loaded: part.modules_loaded,
        modules_ms: part.modules_ms,
    });
}

/// A stretch of the worker's part, on the worker's clock (issue 737).
fn stretch(taken: muru_proto::worker::v1::Stretch) -> admin::Stretch {
    admin::Stretch {
        at_ms: taken.at,
        ms: taken.ms,
    }
}

/// A panel code in the worker added (issue 677), as the record
/// keeps it: its spans still on the worker's clock.
fn panel(panel: muru_proto::worker::v1::RecordPanel) -> admin::Panel {
    fn span(taken: muru_proto::worker::v1::RecordSpan) -> admin::PanelSpan {
        admin::PanelSpan {
            name: taken.name,
            start_ms: taken.start,
            duration_ms: taken.duration.max(0.0),
            category: taken.category,
            children: taken.children.into_iter().map(span).collect(),
            failed: taken.failed,
            at_ms: taken.at,
            segments: taken
                .segments
                .into_iter()
                .map(|segment| admin::PanelSegment {
                    start_ms: segment.start,
                    duration_ms: segment.duration.max(0.0),
                    label: segment.label,
                    detail: segment.detail,
                })
                .collect(),
        }
    }

    admin::Panel {
        name: panel.name,
        title: panel.title,
        spans: panel.spans.into_iter().map(span).collect(),
        values: panel
            .values
            .into_iter()
            .map(|value| admin::PanelValue {
                name: value.name,
                value: value.value,
            })
            .collect(),
        tables: panel
            .tables
            .into_iter()
            .map(|table| admin::PanelTable {
                caption: table.caption,
                columns: table.columns,
                rows: table
                    .rows
                    .into_iter()
                    .map(|row| admin::PanelRow { cells: row.cells })
                    .collect(),
            })
            .collect(),
        dropped: panel.dropped,
        text: panel.text,
        markdown: panel.markdown,
    }
}

/// Mark a record's worker part as never arriving.
pub fn worker_missing(record: &mut admin::RequestRecord) {
    if record.worker_part == WORKER_EXPECTED {
        record.worker_part = WORKER_MISSING.to_owned();
        record.in_progress = record.status == 0;
    }
}

/// The time now, in Unix milliseconds.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Headers as a record keeps them: every value, in order, with secret
/// values masked.
pub fn headers(headers: &http::HeaderMap) -> Vec<admin::RecordedHeader> {
    headers
        .iter()
        .map(|(name, value)| admin::RecordedHeader {
            name: name.as_str().to_owned(),
            value: mask(name.as_str(), &String::from_utf8_lossy(value.as_bytes())),
        })
        .collect()
}

/// A header's value as a record keeps it: masked whole for a header
/// that always holds a secret or whose name looks like one, otherwise
/// with the password of any URL in it masked.
pub fn mask(name: &str, value: &str) -> String {
    let lower = name.to_ascii_lowercase();

    if ALWAYS_MASKED.contains(&lower.as_str()) {
        return MASKED.to_owned();
    }

    let upper = lower.to_ascii_uppercase().replace('-', "_");

    if upper.ends_with("_DSN") || SECRET_WORDS.iter().any(|word| upper.contains(word)) {
        return MASKED.to_owned();
    }

    // A value may list URLs, separated by commas, semicolons or spaces;
    // each is taken on its own and the separators kept.

    let mut masked = String::with_capacity(value.len());
    let mut part = String::new();

    for character in value.chars() {
        if matches!(character, ',' | ';') || character.is_whitespace() {
            masked.push_str(&mask_url(&part));
            part.clear();
            masked.push(character);
        } else {
            part.push(character);
        }
    }

    masked.push_str(&mask_url(&part));

    // A header that carries a URL keeps the secrets in its query out of
    // the record, as the target does.

    if URL_HEADERS.contains(&lower.as_str()) {
        return mask_query(&masked);
    }

    masked
}

/// A target or URL with the values of its secret query parameters masked
/// (see [`secret_parameter`]), and everything else as it came: the path,
/// the fragment, the order and spelling of the parameters, and the
/// values of the others.
pub fn mask_query(target: &str) -> String {
    let Some((before, after)) = target.split_once('?') else {
        return target.to_owned();
    };

    let (query, fragment) = match after.split_once('#') {
        Some((query, fragment)) => (query, Some(fragment)),
        None => (after, None),
    };

    let parameters: Vec<String> = query
        .split('&')
        .map(|parameter| match parameter.split_once('=') {
            Some((name, _)) if secret_parameter(name) => format!("{name}={MASKED}"),
            _ => parameter.to_owned(),
        })
        .collect();

    let mut masked = format!("{before}?{}", parameters.join("&"));

    if let Some(fragment) = fragment {
        masked.push('#');
        masked.push_str(fragment);
    }

    masked
}

/// Whether a query parameter's value is a secret, by its name as it
/// appears in the query. The name is decoded and case-folded, its last
/// bracketed part taken for a form-style name (`user[password]` is
/// `password`), and `-`, `_` and `.` removed, so `access_token`,
/// `accessToken` and `Access-Token` are one name. It is a secret when
/// it is one of [`SECRET_PARAMETERS`] or ends in one of
/// [`SECRET_PARAMETER_ENDINGS`].
fn secret_parameter(raw: &str) -> bool {
    let decoded = percent_encoding::percent_decode_str(&raw.replace('+', " "))
        .decode_utf8_lossy()
        .to_lowercase();

    let mut name = decoded.as_str();

    // `token[]` is a list named `token`; `user[password]` is the
    // password of a user.

    while let Some(stripped) = name.strip_suffix("[]") {
        name = stripped;
    }

    if let Some(inner) = name.strip_suffix(']')
        && let Some(open) = inner.rfind('[')
    {
        name = &inner[open + 1..];
    }

    let normalised: String = name
        .chars()
        .filter(|character| !matches!(character, '-' | '_' | '.'))
        .collect();

    SECRET_PARAMETERS.contains(&normalised.as_str())
        || SECRET_PARAMETER_ENDINGS
            .iter()
            .any(|ending| normalised.ends_with(ending))
}

/// A URL with its password masked, or the text as it was when it is not
/// a URL with a password.
fn mask_url(text: &str) -> String {
    let Some((scheme, rest)) = text.split_once("://") else {
        return text.to_owned();
    };

    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);

    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return text.to_owned();
    };

    let Some((user, _password)) = userinfo.split_once(':') else {
        return text.to_owned();
    };

    format!("{scheme}://{user}:{MASKED}@{host}{path}")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use super::*;

    pub(crate) fn logged(request_id: &str) -> Record {
        Record {
            started: UNIX_EPOCH + Duration::from_millis(1_700_000_000_123),
            request_id: request_id.to_owned(),
            client: IpAddr::V4(Ipv4Addr::LOCALHOST),
            app: "shop".to_owned(),
            release: "r3".to_owned(),
            host: "localhost".to_owned(),
            virtual_host: String::new(),
            referer: None,
            user_agent: None,
            group: "default".to_owned(),
            mount: "default".to_owned(),
            snapshot: 0,
            method: "GET".to_owned(),
            target: "/orders/42".to_owned(),
            version: "HTTP/1.1".to_owned(),
            listener: "127.0.0.1:8080".to_owned(),
            status: 200,
            reason: String::new(),
            bytes: 1234,
            received: 0,
            worker: Some(0),
            retried: false,
            queue: Duration::from_micros(200),
            application: Duration::from_micros(17_900),
            total: Duration::from_micros(18_400),
            redirects: Vec::new(),
            client_class: "default".to_owned(),
            websocket: None,
            handoff: None,
            connection: "C1".to_owned(),
            connection_requests: 1,
        }
    }

    fn opened(request_id: &str, padding: usize) -> admin::RequestRecord {
        admin::RequestRecord {
            request_id: request_id.to_owned(),
            in_progress: true,
            target: "x".repeat(padding),
            ..Default::default()
        }
    }

    #[test]
    fn a_capture_reaches_each_request_it_names_that_is_kept() {
        let recorder = Recorder::default();

        for request_id in ["A", "B"] {
            recorder.open(admin::RequestRecord {
                request_id: request_id.to_owned(),
                ..Default::default()
            });
        }

        let thread = |name: &str, request_id: &str| EvidenceThread {
            name: name.to_owned(),
            request_id: request_id.to_owned(),
            stack: format!("stack of {name}"),
        };
        let evidence = Evidence {
            kind: "worker.timeout_killed".to_owned(),
            message: "killed".to_owned(),
            worker: "0".to_owned(),
            pid: 7,
            requests: ["A", "B", "GONE", "A"].map(str::to_owned).to_vec(),
            threads: vec![thread("t1", "A"), thread("t2", "B"), thread("t3", "GONE")],
            ..Default::default()
        };

        assert_eq!(recorder.capture(&evidence), ["A", "B", "GONE"]);

        // Each record has its own thread's stack as the request's, the
        // others beside it, and the other record reached.

        let found = recorder.get("A").unwrap().captures;
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].stack.as_str(), found[0].pid), ("stack of t1", 7));
        let others: Vec<&str> = found[0]
            .threads
            .iter()
            .map(|thread| thread.name.as_str())
            .collect();
        assert_eq!(others, ["t2", "t3"]);
        assert_eq!(found[0].related, ["B", "GONE"]);
        assert_eq!(
            recorder.get("B").unwrap().captures[0].related,
            ["A", "GONE"]
        );

        // Issue 715: a request with no record yet, still in flight, takes
        // the capture when its record opens, and only once.

        recorder.open(admin::RequestRecord {
            request_id: "GONE".to_owned(),
            ..Default::default()
        });
        let late = recorder.get("GONE").unwrap().captures;
        assert_eq!(late.len(), 1);
        assert_eq!(
            (late[0].stack.as_str(), late[0].related.clone()),
            ("stack of t3", vec!["A".to_owned(), "B".to_owned()])
        );
        recorder.open(admin::RequestRecord {
            request_id: "LATER".to_owned(),
            ..Default::default()
        });
        assert!(recorder.get("LATER").unwrap().captures.is_empty());

        // At most MAX_CAPTURES on one record; a worker's own captures go
        // first when its part arrives.

        for _ in 0..MAX_CAPTURES {
            recorder.capture(&evidence);
        }

        assert_eq!(recorder.get("A").unwrap().captures.len(), MAX_CAPTURES);

        let mut record = admin::RequestRecord::default();
        record.captures.push(admin::RequestCapture::default());
        worker(
            &mut record,
            muru_proto::worker::v1::WorkerRecord {
                captures: vec![muru_proto::worker::v1::RecordCapture {
                    kind: "request.soft_timeout".to_owned(),
                    stack: "here".to_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        assert_eq!(record.captures[0].kind, "request.soft_timeout");
        assert_eq!(record.captures.len(), 2);
    }

    #[test]
    fn the_ring_drops_the_oldest_past_its_count_or_its_bytes() {
        let recorder = Recorder::new(3, 1000);

        for id in ["A", "B", "C", "D"] {
            recorder.open(opened(id, 10));
        }

        assert!(recorder.get("A").is_none(), "past the count");
        assert!(recorder.get("D").is_some());
        assert_eq!(recorder.usage().0, 3);

        // A record that grows when it completes counts its new size, and
        // pushes out what no longer fits.

        let slot = recorder.open(opened("E", 10));
        let mut big = logged("E");
        big.target = "y".repeat(850);
        recorder.complete(&slot, &big);
        let (count, bytes) = recorder.usage();
        assert!(bytes <= 1000, "{bytes}");
        assert!(count < 3, "{count}");

        let record = recorder.get("E").unwrap();
        assert!(!record.in_progress);
        assert_eq!(record.status, 200);
        assert_eq!(record.worker, Some(0));
        assert_eq!(record.started_unix_ms, 1_700_000_000_123);

        // The newest record is kept even when it alone is over the bytes.

        recorder.open(opened("F", 5000));
        assert!(recorder.get("F").is_some());
        assert_eq!(recorder.usage().0, 1);
    }

    #[test]
    fn secrets_are_masked_as_inspect_masks_them() {
        assert_eq!(mask("Cookie", "session=abc"), MASKED);
        assert_eq!(mask("set-cookie", "a=b"), MASKED);
        assert_eq!(mask("Authorization", "Bearer x"), MASKED);
        assert_eq!(mask("Proxy-Authorization", "Basic x"), MASKED);
        assert_eq!(mask("X-Api-Key", "k"), MASKED);
        assert_eq!(mask("X-Auth-Token", "t"), MASKED);
        assert_eq!(mask("X-Sentry-DSN", "https://k@sentry.io/1"), MASKED);
        assert_eq!(mask("Content-Type", "text/html"), "text/html");
        assert_eq!(mask("Accept", "*/*"), "*/*");

        // A URL's password is masked wherever it is, its user and host
        // kept.

        assert_eq!(
            mask("Referer", "https://shop:hunter2@example.com/a?b=c"),
            "https://shop:***@example.com/a?b=c"
        );
        assert_eq!(
            mask("Link", "http://a@x.com, http://u:p@y.com/z"),
            "http://a@x.com, http://u:***@y.com/z"
        );

        let mut map = http::HeaderMap::new();
        map.insert("cookie", "a=b".parse().unwrap());
        map.insert("accept", "text/html".parse().unwrap());
        let kept = headers(&map);
        assert_eq!(kept.len(), 2);
        assert!(
            kept.iter()
                .any(|header| header.name == "cookie" && header.value == MASKED)
        );
    }

    #[test]
    fn secret_query_parameters_are_masked_and_the_rest_kept_as_it_came() {
        assert_eq!(
            mask_query("/books?token=abc&page=2"),
            "/books?token=***&page=2"
        );
        assert_eq!(mask_query("/books"), "/books");
        assert_eq!(mask_query("/books?"), "/books?");
        assert_eq!(
            mask_query("/login?next=/a&code=xyz&state=s#top"),
            "/login?next=/a&code=***&state=s#top"
        );

        // A name with no value, or an empty one, has nothing to hide, and
        // a repeated name is masked each time.

        assert_eq!(mask_query("/a?token&key="), "/a?token&key=***");
        assert_eq!(mask_query("/a?sig=1&sig=2"), "/a?sig=***&sig=***");
    }

    #[test]
    fn a_parameter_name_is_normalised_before_it_is_matched() {
        for name in [
            "access_token",
            "accessToken",
            "Access-Token",
            "access.token",
            "API_KEY",
            "api-key",
            "apiKey",
            "X-Amz-Security-Token",
            "user%5Bpassword%5D",
            "user[password]",
            "auth[token]",
            "token[]",
            "csrf_token",
            "github_token",
            "new_password",
            "client_secret",
            "PHPSESSID",
        ] {
            assert!(secret_parameter(name), "{name} is a secret");
        }

        for name in [
            "author", "keyword", "monkey", "sort_key", "page", "q", "state", "next",
        ] {
            assert!(!secret_parameter(name), "{name} is not a secret");
        }
    }

    #[test]
    fn a_url_header_has_its_query_masked() {
        assert_eq!(
            mask("Referer", "https://shop.example/cart?session=abc&step=2"),
            "https://shop.example/cart?session=***&step=2"
        );
        assert_eq!(
            mask("Location", "/login?next=/orders"),
            "/login?next=/orders"
        );
        assert_eq!(mask("Location", "/done?token=t"), "/done?token=***");
        assert_eq!(
            mask("Content-Type", "text/plain?token=t"),
            "text/plain?token=t"
        );
    }
}
