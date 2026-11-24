//! Entering a detour's preview (design specification section 4.21, ADR
//! 0075). While a daemon takes detours its edge answers paths under
//! `/_muru/preview/` itself, before routing: a one-time entry token sets
//! the preview cookie, which carries the detour's release and secret,
//! and redirects to the application's address, and `leave` expires the
//! cookie. The daemon mints the tokens and sends each edge only their
//! hashes, with when they expire and where they lead.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderValue, Method, Response, StatusCode};
use http_body_util::{BodyExt, Full};

use crate::routing::PREVIEW_COOKIE;

/// The paths the edge answers itself while a daemon takes detours.
pub const PREFIX: &str = "/_muru/preview/";

/// The path that leaves preview.
pub const LEAVE: &str = "leave";

/// The most tokens an edge keeps, a bound on what minting can cost it;
/// the oldest go first.
const MOST_TOKENS: usize = 1024;

/// The body type the edge answers with.
type Body = crate::ResponseBody;

/// What an entry token leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The application.
    pub app: String,
    /// The detour's release.
    pub release: String,
    /// The path to redirect to, on the host the token was used on.
    pub path: String,
    expires: Instant,
}

/// The entry tokens an edge holds, by the SHA-256 of each.
#[derive(Debug, Default)]
pub struct Tokens {
    entries: Mutex<HashMap<[u8; 32], Entry>>,
}

impl Tokens {
    /// Keep a token, by its hash, for `ttl`.
    pub fn add(&self, hash: [u8; 32], app: &str, release: &str, path: &str, ttl: Duration) {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("tokens poisoned");
        entries.retain(|_, entry| entry.expires > now);

        while entries.len() >= MOST_TOKENS {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires)
                .map(|(hash, _)| *hash)
            else {
                break;
            };

            entries.remove(&oldest);
        }

        entries.insert(
            hash,
            Entry {
                app: app.to_owned(),
                release: release.to_owned(),
                path: path.to_owned(),
                expires: now + ttl,
            },
        );
    }

    /// Use a token: what it leads to, once, while it lasts. A token used,
    /// expired or never given is none.
    pub fn take(&self, token: &str) -> Option<Entry> {
        let mut entries = self.entries.lock().expect("tokens poisoned");
        let entry = entries.remove(&hash(token))?;
        (entry.expires > Instant::now()).then_some(entry)
    }
}

/// A token's SHA-256, as the daemon sends it and the edge keeps it.
pub fn hash(token: &str) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    let mut hash = [0u8; 32];
    hash.copy_from_slice(digest.as_ref());
    hash
}

/// Whether a path is one the edge answers while a daemon takes detours.
pub fn reserved(path: &str) -> bool {
    path.starts_with(PREFIX) || path == PREFIX.trim_end_matches('/')
}

/// The preview cookie's value for a detour: its release and its secret.
pub fn value(release: &str, secret: &str) -> String {
    format!("{release}.{secret}")
}

/// The cookie that enters a detour's preview: for the session, for the
/// host the request named and every path on it.
pub fn set_cookie(value: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{PREVIEW_COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax{secure}")
}

/// The cookie that leaves preview, or drops one that names a detour that
/// has gone.
pub fn expire_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{PREVIEW_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure}")
}

/// Answer a path under the prefix: a token, `leave`, or nothing. Only
/// GET and HEAD are answered. `secret_of` gives a detour's secret while
/// it is open.
pub fn answer(
    method: &Method,
    path: &str,
    tokens: &Tokens,
    secure: bool,
    secret_of: impl Fn(&str, &str) -> Option<String>,
) -> Response<Body> {
    if method != Method::GET && method != Method::HEAD {
        return page(
            StatusCode::METHOD_NOT_ALLOWED,
            "Only GET opens a preview link.",
            None,
            None,
        );
    }

    let rest = path.strip_prefix(PREFIX).unwrap_or_default();

    if rest == LEAVE {
        return page(
            StatusCode::SEE_OTHER,
            "You have left the preview.",
            Some("/"),
            Some(expire_cookie(secure)),
        );
    }

    if rest.is_empty() || rest.contains('/') {
        return page(StatusCode::NOT_FOUND, "There is nothing here.", None, None);
    }

    let entered = tokens
        .take(rest)
        .and_then(|entry| secret_of(&entry.app, &entry.release).map(|secret| (entry, secret)));

    match entered {
        Some((entry, secret)) => page(
            StatusCode::SEE_OTHER,
            "You are in the preview of a detour.",
            Some(&entry.path),
            Some(set_cookie(&value(&entry.release, &secret), secure)),
        ),
        None => page(
            StatusCode::NOT_FOUND,
            "This preview link is unknown, used or expired, or its detour has ended. A link \
             works once, within a minute: muructl detour preview gives a new one.",
            None,
            None,
        ),
    }
}

