//! Placeholders for what identifies an installation (design specification
//! section 18.6): `muructl debug report` may be posted in public, so the
//! names of applications, virtual hosts and users, the paths and the
//! addresses in it are replaced, each value by the same placeholder
//! wherever it appears.
//!
//! Nothing is hidden or encoded: the person reads the report before
//! posting it, and a placeholder says what kind of thing stood there.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use serde::Serialize;

/// Paths that say nothing about an installation, kept as they are: where
/// an operating system keeps its own files and interpreters.
const SYSTEM_PATHS: [&str; 11] = [
    "/usr/",
    "/bin/",
    "/sbin/",
    "/lib/",
    "/lib64/",
    "/opt/homebrew/",
    "/System/",
    "/Library/Frameworks/",
    "/dev/",
    "/proc/",
    "/sys/",
];

/// The words in a setting's key that mark its value as a secret, as
/// `muructl debug inspect` masks a variable by its name. A key is split
/// into its words first, so `keepalive` is not taken for a key.
const SECRET_WORDS: [&str; 11] = [
    "secret",
    "secrets",
    "token",
    "password",
    "passwd",
    "key",
    "apikey",
    "credential",
    "credentials",
    "auth",
    "dsn",
];

/// Values of one kind, each numbered in the order it was first seen.
#[derive(Debug, Default)]
struct Numbered {
    seen: Vec<String>,
}

impl Numbered {
    /// The number of a value, from 1, the same each time it is asked for.
    fn number(&mut self, value: &str) -> usize {
        if let Some(at) = self.seen.iter().position(|seen| seen == value) {
            return at + 1;
        }

        self.seen.push(value.to_owned());
        self.seen.len()
    }
}

/// How many values of each kind were replaced, for the report to say.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Replaced {
    /// Application names.
    pub applications: usize,
    /// Virtual host and node names.
    pub hosts: usize,
    /// User and group names.
    pub users: usize,
    /// Paths outside Muru's own directories and the system's.
    pub paths: usize,
    /// Network addresses that are not the machine's own loopback.
    pub addresses: usize,
    /// Python modules named as entry points and hooks.
    pub modules: usize,
}

/// A piece of a text on its way through the replacements: still open to
/// them, or already a placeholder that later ones leave alone.
enum Piece {
    Open(String),
    Done(String),
}

/// The replacements for one report.
#[derive(Debug, Default)]
pub struct Redactor {
    /// Muru's own directories, each with the label that stands for it,
    /// the longest path first.
    roots: Vec<(String, String)>,
    /// The home directory.
    home: Option<String>,
    /// Names with the placeholder for each, the longest name first.
    names: Vec<(String, String)>,
    /// The placeholders of names that a text held.
    used: std::collections::BTreeSet<String>,
    apps: Numbered,
    hosts: Numbered,
    users: Numbered,
    paths: Numbered,
    addresses: Numbered,
    modules: Numbered,
}

impl Redactor {
    /// One of Muru's own directories or files, which reads as `label`
    /// with whatever follows it kept: the layout below is Muru's own.
    pub fn root(&mut self, path: &Path, label: &str) {
        let path = path.display().to_string();

        if path.len() > 1 && !self.roots.iter().any(|(known, _)| *known == path) {
            self.roots.push((path, label.to_owned()));
            self.roots
                .sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
        }
    }

    /// The home directory, which reads as `~`.
    pub fn home(&mut self, path: &Path) {
        let path = path.display().to_string();

        if path.len() > 1 {
            self.home = Some(path.trim_end_matches('/').to_owned());
        }
    }

    /// An application's name, which reads as `app1`, `app2` and so on.
    pub fn app(&mut self, name: &str) {
        let placeholder = format!("app{}", self.apps.number(name));
        self.name(name, placeholder);
    }

    /// A virtual host's name, which reads as `host1`, `host2` and so on.
    pub fn host(&mut self, name: &str) {
        let placeholder = format!("host{}", self.hosts.number(name));
        self.name(name, placeholder);
    }

