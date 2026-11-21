//! Path hints opened by the worker (design specification section 4.9,
//! issue 848): a response whose headers name a file with `X-Muru-Sendfile`,
//! or the mount's second header, has the header removed, the file opened
//! as the application's user beneath one of the mount's `sendfile_roots`,
//! with the resolution the edge uses (`muru_common::beneath`), and its
//! descriptor sent to the edge on a FILE frame flagged as a path hint's,
//! which the edge serves with static-file behaviour. A precompressed
//! variant beside the file, in the first of the encodings the edge said it
//! would serve one in, goes on a second frame.
//!
//! The roots are not a boundary inside a worker, since code that can open
//! a file can pass it as a descriptor hint; they catch a hint built from a
//! request parameter, and keep what a hint may name one rule whatever
//! answers the route.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use muru_common::beneath::{self, Failure, Root};
use muru_proto::worker::v1 as pb;

/// A mount's path hint settings.
#[derive(Debug, Default)]
pub struct Hints {
    /// The directories a hinted file must lie beneath, absolute, held
    /// open once used; with none every hint is refused.
    pub roots: Vec<Arc<Root>>,
    /// A second header honoured as `X-Muru-Sendfile` is, lower case.
    pub header: Option<String>,
}

impl Hints {
    /// The settings for a mount, from its roots, second header and the
    /// roots Muru provides that must be owned by a user (issue 849), by
    /// path with the uid.
    pub fn new(
        roots: &[String],
        header: Option<&str>,
        owned: &std::collections::HashMap<String, u32>,
    ) -> Self {
        Self {
            roots: roots
                .iter()
                .map(|root| {
                    Arc::new(match owned.get(root) {
                        Some(&uid) => Root::owned_by(PathBuf::from(root), uid),
                        None => Root::new(PathBuf::from(root)),
                    })
                })
                .collect(),
            header: header
                .filter(|name| !name.is_empty())
                .map(str::to_ascii_lowercase),
        }
    }

    /// Take the hint from a response's headers: every hint header is
    /// removed, and the path of the first with a value is returned.
    pub fn take(&self, headers: &mut Vec<(Vec<u8>, Vec<u8>)>) -> Option<String> {
        let mut path = None;

        headers.retain(|(name, value)| {
            let hint = name.eq_ignore_ascii_case(b"x-muru-sendfile")
                || self
                    .header
                    .as_deref()
                    .is_some_and(|header| name.eq_ignore_ascii_case(header.as_bytes()));

            if hint && path.is_none() {
                let text = String::from_utf8_lossy(value).trim().to_owned();
                path = (!text.is_empty()).then_some(text);
            }

            !hint
        });

        path
    }

    /// Open the file a hint names, and the variant beside it in the first
    /// of `encodings` (content-coding tokens) that has one. On the
    /// blocking pool.
    pub fn open(&self, path: &str, encodings: &[String]) -> Result<Opened, pb::PathHintOutcome> {
        let (root, rest) = beneath::locate(&self.roots, Path::new(path))
            .map_err(|_| pb::PathHintOutcome::Refused)?;

        let opened = root
            .open(&rest)
            .map_err(|failure| outcome(path, &failure))?;

        if !opened.metadata.is_file() {
            return Err(pb::PathHintOutcome::Missing);
        }

        let variant = encodings.iter().find_map(|encoding| {
            let suffix = suffix(encoding)?;
            let mut name = rest.as_os_str().to_owned();
            name.push(".");
            name.push(suffix);

            let variant = root.open(Path::new(&name)).ok()?;
            variant
                .metadata
                .is_file()
                .then(|| (encoding.clone(), variant.file))
        });

        Ok(Opened {
            file: opened.file,
            variant,
        })
    }
}

/// A hinted file the worker opened, and its variant when one is taken.
#[derive(Debug)]
pub struct Opened {
    /// The file itself.
    pub file: File,
    /// The precompressed variant, with its content coding.
    pub variant: Option<(String, File)>,
}

/// What the edge is told of a file that was not opened. A refusal is
/// logged by the edge with the path; a file that is there and cannot be
/// read, or a root that cannot be used, is logged here too, since only
/// the worker knows why.
fn outcome(path: &str, failure: &Failure) -> pb::PathHintOutcome {
    match failure {
        Failure::Escapes => pb::PathHintOutcome::Refused,
        Failure::NotOwned(_) => {
            eprintln!("muru: {failure}; the hinted file {path} is refused");
            pb::PathHintOutcome::Refused
        }
        Failure::Missing => pb::PathHintOutcome::Missing,
        Failure::Root(_) | Failure::Unreadable(_) => {
            eprintln!("muru: the hinted file {path} could not be opened: {failure}");
            pb::PathHintOutcome::Missing
        }
    }
}