/// A small page the edge answers with, optionally a redirect that sets a
/// cookie.
fn page(
    status: StatusCode,
    text: &str,
    location: Option<&str>,
    cookie: Option<String>,
) -> Response<Body> {
    let body = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>Muru preview</title></head>\
         <body><p>{text}</p></body></html>\n"
    );
    let mut response = Response::new(
        Full::new(Bytes::from(body))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;

    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );

    if let Some(location) = location.and_then(|location| HeaderValue::from_str(location).ok()) {
        headers.insert(http::header::LOCATION, location);
    }

    if let Some(cookie) = cookie.and_then(|cookie| HeaderValue::from_str(&cookie).ok()) {
        headers.insert(http::header::SET_COOKIE, cookie);
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret_of(app: &str, release: &str) -> Option<String> {
        (app == "shop" && release == "dev-7f3a").then(|| "0f1e2d".to_owned())
    }

    fn header<'a>(response: &'a Response<Body>, name: &str) -> &'a str {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    }

    #[test]
    fn a_token_enters_once_and_sets_the_cookie_for_the_host() {
        let tokens = Tokens::default();
        tokens.add(
            hash("abc"),
            "shop",
            "dev-7f3a",
            "/cart/",
            Duration::from_secs(60),
        );

        let entered = answer(&Method::GET, "/_muru/preview/abc", &tokens, true, secret_of);
        assert_eq!(entered.status(), StatusCode::SEE_OTHER);
        assert_eq!(header(&entered, "location"), "/cart/");
        assert_eq!(
            header(&entered, "set-cookie"),
            "muru_preview=dev-7f3a.0f1e2d; Path=/; HttpOnly; SameSite=Lax; Secure"
        );
        assert_eq!(header(&entered, "cache-control"), "no-store");

        // Used once, it enters no more.

        let again = answer(&Method::GET, "/_muru/preview/abc", &tokens, true, secret_of);
        assert_eq!(again.status(), StatusCode::NOT_FOUND);
        assert!(again.headers().get("set-cookie").is_none());
    }

    #[test]
    fn an_expired_or_unknown_token_or_one_of_a_closed_detour_is_a_page_saying_so() {
        let tokens = Tokens::default();
        tokens.add(hash("old"), "shop", "dev-7f3a", "/", Duration::ZERO);
        tokens.add(
            hash("gone"),
            "shop",
            "dev-0000",
            "/",
            Duration::from_secs(60),
        );

        for token in ["old", "never", "gone"] {
            let path = format!("/_muru/preview/{token}");
            let response = answer(&Method::GET, &path, &tokens, false, secret_of);
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{token}");
        }

        assert_eq!(
            answer(&Method::POST, "/_muru/preview/x", &tokens, false, secret_of).status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            answer(
                &Method::GET,
                "/_muru/preview/a/b",
                &tokens,
                false,
                secret_of
            )
            .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn leaving_expires_the_cookie_and_goes_home() {
        let left = answer(
            &Method::GET,
            "/_muru/preview/leave",
            &Tokens::default(),
            false,
            secret_of,
        );
        assert_eq!(left.status(), StatusCode::SEE_OTHER);
        assert_eq!(header(&left, "location"), "/");
        assert_eq!(
            header(&left, "set-cookie"),
            "muru_preview=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax"
        );
        assert!(reserved("/_muru/preview/leave"));
        assert!(reserved("/_muru/preview"));
        assert!(!reserved("/_muru/toolbar/toolbar.js"));
    }
}
