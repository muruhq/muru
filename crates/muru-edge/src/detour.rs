//! What the daemon's edge tells a detour's run about each request
//! (design specification section 4.21, ADR 0077): whether the request
//! reached the detour by its preview, and the matched route's path as
//! the run receives it. Both travel in one internal header, which the
//! daemon's edge alone sets: a client's own is removed with every other
//! X-Muru header before the request reaches the run, and the run's edge
//! takes it off before the application sees the request.
//!
//! The header's value is `preview` or `traffic`, then, when the route's
//! path is not the root, `; base=PATH` with the path percent-encoded as
//! a URL path is:
//!
//! ```text
//! x-muru-detour: preview; base=/app
//! ```

use http::{HeaderMap, HeaderName, HeaderValue};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

/// The internal header the daemon's edge sends a detour's run.
pub const HEADER: HeaderName = HeaderName::from_static("x-muru-detour");

/// What a path's characters are encoded as in the header: everything a
/// URL path would encode, and the separators the value uses.
const PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b';')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// How a request reached a detour, as the daemon's edge tells its run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Via {
    /// The request carries the detour's preview, its release and secret,
    /// so it is the developer's: only such a request gets the toolbar.
    pub previewed: bool,
    /// The path of the route the request matched, as the run receives
    /// it: the route's path with what the route strips taken off, and
    /// without a trailing slash. Empty for a route at the root, or at
    /// the part it strips. The run's toolbar lives under it, so its
    /// requests match the application's route as the page's do.
    pub base: String,
}

impl Via {
    /// The header's value.
    pub fn value(&self) -> HeaderValue {
        let how = if self.previewed { "preview" } else { "traffic" };
        let base = self.base.trim_end_matches('/');

        let text = if base.is_empty() {
            how.to_owned()
        } else {
            format!("{how}; base={}", utf8_percent_encode(base, PATH))
        };

        HeaderValue::from_str(&text).unwrap_or_else(|_| HeaderValue::from_static("traffic"))
    }

    /// Read the header's value; none for one that is not this form.
    pub fn parse(value: &HeaderValue) -> Option<Self> {
        let text = value.to_str().ok()?;
        let mut parts = text.split(';').map(str::trim);

        let previewed = match parts.next()? {
            "preview" => true,
            "traffic" => false,
            _ => return None,
        };

        let mut base = String::new();

        for part in parts {
            let encoded = part.strip_prefix("base=")?;
            base = percent_decode_str(encoded).decode_utf8().ok()?.into_owned();

            if !base.starts_with('/') {
                return None;
            }
        }

        Some(Self {
            previewed,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    /// Take the header off a request, read. A run that is no detour
    /// takes it off and trusts nothing it said, which is `trusted`
    /// false: its peers are not the daemon's edge.
    pub fn take(headers: &mut HeaderMap, trusted: bool) -> Option<Self> {
        let value = headers.remove(HEADER)?;
        trusted.then(|| Self::parse(&value)).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_says_how_and_under_which_path_and_reads_back() {
        let previewed = Via {
            previewed: true,
            base: String::new(),
        };
        assert_eq!(previewed.value(), "preview");
        assert_eq!(Via::parse(&previewed.value()), Some(previewed));

        let under = Via {
            previewed: false,
            base: "/app/caf\u{e9} menu/".to_owned(),
        };
        assert_eq!(under.value(), "traffic; base=/app/caf%C3%A9%20menu");
        assert_eq!(
            Via::parse(&under.value()),
            Some(Via {
                previewed: false,
                base: "/app/caf\u{e9} menu".to_owned(),
            })
        );

        for bad in ["", "maybe", "preview; root=/x", "preview; base=app"] {
            assert_eq!(Via::parse(&HeaderValue::from_static(bad)), None, "{bad}");
        }
    }

    #[test]
    fn the_header_is_taken_off_and_trusted_only_from_the_daemons_edge() {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER, HeaderValue::from_static("preview; base=/app"));
        assert_eq!(
            Via::take(&mut headers, true),
            Some(Via {
                previewed: true,
                base: "/app".to_owned(),
            })
        );
        assert!(headers.is_empty());

        headers.insert(HEADER, HeaderValue::from_static("preview"));
        assert_eq!(Via::take(&mut headers, false), None);
        assert!(headers.is_empty());
    }
}
