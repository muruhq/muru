//! Signing in to the daemon's dashboard (design specification section
//! 10.7). Every page under the daemon needs a session, though none of
//! them changes anything. A person signs in with a single-use link that
//! `muructl dashboard login` asks the daemon for over the admin socket,
//! so whoever may use the socket may look; the link gives the browser a
//! session cookie. A script is given a session's id to send as a bearer
//! token.

use std::collections::HashMap;
use std::io::Read as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a login link may wait to be opened.
pub const CODE_LIFETIME: Duration = Duration::from_secs(300);

/// How long a session lasts once signed in.
const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 3600);

/// The cookie that carries the session.
pub const SESSION_COOKIE: &str = "muru_dashboard";

/// Login codes and sessions, shared between the admin API, which issues
/// the codes, and the dashboard, which redeems them.
#[derive(Debug, Default)]
pub struct Sessions {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Each code's expiry.
    codes: HashMap<String, Instant>,
    /// Each session's expiry, by its id.
    sessions: HashMap<String, Instant>,
}

impl Sessions {
    /// No codes and no sessions.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A single-use login code, good for five minutes.
    pub fn issue_code(&self) -> String {
        let code = random_token();
        let mut inner = self.inner.lock().expect("sessions poisoned");
        let now = Instant::now();
        inner.codes.retain(|_, expires| *expires > now);
        inner.codes.insert(code.clone(), now + CODE_LIFETIME);
        code
    }

    /// Redeem a login code for a new session's id, once.
    pub fn redeem(&self, code: &str) -> Option<String> {
        let mut inner = self.inner.lock().expect("sessions poisoned");
        let now = Instant::now();

        match inner.codes.remove(code) {
            Some(expires) if expires > now => Some(inner.open(now)),
            _ => None,
        }
    }

    /// A new session at once, for a script to name as a bearer token.
    pub fn issue_token(&self) -> String {
        let mut inner = self.inner.lock().expect("sessions poisoned");
        inner.open(Instant::now())
    }

    /// Whether a cookie value or bearer token names a live session.
    pub fn live(&self, id: &str) -> bool {
        let inner = self.inner.lock().expect("sessions poisoned");

        inner
            .sessions
            .get(id)
            .is_some_and(|expires| *expires > Instant::now())
    }
}

impl Inner {
    fn open(&mut self, now: Instant) -> String {
        self.sessions.retain(|_, expires| *expires > now);
        let id = random_token();
        self.sessions.insert(id.clone(), now + SESSION_LIFETIME);
        id
    }
}

/// 32 random bytes from the kernel, as hex.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .expect("the kernel's random source is readable");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The fields of a query string, decoded.
pub fn query_fields(query: &str) -> HashMap<String, String> {
    let decode = |text: &str| -> String {
        let text = text.replace('+', " ");
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;

        while index < bytes.len() {
            if bytes[index] == b'%'
                && let Some(hex) = text.get(index + 1..index + 3)
                && let Ok(byte) = u8::from_str_radix(hex, 16)
            {
                out.push(byte);
                index += 3;
                continue;
            }

            out.push(bytes[index]);
            index += 1;
        }

        String::from_utf8_lossy(&out).into_owned()
    };

    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode(key), decode(value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_signs_in_once() {
        let sessions = Sessions::new();
        let code = sessions.issue_code();
        assert_eq!(code.len(), 64);

        let id = sessions.redeem(&code).expect("a fresh code");
        assert!(sessions.redeem(&code).is_none(), "codes are single-use");
        assert!(sessions.redeem("made-up").is_none());
        assert!(sessions.live(&id));
        assert!(!sessions.live("made-up"));
        assert!(!sessions.live(&code), "a code is not a session");
        assert!(sessions.live(&sessions.issue_token()));
    }

    #[test]
    fn queries_decode() {
        let fields = query_fields("release=shop%40r1&note=a+b%2Fc");
        assert_eq!(fields["release"], "shop@r1");
        assert_eq!(fields["note"], "a b/c");
    }
}
