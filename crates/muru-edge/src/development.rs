//! What a development run tells the browser about each request (design
//! specification section 4.20): `Server-Timing` with the queue and
//! application times and the request ID, and `Muru-Request-Link` naming
//! the request's page on the dashboard. Neither is sent outside
//! development mode.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

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

/// What a development run learns of one client request as it is
/// handled, shared by every hop of it (section 4.10) so that the final
/// response can carry it.
#[derive(Debug, Default)]
pub struct Request {
    timing: Mutex<Option<Timing>>,
}

impl Request {
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
