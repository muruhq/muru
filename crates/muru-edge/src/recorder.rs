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
use std::time::{SystemTime, UNIX_EPOCH};

use muru_proto::admin::v1 as admin;
use prost::Message as _;

use crate::access::Record;

/// The most records kept.
pub const CAPACITY: usize = 1000;

/// The most bytes of records kept, measured as their encoded size.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

/// What a masked value reads.
pub const MASKED: &str = "***";

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

/// One record in the ring, shared with the request that is filling it.
pub type Slot = Arc<Mutex<admin::RequestRecord>>;

struct Kept {
    slot: Slot,
    bytes: usize,
}

struct Ring {
    kept: VecDeque<Kept>,
    bytes: usize,
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
            }),
            capacity: capacity.max(1),
            max_bytes,
        }
    }

    /// Keep a record, dropping the oldest while the ring is over either
    /// bound, and give back its slot for the rest of it.
    pub fn open(&self, record: admin::RequestRecord) -> Slot {
        let bytes = record.encoded_len();
        let slot = Arc::new(Mutex::new(record));
        let mut ring = self.ring.lock().expect("recorder poisoned");
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

        record.panels = crate::panels::builtin(&record);
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
        target: head.target.to_owned(),
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
    record.target = logged.target.clone();
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
    record.redirects = logged.redirects.clone();
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
    });
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
    masked
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
}
