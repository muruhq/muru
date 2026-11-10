//! What `muructl debug bundle --scrub` takes out of a bundle that has to
//! go to someone with a narrower need than the people who own the
//! system: client addresses, which read as placeholders, the same one
//! for the same address so that a client can still be followed through
//! the bundle, and the query strings of request targets.
//!
//! An access log record is a JSON object whose fields are known, and
//! every string in it is scrubbed. Any other line is free text, in which
//! only what has the shape of an address or of a target with a query
//! can be found.

use super::redact::Redactor;

/// What a query string that was cut reads as.
const CUT: &str = "?...";

/// The replacements for one bundle.
#[derive(Default)]
pub struct Scrubber {
    redactor: Redactor,
}

impl Scrubber {
    /// One line of a log, without its line end: a JSON record with every
    /// string in it scrubbed, and any other line as text.
    pub fn line(&mut self, line: &str) -> String {
        if line.starts_with('{')
            && let Ok(mut record) = serde_json::from_str::<serde_json::Value>(line)
        {
            self.json(&mut record);
            return record.to_string();
        }

        self.text(line)
    }

    /// A text with its addresses replaced and its query strings cut.
    pub fn text(&mut self, text: &str) -> String {
        self.redactor.addresses(&without_queries(text))
    }

    /// Every string in a JSON value, scrubbed as text.
    pub fn json(&mut self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.text(text),
            serde_json::Value::Array(items) => items.iter_mut().for_each(|item| self.json(item)),
            serde_json::Value::Object(fields) => {
                fields.values_mut().for_each(|field| self.json(field));
            }
            _ => {}
        }
    }
}

/// A text with the query string cut from each request target and URL in
/// it: from a word that starts with a slash or holds `://`, whatever
/// follows its first question mark.
fn without_queries(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let ends = |c: char| c.is_whitespace() || "\"'`<>,;".contains(c);

    loop {
        let end = rest.find(ends).unwrap_or(rest.len());
        let word = &rest[..end];

        match word.find('?') {
            Some(at) if word.starts_with('/') || word[..at].contains("://") => {
                out.push_str(&word[..at]);
                out.push_str(CUT);
            }
            _ => out.push_str(word),
        }

        let Some(separator) = rest[end..].chars().next() else {
            return out;
        };

        out.push(separator);
        rest = &rest[end + separator.len_utf8()..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_access_record_loses_its_client_and_its_query() {
        let mut scrubber = Scrubber::default();
        let record = r#"{"time":"2026-11-02T05:42:22.836Z","client":"203.0.113.9","app":"shop","host":"shop.example.com","method":"GET","target":"/orders?id=7&token=abc","status":200,"redirects":["/login?next=/orders"],"websocket":null}"#;
        let scrubbed: serde_json::Value = serde_json::from_str(&scrubber.line(record)).unwrap();

        assert_eq!(scrubbed["client"], "<address1>");
        assert_eq!(scrubbed["target"], "/orders?...");
        assert_eq!(scrubbed["redirects"][0], "/login?...");
        assert_eq!(scrubbed["time"], "2026-11-02T05:42:22.836Z");
        assert_eq!(scrubbed["host"], "shop.example.com");
        assert_eq!(scrubbed["status"], 200);

        // The same client reads the same on a later line, and another
        // differently.

        let again = scrubber.line(r#"{"client":"203.0.113.9","target":"/"}"#);
        assert_eq!(again, r#"{"client":"<address1>","target":"/"}"#);
        assert_eq!(
            scrubber.line(r#"{"client":"198.51.100.4","target":"/"}"#),
            r#"{"client":"<address2>","target":"/"}"#
        );
    }

    #[test]
    fn a_line_of_text_loses_what_looks_like_an_address_or_a_query() {
        let mut scrubber = Scrubber::default();

        assert_eq!(
            scrubber.line(
                r#"203.0.113.9 - - [02/Nov/2026:05:42:22 +0000] "GET /orders?id=7 HTTP/1.1" 200 5 "https://shop.example.com/cart?step=2" "curl/8""#
            ),
            r#"<address1> - - [02/Nov/2026:05:42:22 +0000] "GET /orders?... HTTP/1.1" 200 5 "https://shop.example.com/cart?..." "curl/8""#
        );
        assert_eq!(
            scrubber
                .line("2026-11-02T05:42:22.836Z muru.worker[0]: INFO order for 203.0.113.9 ok?"),
            "2026-11-02T05:42:22.836Z muru.worker[0]: INFO order for <address1> ok?"
        );
        assert_eq!(
            scrubber.line("serving on 127.0.0.1:8000 since 05:42:22"),
            "serving on 127.0.0.1:8000 since 05:42:22"
        );
        assert_eq!(scrubber.line("{not json} /a?b"), "{not json} /a?...");
        assert_eq!(scrubber.line(""), "");
    }
}
