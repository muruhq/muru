//! The logs of a window: which stretch of each file in the log directory
//! a collection takes.
//!
//! A live log is taken from where the window starts in it to its end as
//! it was when it was opened. An archive the rotation left is taken when
//! it was last written in the window: from where the window starts in
//! it when it is plain text, and whole when it is compressed, since the
//! window cannot be found in it without decompressing all of it.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use super::window::{Window, window_start};
use crate::log_files::LogFile;

/// How a file in the log directory is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packed {
    /// gzip, as the `.gz` suffix says.
    Gzip,
    /// zstd, as the `.zst` suffix says.
    Zstd,
}

impl Packed {
    /// How a file of this name is compressed, if it is.
    pub fn of(name: &str) -> Option<Self> {
        if name.ends_with(".gz") {
            Some(Packed::Gzip)
        } else if name.ends_with(".zst") {
            Some(Packed::Zstd)
        } else {
            None
        }
    }

    /// The name of a file less the suffix of its compression.
    pub fn stem(name: &str) -> &str {
        name.strip_suffix(".gz")
            .or_else(|| name.strip_suffix(".zst"))
            .unwrap_or(name)
    }

    /// A reader of what the file holds once decompressed.
    pub fn reader<'a>(self, packed: impl Read + 'a) -> io::Result<Box<dyn Read + 'a>> {
        Ok(match self {
            Packed::Gzip => Box::new(flate2::read::MultiGzDecoder::new(packed)),
            Packed::Zstd => Box::new(zstd::stream::read::Decoder::new(packed)?),
        })
    }
}

/// A stretch of one file in the log directory: the part of it that is in
/// the window. The file is held open, so the stretch is of the file as
/// it was found even if it is rotated away meanwhile.
#[derive(Debug)]
pub struct Stretch {
    /// The file's path relative to the log directory.
    pub name: String,
    /// The file, open for reading.
    pub file: File,
    /// Where the stretch starts in the file.
    pub start: u64,
    /// How many bytes it is.
    pub length: u64,
    /// How the file is compressed, when it is: the stretch is then the
    /// whole file.
    pub packed: Option<Packed>,
    /// Whether the file is an archive the rotation left.
    pub archive: bool,
}

/// The stretches of a log and its archives that are in the window, the
/// oldest first, or for a file that could not be read, its name and why.
/// An archive last written before the window is passed over.
pub fn stretches(
    directory: &Path,
    log: &LogFile,
    window: Window,
) -> Vec<Result<Stretch, (String, String)>> {
    let since = UNIX_EPOCH + Duration::from_millis(window.since_unix_ms);

    let archives = log
        .archives
        .iter()
        .filter(|archive| archive.modified >= since)
        .map(|archive| (archive.name.as_str(), true));
    let live = log.present.then_some((log.name.as_str(), false));

    archives
        .chain(live)
        .map(|(name, archive)| {
            stretch(directory, name, archive, window)
                .map_err(|error| (name.to_owned(), error.to_string()))
        })
        .collect()
}

