//! The archive a bundle is written as: a gzip-compressed tar of the parts
//! collected, under one directory, with a manifest that lists every part
//! and says of each whether it was collected and, when it was not, why.
//!
//! The layout is one thing, so that whatever writes an archive of this
//! kind is read the same way: the paths of [`layout`] are where each kind
//! of evidence goes.
//!
//! An archive is written as a stream, part by part, so it can go to a
//! pipe as well as to a file, and nothing is held in memory or on disk
//! beside it. A part whose size is known before it is read, a stretch of
//! a log say, is copied straight in.

use std::io::{self, Read, Write};

use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Serialize;

use super::window::Window;

/// The version of the layout, raised when a reader would have to change.
pub const FORMAT: u32 = 1;

/// Where each kind of evidence goes in an archive, below its one
/// directory.
pub mod layout {
    /// What the archive holds, and what it could not.
    pub const MANIFEST: &str = "manifest.json";
    /// How Muru runs, its version and the platform.
    pub const INSTALLATION: &str = "installation.json";
    /// The configuration in force: every release's settings, the
    /// daemon's own file, and what is in force outside configuration.
    pub const CONFIG: &str = "config";
    /// Every event of the window, a JSON object a line.
    pub const EVENTS: &str = "events.jsonl";
    /// The logs of the window, by their names in the log directory.
    pub const LOGS: &str = "logs";
    /// What the workers were doing when the archive was made.
    pub const LIVE: &str = "live";
    /// Metrics and what they diagnose.
    pub const FIGURES: &str = "figures";
    /// The machine, and what each release's environment holds.
    pub const HOST: &str = "host";
    /// What went wrong before: timeouts, kills and recycles.
    pub const FAILURES: &str = "failures";
    /// The incidents of the window, each in a directory named by its id
    /// and laid out as an archive of its own is.
    pub const INCIDENTS: &str = "incidents";
}

/// One part of an archive, as the manifest lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// Its path below the archive's directory.
    pub path: String,
    /// Whether it is in the archive.
    pub collected: bool,
    /// Its size before compression, when it is in the archive or its
    /// size is known beforehand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Why it is not in the archive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// What a reader should know about it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What an archive says of itself, written as its last member.
#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    /// The version of the layout.
    pub format: u32,
    /// What made the archive: `bundle`.
    pub kind: String,
    /// When it was made, as RFC 3339 in UTC.
    pub created: String,
    /// Muru's version.
    pub muru: String,
    /// The machine's name.
    pub node: String,
    /// What it was limited to, as `app` or `app@release`; empty for the
    /// whole node.
    pub target: String,
    /// The period it covers.
    pub window: Window,
    /// How Muru runs: `a user daemon`, `muructl run` and so on.
    pub runs: String,
    /// Whether it was running when the archive was made.
    pub running: bool,
    /// The options that decide what it holds, by name.
    pub options: serde_json::Value,
    /// Whether the configuration changed during the window, when the
    /// events could be read to tell. The archive holds the configuration
    /// as it was when it was made.
    pub configuration_changed: Option<bool>,
    /// How the making of the archive was recorded on the system it is
    /// of, or why it was not.
    pub recorded: String,
    /// What an archive never holds, and what was taken out of this one.
    pub left_out: Vec<String>,
    /// Every part: those in the archive, and those that could not be
    /// collected, each with why.
    pub parts: Vec<Entry>,
}

/// A source read as exactly so many bytes: one that ends early, as a
/// file cut while it is read does, is made up with line ends, since the
/// size of a member is fixed before its bytes are written.
struct Exactly<R> {
    inner: R,
    left: u64,
    /// Why the source ended early, when it did.
    short: Option<String>,
}

impl<R: Read> Read for Exactly<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let most = buffer
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));

        if most == 0 {
            return Ok(0);
        }

        if self.short.is_none() {
            match self.inner.read(&mut buffer[..most]) {
                Ok(0) => self.short = Some("it ended before its size was read".to_owned()),
                Ok(read) => {
                    self.left -= read as u64;
                    return Ok(read);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
                Err(error) => self.short = Some(format!("reading it failed: {error}")),
            }
        }

        buffer[..most].fill(b'\n');
        self.left -= most as u64;
        Ok(most)
    }
}

/// An archive being written, or with `dry` only planned: the same parts
/// are offered either way, and a planned one records what it would hold
/// without holding it.
pub struct Archive<W: Write> {
    /// The tar stream, or none when the archive is only planned.
    tar: Option<tar::Builder<GzEncoder<W>>>,
    /// The directory every part is under.
    root: String,
    /// The time every member carries, in Unix seconds.
    made: u64,
    entries: Vec<Entry>,
}