/// The suffix of a file precompressed in a content coding the edge
/// serves variants in.
fn suffix(encoding: &str) -> Option<&'static str> {
    match encoding {
        "br" => Some("br"),
        "zstd" => Some("zst"),
        "gzip" => Some("gz"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Read as _;

    fn read(file: &mut File) -> String {
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn every_hint_header_is_removed_and_the_first_named_path_taken() {
        let hints = Hints::new(&[], Some("X-Sendfile"), &HashMap::new());
        let mut headers = vec![
            (b"Content-Type".to_vec(), b"text/plain".to_vec()),
            (b"X-Sendfile".to_vec(), b" /srv/a.txt ".to_vec()),
            (b"x-muru-sendfile".to_vec(), b"/srv/b.txt".to_vec()),
        ];
        assert_eq!(hints.take(&mut headers).as_deref(), Some("/srv/a.txt"));
        assert_eq!(
            headers,
            [(b"Content-Type".to_vec(), b"text/plain".to_vec())]
        );

        // Without the second header named, X-Sendfile is an ordinary one.

        let mut headers = vec![(b"X-Sendfile".to_vec(), b"/srv/a.txt".to_vec())];
        assert_eq!(
            Hints::new(&[], None, &HashMap::new()).take(&mut headers),
            None
        );
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn a_hint_opens_beneath_its_root_with_the_variant_the_edge_would_serve() {
        let base = tempfile::tempdir().unwrap();
        let media = base.path().join("media");
        std::fs::create_dir(&media).unwrap();
        std::fs::write(media.join("app.js"), b"plain").unwrap();
        std::fs::write(media.join("app.js.gz"), b"gzipped").unwrap();
        std::fs::write(media.join("app.js.br"), b"brotli").unwrap();
        std::fs::write(base.path().join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink("../secret.txt", media.join("escape")).unwrap();

        let hints = Hints::new(&[media.display().to_string()], None, &HashMap::new());
        let path = media.join("app.js").display().to_string();

        let mut opened = hints
            .open(&path, &["zstd".to_owned(), "gzip".to_owned()])
            .unwrap();
        assert_eq!(read(&mut opened.file), "plain");
        let (encoding, mut variant) = opened.variant.unwrap();
        assert_eq!(
            (encoding.as_str(), read(&mut variant).as_str()),
            ("gzip", "gzipped")
        );
        assert!(hints.open(&path, &[]).unwrap().variant.is_none());

        let outcome = |path: &Path| hints.open(&path.display().to_string(), &[]).unwrap_err();
        assert_eq!(outcome(&media.join("escape")), pb::PathHintOutcome::Refused);
        assert_eq!(
            outcome(&base.path().join("secret.txt")),
            pb::PathHintOutcome::Refused
        );
        assert_eq!(outcome(Path::new("app.js")), pb::PathHintOutcome::Refused);
        assert_eq!(
            outcome(&media.join("nowhere")),
            pb::PathHintOutcome::Missing
        );
        assert_eq!(outcome(&media), pb::PathHintOutcome::Missing);
        assert_eq!(
            Hints::default().open(&path, &[]).unwrap_err(),
            pb::PathHintOutcome::Refused,
            "with no roots every hint is refused"
        );
    }

    #[test]
    fn a_root_muru_provides_must_be_its_users() {
        use std::os::unix::fs::MetadataExt as _;

        let base = tempfile::tempdir().unwrap();
        let media = base.path().join("media");
        std::fs::create_dir(&media).unwrap();
        std::fs::write(media.join("report.pdf"), b"report").unwrap();
        let root = media.display().to_string();
        let path = media.join("report.pdf").display().to_string();
        let uid = std::fs::metadata(&media).unwrap().uid();

        let mine = Hints::new(
            std::slice::from_ref(&root),
            None,
            &HashMap::from([(root.clone(), uid)]),
        );
        assert!(mine.open(&path, &[]).is_ok());

        let another = Hints::new(
            std::slice::from_ref(&root),
            None,
            &HashMap::from([(root.clone(), uid + 1)]),
        );
        assert_eq!(
            another.open(&path, &[]).unwrap_err(),
            pb::PathHintOutcome::Refused
        );
    }
}