/// The stretch of one file that is in the window.
fn stretch(directory: &Path, name: &str, archive: bool, window: Window) -> io::Result<Stretch> {
    let mut file = File::open(directory.join(name))?;
    let length = file.metadata()?.len();
    let packed = Packed::of(name);

    let start = match packed {
        Some(_) => 0,
        None => window_start(&mut file, length, window.since_unix_ms)?,
    };

    Ok(Stretch {
        name: name.to_owned(),
        file,
        start,
        length: length - start,
        packed,
        archive,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Seek as _, SeekFrom, Write as _};
    use std::time::SystemTime;

    use super::*;
    use crate::log_files;

    /// 2026-11-02T05:42:22Z.
    const BASE: u64 = 1_793_598_142;

    fn line(second: u64, text: &str) -> String {
        let time = UNIX_EPOCH + Duration::from_secs(BASE + second);
        format!("{} muru: {text}\n", muru_common::log::full(time))
    }

    fn touch(path: &Path, second: u64) {
        let file = File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(BASE + second))
            .unwrap();
    }

    fn read(stretch: &mut Stretch) -> String {
        stretch.file.seek(SeekFrom::Start(stretch.start)).unwrap();
        let mut text = Vec::new();
        (&stretch.file)
            .take(stretch.length)
            .read_to_end(&mut text)
            .unwrap();

        match stretch.packed {
            Some(packed) => {
                let mut plain = String::new();
                packed
                    .reader(&text[..])
                    .unwrap()
                    .read_to_string(&mut plain)
                    .unwrap();
                plain
            }
            None => String::from_utf8(text).unwrap(),
        }
    }

    #[test]
    fn the_window_of_a_log_reaches_into_its_archives() {
        let directory = tempfile::tempdir().unwrap();
        let logs = directory.path();

        // The live log, a plain archive that straddles the start of the
        // window, a compressed one inside it, and two from before it.

        std::fs::write(
            logs.join("edge.log"),
            line(300, "live one") + &line(400, "live two"),
        )
        .unwrap();
        std::fs::write(
            logs.join("edge.log.20261102-054500"),
            line(50, "before the window") + &line(150, "plain, in the window"),
        )
        .unwrap();
        touch(&logs.join("edge.log.20261102-054500"), 160);

        let mut packed = flate2::write::GzEncoder::new(
            File::create(logs.join("edge.log.20261102-054700.gz")).unwrap(),
            flate2::Compression::fast(),
        );
        packed
            .write_all(
                (line(90, "packed, before") + &line(250, "packed, in the window")).as_bytes(),
            )
            .unwrap();
        packed.finish().unwrap();
        touch(&logs.join("edge.log.20261102-054700.gz"), 260);

        for old in ["edge.log.20261102-050000", "edge.log.20261102-051000.zst"] {
            std::fs::write(logs.join(old), line(10, "long before")).unwrap();
            touch(&logs.join(old), 20);
        }

        let window = Window::last(
            Duration::from_secs(400),
            UNIX_EPOCH + Duration::from_secs(BASE + 500),
        );
        let listed = log_files::list(logs, None).unwrap();
        let edge = listed.iter().find(|log| log.name == "edge.log").unwrap();
        assert_eq!(edge.archives.len(), 4);

        let mut found: Vec<Stretch> = stretches(logs, edge, window)
            .into_iter()
            .map(Result::unwrap)
            .collect();
        let named: Vec<(&str, bool, bool)> = found
            .iter()
            .map(|stretch| {
                (
                    stretch.name.as_str(),
                    stretch.archive,
                    stretch.packed.is_some(),
                )
            })
            .collect();
        assert_eq!(
            named,
            [
                ("edge.log.20261102-054500", true, false),
                ("edge.log.20261102-054700.gz", true, true),
                ("edge.log", false, false),
            ]
        );

        // The plain files give only their lines of the window, and the
        // compressed one all of it.

        assert_eq!(read(&mut found[0]), line(150, "plain, in the window"));
        assert_eq!(
            read(&mut found[1]),
            line(90, "packed, before") + &line(250, "packed, in the window")
        );
        assert_eq!(
            read(&mut found[2]),
            line(300, "live one") + &line(400, "live two")
        );
    }

    #[test]
    fn a_file_that_cannot_be_read_is_named_with_why() {
        let directory = tempfile::tempdir().unwrap();
        let gone = LogFile {
            name: "access.log".to_owned(),
            kind: log_files::Kind::Access,
            app: String::new(),
            release: String::new(),
            hosts: Vec::new(),
            size: 10,
            present: true,
            modified: SystemTime::now(),
            archives: Vec::new(),
            orphaned: false,
        };
        let window = Window::last(Duration::from_secs(60), SystemTime::now());
        let found = stretches(directory.path(), &gone, window);

        assert_eq!(found.len(), 1);
        let (name, why) = found.into_iter().next().unwrap().unwrap_err();
        assert_eq!(name, "access.log");
        assert!(why.contains("No such file"), "{why}");
    }

    #[test]
    fn a_files_compression_is_read_from_its_name() {
        assert_eq!(
            Packed::of("access.log.20261102-054700.gz"),
            Some(Packed::Gzip)
        );
        assert_eq!(Packed::of("apps/shop/r1.log.1.zst"), Some(Packed::Zstd));
        assert_eq!(Packed::of("access.log"), None);
        assert_eq!(Packed::stem("access.log.1.zst"), "access.log.1");
        assert_eq!(Packed::stem("access.log"), "access.log");
    }
}