impl Archive<io::Sink> {
    /// An archive that is only planned: its parts are listed with their
    /// sizes, and nothing is written.
    pub fn planned(root: &str) -> Self {
        Self {
            tar: None,
            root: root.to_owned(),
            made: 0,
            entries: Vec::new(),
        }
    }
}

impl<W: Write> Archive<W> {
    /// An archive written to `writer` as it is collected, every part
    /// under the directory `root` and stamped `made_unix_s`. Compression
    /// is the fastest, since an archive is made on a system that may
    /// have little to spare.
    pub fn new(writer: W, root: &str, made_unix_s: u64) -> Self {
        Self {
            tar: Some(tar::Builder::new(GzEncoder::new(
                writer,
                Compression::fast(),
            ))),
            root: root.to_owned(),
            made: made_unix_s,
            entries: Vec::new(),
        }
    }

    /// Whether the archive is only planned.
    pub fn is_planned(&self) -> bool {
        self.tar.is_none()
    }

    /// The parts so far.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Add a part whose content is in hand.
    pub fn bytes(&mut self, path: &str, content: &[u8], note: Option<String>) -> io::Result<()> {
        self.stream(path, content.len() as u64, content, note)
    }

    /// Add a part of `size` bytes read from `source`, such as a stretch
    /// of a file. Exactly `size` bytes are written: a source that ends
    /// early or fails is made up with line ends and the part says so,
    /// so that only writing the archive itself can fail here.
    pub fn stream(
        &mut self,
        path: &str,
        size: u64,
        source: impl Read,
        note: Option<String>,
    ) -> io::Result<()> {
        let mut entry = Entry {
            path: path.to_owned(),
            collected: true,
            bytes: Some(size),
            why: None,
            note,
        };

        if let Some(tar) = &mut self.tar {
            let mut source = Exactly {
                inner: source,
                left: size,
                short: None,
            };
            let mut header = tar::Header::new_gnu();
            header.set_size(size);
            header.set_mode(0o600);
            header.set_mtime(self.made);
            header.set_entry_type(tar::EntryType::Regular);
            tar.append_data(&mut header, format!("{}/{path}", self.root), &mut source)?;

            if let Some(short) = source.short {
                let said = entry.note.take().map_or(String::new(), |note| note + "; ");
                entry.note = Some(format!("{said}{short}, and the rest is line ends"));
            }
        }

        self.entries.push(entry);
        Ok(())
    }

    /// Record a part that would be asked for when the archive is made,
    /// whose size is not known beforehand. Only a planned archive lists
    /// parts this way.
    pub fn would_ask(&mut self, path: &str, note: &str) {
        self.entries.push(Entry {
            path: path.to_owned(),
            collected: true,
            bytes: None,
            why: None,
            note: Some(note.to_owned()),
        });
    }

    /// Record a part that could not be collected, and why.
    pub fn missing(&mut self, path: &str, why: impl std::fmt::Display) {
        self.entries.push(Entry {
            path: path.to_owned(),
            collected: false,
            bytes: None,
            why: Some(why.to_string()),
            note: None,
        });
    }

    /// How many bytes the parts hold before compression, and about how
    /// many the archive takes: a part that is compressed already, as a
    /// rotated log is, at its size, and text at a quarter of its own,
    /// which logs and JSON compress well within.
    pub fn sizes(&self) -> (u64, u64) {
        let mut raw = 0;
        let mut about = 64 * 1024;

        for entry in &self.entries {
            let bytes = entry.bytes.unwrap_or(0);
            raw += bytes;

            about += if entry.path.ends_with(".gz") || entry.path.ends_with(".zst") {
                bytes
            } else {
                bytes / 4
            };
        }

        (raw, about)
    }