    /// The machine's own name, which reads as `node`.
    pub fn node(&mut self, name: &str) {
        self.name(name, "node".to_owned());
    }

    /// A user's or a group's name, which reads as `user1`, `user2` and
    /// so on.
    pub fn user(&mut self, name: &str) {
        let placeholder = format!("user{}", self.users.number(name));
        self.name(name, placeholder);
    }

    fn name(&mut self, name: &str, placeholder: String) {
        if name.is_empty() || self.names.iter().any(|(known, _)| known == name) {
            return;
        }

        self.names.push((name.to_owned(), placeholder));
        self.names
            .sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    }

    /// How many values of each kind a placeholder was written for. A
    /// name that was known and never met is not counted.
    pub fn replaced(&self) -> Replaced {
        let used = |kind: &str| {
            self.used
                .iter()
                .filter(|placeholder| placeholder.starts_with(kind))
                .count()
        };

        Replaced {
            applications: used("app"),
            hosts: used("host") + used("node"),
            users: used("user"),
            paths: self.paths.seen.len(),
            addresses: self.addresses.seen.len(),
            modules: self.modules.seen.len(),
        }
    }

    /// An entry point or a hooks module, `module:attribute`, with the
    /// module replaced: its dotted path names the application's code.
    pub fn module(&mut self, entrypoint: &str) -> String {
        let (module, attribute) = match entrypoint.split_once(':') {
            Some((module, attribute)) => (module, Some(attribute)),
            None => (entrypoint, None),
        };

        if module.is_empty() {
            return entrypoint.to_owned();
        }

        let placeholder = format!("<module{}>", self.modules.number(module));

        match attribute {
            Some(attribute) => format!("{placeholder}:{attribute}"),
            None => placeholder,
        }
    }

    /// A setting's value by its key: masked whole when the key says it is
    /// a secret, and otherwise as `text` leaves it.
    pub fn value(&mut self, key: &str, value: &str) -> String {
        let secret = key
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| SECRET_WORDS.contains(&word.to_lowercase().as_str()));

        if secret {
            return "<redacted>".to_owned();
        }

        self.text(value)
    }

    /// A text with every path, address and known name in it replaced.
    pub fn text(&mut self, text: &str) -> String {
        let mut pieces = self.paths_and_urls(text);
        pieces = self.each_open(pieces, Self::addresses_in);
        pieces = self.each_open(pieces, Self::names_in);

        pieces
            .into_iter()
            .map(|piece| match piece {
                Piece::Open(text) | Piece::Done(text) => text,
            })
            .collect()
    }

    /// Run one replacement over the pieces still open to it.
    fn each_open(
        &mut self,
        pieces: Vec<Piece>,
        replace: fn(&mut Self, &str) -> Vec<Piece>,
    ) -> Vec<Piece> {
        let mut out = Vec::new();

        for piece in pieces {
            match piece {
                Piece::Open(text) => out.extend(replace(self, &text)),
                done => out.push(done),
            }
        }

        out
    }

    /// The text with its URLs and absolute paths replaced.
    fn paths_and_urls(&mut self, text: &str) -> Vec<Piece> {
        let mut pieces = Vec::new();
        let mut open = String::new();
        let mut at = 0;

        while at < text.len() {
            let rest = &text[at..];
            let boundary = text[..at].chars().next_back().is_none_or(starts_a_value);

            if boundary && let Some(length) = url_length(rest) {
                flush(&mut pieces, &mut open);
                pieces.push(Piece::Done(self.url(&rest[..length])));
                at += length;
                continue;
            }

            if boundary && rest.starts_with('/') {
                // One of Muru's own directories, whose path may hold a
                // space, is matched whole before any path is cut out.

                if let Some((root, label)) = self.root_of(rest) {
                    flush(&mut pieces, &mut open);
                    pieces.push(Piece::Done(label));
                    at += root;

                    // What follows is Muru's own layout, kept for the
                    // names in it to be replaced.

                    let tail = path_length(&text[at..]);
                    open.push_str(&text[at..at + tail]);
                    at += tail;
                    continue;
                }

                let length = path_length(rest);
                let path = &rest[..length];

                if is_path(path) {
                    if SYSTEM_PATHS.iter().any(|prefix| path.starts_with(prefix)) {
                        open.push_str(path);
                    } else {
                        flush(&mut pieces, &mut open);
                        pieces.push(Piece::Done(self.path(path)));
                    }

                    at += length;
                    continue;
                }
            }

            let next = rest.chars().next().map_or(1, char::len_utf8);
            open.push_str(&rest[..next]);
            at += next;
        }

        flush(&mut pieces, &mut open);
        pieces
    }

    /// The root `text` starts with, as its length and its label, when
    /// the root ends where a path component does.
    fn root_of(&self, text: &str) -> Option<(usize, String)> {
        self.roots.iter().find_map(|(root, label)| {
            let after = text.strip_prefix(root.as_str())?;
            let ends = after.trim_end_matches('.').is_empty()
                || after
                    .chars()
                    .next()
                    .is_some_and(|next| next == '/' || ends_a_path(next))
                || after.starts_with(". ");

            ends.then(|| (root.len(), label.clone()))
        })
    }

    /// The placeholder for a path outside Muru's own directories: `~`
    /// for the home directory and `~/<pathN>` below it, else `<pathN>`.
    fn path(&mut self, path: &str) -> String {
        if let Some(home) = &self.home {
            if path == home {
                return "~".to_owned();
            }

            if path.starts_with(home.as_str()) && path[home.len()..].starts_with('/') {
                return format!("~/<path{}>", self.paths.number(path));
            }
        }

        format!("<path{}>", self.paths.number(path))
    }

    /// A URL with its host replaced, its user and password dropped, and
    /// whatever follows the host cut down to a mark that something did.
    fn url(&mut self, url: &str) -> String {
        let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, after) = rest.split_at(end);
        let location = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);

        // The port follows the last colon, unless that colon is inside
        // the brackets of an IPv6 address.

        let (host, port) = match location.rsplit_once(':') {
            Some((host, port)) if !port.contains(']') => (host, Some(port)),
            _ => (location, None),
        };

        let mut out = format!("{scheme}://{}", self.host_in_url(host));

        if let Some(port) = port {
            out.push(':');
            out.push_str(port);
        }

        match after {
            "" => {}
            "/" => out.push('/'),
            _ => out.push_str("/..."),
        }

        out
    }

    /// The host of a URL: a loopback or unspecified address and
    /// `localhost` as they are, any other address or name replaced.
    fn host_in_url(&mut self, host: &str) -> String {
        let bare = host.trim_start_matches('[').trim_end_matches(']');

        if host.is_empty() || bare.eq_ignore_ascii_case("localhost") {
            return host.to_owned();
        }

        if let Ok(address) = bare.parse::<IpAddr>() {
            return self.address(address, host);
        }

        if !self.names.iter().any(|(name, _)| name == host) {
            self.host(host);
        }

        let placeholder = self
            .names
            .iter()
            .find(|(name, _)| name == host)
            .map_or_else(String::new, |(_, placeholder)| placeholder.clone());
        self.used.insert(placeholder.clone());
        placeholder
    }

    /// An address as it reads: the machine's own loopback and the
    /// unspecified address as written, any other as `<addressN>`.
    fn address(&mut self, address: IpAddr, written: &str) -> String {
        if address.is_loopback() || address.is_unspecified() {
            return written.to_owned();
        }

        format!("<address{}>", self.addresses.number(&address.to_string()))
    }

    /// The text with the network addresses in it replaced. An address is
    /// looked for in each run of the characters one is written with.
    fn addresses_in(&mut self, text: &str) -> Vec<Piece> {
        let mut pieces = Vec::new();
        let mut open = String::new();
        let mut at = 0;

        while at < text.len() {
            let rest = &text[at..];
            let run = rest
                .find(|c: char| !(c.is_ascii_hexdigit() || c == ':' || c == '.'))
                .unwrap_or(rest.len());

            // A run inside a word is part of the word, not an address.

            let inside = text[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
                || rest[run..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');

            if run == 0 {
                let next = rest.chars().next().map_or(1, char::len_utf8);
                open.push_str(&rest[..next]);
                at += next;
                continue;
            }

            match (!inside).then(|| address_in(&rest[..run])).flatten() {
                Some((address, length)) => {
                    let written = &rest[..length];
                    let shown = self.address(address, written);

                    if shown == written {
                        open.push_str(written);
                    } else {
                        flush(&mut pieces, &mut open);
                        pieces.push(Piece::Done(shown));
                    }

                    open.push_str(&rest[length..run]);
                }
                None => open.push_str(&rest[..run]),
            }

            at += run;
        }

        flush(&mut pieces, &mut open);
        pieces
    }

    /// The text with every known name replaced where it stands as a word
    /// of its own, the longest name tried first.
    fn names_in(&mut self, text: &str) -> Vec<Piece> {
        let mut pieces = Vec::new();
        let mut open = String::new();
        let mut at = 0;

        while at < text.len() {
            let rest = &text[at..];
            let before = text[..at].chars().next_back();

            let found = self.names.iter().find(|(name, _)| {
                rest.starts_with(name.as_str())
                    && !before.is_some_and(in_a_name)
                    && !rest[name.len()..].chars().next().is_some_and(in_a_name)
            });

            match found {
                Some((name, placeholder)) => {
                    flush(&mut pieces, &mut open);
                    pieces.push(Piece::Done(placeholder.clone()));
                    self.used.insert(placeholder.clone());
                    at += name.len();
                }
                None => {
                    let next = rest.chars().next().map_or(1, char::len_utf8);
                    open.push_str(&rest[..next]);
                    at += next;
                }
            }
        }

        flush(&mut pieces, &mut open);
        pieces
    }
}

/// Move what is gathered in `open` into the pieces.
fn flush(pieces: &mut Vec<Piece>, open: &mut String) {
    if !open.is_empty() {
        pieces.push(Piece::Open(std::mem::take(open)));
    }
}

/// Whether a path or a URL may start after this character: after a
/// space, a quote, a bracket or the punctuation a value follows.
fn starts_a_value(before: char) -> bool {
    before.is_whitespace() || "\"'`([{=,:;".contains(before)
}

/// Whether this character ends a path written in a sentence.
fn ends_a_path(c: char) -> bool {
    c.is_whitespace() || "\"'`<>|,;:)]}".contains(c)
}

/// Whether this character may be part of a name, so that a known name
/// beside it is part of a longer word and left alone.
fn in_a_name(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// How much of the start of `text` is one path: up to what ends a path,
/// less a full stop that ends the sentence.
fn path_length(text: &str) -> usize {
    let end = text.find(ends_a_path).unwrap_or(text.len());
    text[..end].trim_end_matches('.').len()
}

/// Whether a run of characters starting with a slash is a path: more
/// than the slash, and not the `//` of something else.
fn is_path(path: &str) -> bool {
    path.len() > 1 && !path.starts_with("//")
}

/// The length of the URL `text` starts with, if it starts with one: a
/// scheme, `://`, and what follows up to a space, a quote or a bracket.
fn url_length(text: &str) -> Option<usize> {
    if !text.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }

    let scheme = text.find(|c: char| !(c.is_ascii_alphanumeric() || "+.-".contains(c)))?;

    if !text[scheme..].starts_with("://") {
        return None;
    }

    let rest = &text[scheme + 3..];
    let end = rest
        .find(|c: char| c.is_whitespace() || "\"'`<>|,;)}".contains(c))
        .unwrap_or(rest.len());
    let length = scheme + 3 + rest[..end].trim_end_matches('.').len();

    (length > scheme + 3).then_some(length)
}

/// The address a run of digits, hexadecimal letters, colons and full
/// stops starts with, and how much of the run it takes: an IPv4 address,
/// with a port after it left in the run, or an IPv6 address.
fn address_in(run: &str) -> Option<(IpAddr, usize)> {
    if run.matches(':').count() >= 2 {
        // The whole run, or the run less the full stop or colon that
        // follows an address in a sentence.

        let shorter = run.trim_end_matches('.');
        let shorter = shorter.strip_suffix(':').unwrap_or(shorter);

        return [run, shorter].into_iter().find_map(|written| {
            let address = written.parse::<Ipv6Addr>().ok()?;
            Some((IpAddr::V6(address), written.len()))
        });
    }

    let run = run.trim_end_matches(['.', ':']);

    let host = run.split(':').next().unwrap_or(run);

    host.parse::<Ipv4Addr>()
        .ok()
        .map(|address| (IpAddr::V4(address), host.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor() -> Redactor {
        let mut redactor = Redactor::default();
        redactor.home(Path::new("/home/graham"));
        redactor.root(Path::new("/home/graham/.local/state/muru"), "<state-dir>");
        redactor.root(
            Path::new("/Users/graham/Library/Application Support/muru"),
            "<state-dir>",
        );
        redactor.root(Path::new("/tmp/muru-501"), "<run-dir>");
        redactor.app("shop");
        redactor.app("shop-admin");
        redactor.host("shop.example.com");
        redactor.node("grumpy");
        redactor.user("graham");
        redactor
    }

    #[test]
    fn the_same_value_always_reads_as_the_same_placeholder() {
        let mut redactor = redactor();

        assert_eq!(
            redactor.text("release shop@r3 is ready"),
            "release app1@r3 is ready"
        );
        assert_eq!(
            redactor.text("shop-admin@r1 replaced shop@r3"),
            "app2@r1 replaced app1@r3"
        );
        assert_eq!(redactor.text("/srv/acme/shop"), "<path1>");
        assert_eq!(redactor.text("/srv/other"), "<path2>");
        assert_eq!(
            redactor.text("moved /srv/acme/shop to /srv/other, then /srv/acme/shop"),
            "moved <path1> to <path2>, then <path1>"
        );
        assert_eq!(
            redactor.text("203.0.113.9 and 203.0.113.9:443"),
            "<address1> and <address1>:443"
        );

        // A second report gives the same values the same placeholders.

        let mut again = super::tests::redactor();
        assert_eq!(again.text("/srv/acme/shop"), "<path1>");
        assert_eq!(again.text("shop-admin shop"), "app2 app1");
    }

    #[test]
    fn a_name_is_replaced_only_as_a_word_of_its_own() {
        let mut redactor = redactor();

        assert_eq!(redactor.text("workshop"), "workshop");
        assert_eq!(redactor.text("shop_tools"), "shop_tools");
        assert_eq!(redactor.text("apps/shop/r1"), "apps/app1/r1");
        assert_eq!(
            redactor.text("shop.wsgi:application"),
            "app1.wsgi:application"
        );
        assert_eq!(
            redactor.text("host shop.example.com on grumpy for graham"),
            "host host1 on node for user1"
        );
    }

    #[test]
    fn paths_keep_only_what_muru_laid_out() {
        let mut redactor = redactor();

        assert_eq!(redactor.text("/home/graham"), "~");
        assert_eq!(redactor.text("/home/graham/work/acme/app.py"), "~/<path1>");
        assert_eq!(
            redactor.text("/home/graham/.local/state/muru/apps/shop/r3/env"),
            "<state-dir>/apps/app1/r3/env"
        );
        assert_eq!(
            redactor
                .text("reading /Users/graham/Library/Application Support/muru/muru.sqlite: denied"),
            "reading <state-dir>/muru.sqlite: denied"
        );
        assert_eq!(
            redactor.text("socket /tmp/muru-501/admin.sock."),
            "socket <run-dir>/admin.sock."
        );

        // A directory that only starts like one of Muru's is not it.

        assert_eq!(redactor.text("/tmp/muru-5012/x"), "<path2>");

        // The system's own paths say nothing about the installation.

        assert_eq!(
            redactor.text("python = /usr/bin/python3.13"),
            "python = /usr/bin/python3.13"
        );
        assert_eq!(
            redactor.text("File \"/srv/acme/views.py\", line 3, in view"),
            "File \"<path3>\", line 3, in view"
        );
        assert_eq!(redactor.text("5/10 done, n/a"), "5/10 done, n/a");
    }

    #[test]
    fn addresses_other_than_the_machines_own_are_replaced() {
        let mut redactor = redactor();

        assert_eq!(redactor.text("127.0.0.1:8000"), "127.0.0.1:8000");
        assert_eq!(
            redactor.text("0.0.0.0:80 [::1]:8000 [::]:80"),
            "0.0.0.0:80 [::1]:8000 [::]:80"
        );
        assert_eq!(redactor.text("10.1.2.3"), "<address1>");
        assert_eq!(
            redactor.text("trusted 10.0.0.0/8."),
            "trusted <address2>/8."
        );
        assert_eq!(redactor.text("[2001:db8::1]:443"), "[<address3>]:443");
        assert_eq!(
            redactor.text("from fe80::, and 2001:db8::1."),
            "from <address4>, and <address3>."
        );
        assert_eq!(
            redactor.text("at 05:42:22 on Python 3.13.1"),
            "at 05:42:22 on Python 3.13.1"
        );
        assert_eq!(redactor.text("build abc10.1.2.3"), "build abc10.1.2.3");
    }

    #[test]
    fn a_url_keeps_its_scheme_and_port_and_loses_the_rest() {
        let mut redactor = redactor();

        assert_eq!(
            redactor.text("http://127.0.0.1:8000"),
            "http://127.0.0.1:8000"
        );
        assert_eq!(
            redactor.text("on http://localhost:8000/"),
            "on http://localhost:8000/"
        );
        assert_eq!(
            redactor.text("https://shop.example.com/orders?id=7"),
            "https://host1/..."
        );
        assert_eq!(
            redactor.text("posting to https://ops:hunter2@hooks.example.net:8443/t/abc."),
            "posting to https://host2:8443/...."
        );
        assert_eq!(
            redactor.text("http://203.0.113.9:80"),
            "http://<address1>:80"
        );
        assert_eq!(
            redactor.text("http://[2001:db8::1]:80/x"),
            "http://<address2>:80/..."
        );
    }

    #[test]
    fn modules_and_secrets_are_replaced_by_what_they_are() {
        let mut redactor = redactor();

        assert_eq!(
            redactor.module("acme.billing.wsgi:application"),
            "<module1>:application"
        );
        assert_eq!(redactor.module("acme.hooks"), "<module2>");
        assert_eq!(
            redactor.module("acme.billing.wsgi:create"),
            "<module1>:create"
        );
        assert_eq!(redactor.value("webhook.secret", "hunter2"), "<redacted>");
        assert_eq!(redactor.value("tls_key", "/etc/ssl/k.pem"), "<redacted>");
        assert_eq!(redactor.value("database_dsn", "postgres://x"), "<redacted>");
        assert_eq!(redactor.value("processes", "4"), "4");
        assert_eq!(redactor.value("limits.keepalive_timeout", "5s"), "5s");

        // Only what a text held is counted, not every name known.

        let replaced = redactor.replaced();
        assert_eq!(replaced.applications, 0);
        assert_eq!(replaced.modules, 2);

        redactor.text("shop on grumpy and shop.example.com, by graham for shop");
        let replaced = redactor.replaced();
        assert_eq!(replaced.applications, 1);
        assert_eq!(replaced.hosts, 2);
        assert_eq!(replaced.users, 1);
    }
}