    /// Write the manifest, with every part listed, as the last member,
    /// and end the archive, giving back what it was written to; nothing
    /// for a planned one. The manifest's own parts are replaced.
    pub fn finish(mut self, mut manifest: Manifest) -> io::Result<Option<W>> {
        manifest.parts = std::mem::take(&mut self.entries);
        let text = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
        self.bytes(layout::MANIFEST, &text, None)?;

        match self.tar {
            Some(tar) => Ok(Some(tar.into_inner()?.finish()?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use flate2::read::GzDecoder;

    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            format: FORMAT,
            kind: "bundle".to_owned(),
            created: "2026-11-02T05:42:22Z".to_owned(),
            muru: "1.0.0".to_owned(),
            node: "web1".to_owned(),
            target: String::new(),
            window: Window {
                since_unix_ms: 1000,
                until_unix_ms: 2000,
            },
            runs: "a user daemon".to_owned(),
            running: true,
            options: serde_json::json!({"scrub": false}),
            configuration_changed: Some(false),
            recorded: "as the event debug.bundle".to_owned(),
            left_out: vec!["secret values".to_owned()],
            parts: Vec::new(),
        }
    }

    /// The members of an archive, by path, with each one's mode.
    fn members(archive: &[u8]) -> BTreeMap<String, (u32, Vec<u8>)> {
        let mut members = BTreeMap::new();
        let mut tar = tar::Archive::new(GzDecoder::new(archive));

        for member in tar.entries().unwrap() {
            let mut member = member.unwrap();
            let path = member.path().unwrap().display().to_string();
            let mode = member.header().mode().unwrap();
            let mut content = Vec::new();
            member.read_to_end(&mut content).unwrap();
            members.insert(path, (mode, content));
        }

        members
    }

    #[test]
    fn the_parts_are_under_one_directory_with_the_manifest_last() {
        let mut archive = Archive::new(Vec::new(), "muru-bundle-1", 1_793_598_142);
        archive.bytes("installation.json", b"{}\n", None).unwrap();
        archive
            .stream(
                "logs/access.log",
                5,
                &b"hello world"[..],
                Some("the window".to_owned()),
            )
            .unwrap();
        archive.missing("live/shop@r3/stacks-1.json", "no answer within 5 seconds");

        let written = archive.finish(manifest()).unwrap().unwrap();
        let members = members(&written);

        assert_eq!(
            members.keys().collect::<Vec<_>>(),
            [
                "muru-bundle-1/installation.json",
                "muru-bundle-1/logs/access.log",
                "muru-bundle-1/manifest.json",
            ]
        );
        assert_eq!(members["muru-bundle-1/logs/access.log"].1, b"hello");
        assert!(members.values().all(|(mode, _)| *mode == 0o600));

        // The manifest lists what is in the archive and what is not.

        let manifest: serde_json::Value =
            serde_json::from_slice(&members["muru-bundle-1/manifest.json"].1).unwrap();
        assert_eq!(manifest["format"], 1);
        assert_eq!(manifest["kind"], "bundle");
        assert_eq!(manifest["window"]["since_unix_ms"], 1000);
        assert_eq!(
            manifest["parts"],
            serde_json::json!([
                {"path": "installation.json", "collected": true, "bytes": 3},
                {"path": "logs/access.log", "collected": true, "bytes": 5, "note": "the window"},
                {
                    "path": "live/shop@r3/stacks-1.json",
                    "collected": false,
                    "why": "no answer within 5 seconds",
                },
            ])
        );
    }

    #[test]
    fn a_source_that_ends_early_is_made_up_and_said_to_be() {
        struct Failing;

        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("the disk went away"))
            }
        }

        let mut archive = Archive::new(Vec::new(), "b", 0);
        archive
            .stream("logs/short.log", 8, &b"abc\n"[..], None)
            .unwrap();
        archive
            .stream("logs/failing.log", 3, Failing, None)
            .unwrap();
        archive.bytes("after", b"still here", None).unwrap();

        let notes: Vec<Option<String>> = archive
            .entries()
            .iter()
            .map(|entry| entry.note.clone())
            .collect();
        assert_eq!(
            notes,
            [
                Some("it ended before its size was read, and the rest is line ends".to_owned()),
                Some("reading it failed: the disk went away, and the rest is line ends".to_owned()),
                None,
            ]
        );

        let members = members(&archive.finish(manifest()).unwrap().unwrap());
        assert_eq!(members["b/logs/short.log"].1, b"abc\n\n\n\n\n");
        assert_eq!(members["b/logs/failing.log"].1, b"\n\n\n");
        assert_eq!(members["b/after"].1, b"still here");
    }

    #[test]
    fn a_planned_archive_lists_its_parts_and_writes_nothing() {
        let mut archive = Archive::planned("muru-bundle-1");
        assert!(archive.is_planned());

        archive
            .stream("logs/access.log", 4000, io::empty(), None)
            .unwrap();
        archive
            .stream(
                "logs/access.log.20261102-000000.gz",
                1000,
                io::empty(),
                None,
            )
            .unwrap();
        archive.would_ask("live/shop@r3/status.json", "asked of the daemon");
        archive.missing("figures/metrics.txt", "no daemon is running");

        // Text is taken to compress to a quarter, and what is compressed
        // already to stay as it is.

        assert_eq!(archive.sizes(), (5000, 64 * 1024 + 1000 + 1000));
        assert_eq!(archive.entries().len(), 4);
        assert_eq!(archive.entries()[2].bytes, None);
        assert!(archive.finish(manifest()).unwrap().is_none());
    }

    #[test]
    fn a_failure_to_write_the_archive_is_an_error() {
        struct Full;

        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("no space left on device"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut archive = Archive::new(Full, "b", 0);
        let content = vec![b'x'; 4 * 1024 * 1024];
        let written = archive
            .bytes("logs/big.log", &content, None)
            .and_then(|()| archive.finish(manifest()).map(|_| ()));

        assert!(
            written
                .unwrap_err()
                .to_string()
                .contains("no space left on device")
        );
    }
}
