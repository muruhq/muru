//! Files that other processes read while they may be rewritten: records,
//! pid files and the like. They are written whole, so a reader never finds
//! one empty or half written.

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Write `contents` to `path` whole or not at all: to a file beside it,
/// then renamed over it, so a reader that opens `path` finds the old
/// contents or the new, never an empty or a half-written file.
///
/// The file beside it is named for this write alone, so two writers of the
/// same file, in one process or in two, cannot rename each other's
/// half-written file into place. It is removed when the write fails.
pub fn write_whole(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    write_beside(path, contents.as_ref(), None)
}

/// [`write_whole`], with the file given `mode` before it is renamed into
/// place, whatever the umask, so that a reader running as another user can
/// read it from the moment it is there.
pub fn write_whole_with_mode(
    path: &Path,
    contents: impl AsRef<[u8]>,
    mode: u32,
) -> std::io::Result<()> {
    write_beside(path, contents.as_ref(), Some(mode))
}

fn write_beside(path: &Path, contents: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let partial = partial_path(path);

    let written =
        write_partial(&partial, contents, mode).and_then(|()| std::fs::rename(&partial, path));

    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }

    written
}

fn write_partial(partial: &Path, contents: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(partial)?;

    file.write_all(contents)?;

    if let Some(mode) = mode {
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }

    Ok(())
}

/// The name the contents of `path` are written under before the rename:
/// `path` with this process's ID and a count of its writes appended.
fn partial_path(path: &Path) -> PathBuf {
    static WRITES: AtomicU64 = AtomicU64::new(0);

    let write = WRITES.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();

    name.push(format!(".{}.{write}.partial", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();

        names.sort();
        names
    }

    #[test]
    fn a_write_replaces_the_file_and_leaves_nothing_beside_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("relay.toml");

        write_whole(&path, "pid = 1\n").unwrap();
        write_whole(&path, "pid = 2\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "pid = 2\n");
        assert_eq!(names(directory.path()), ["relay.toml"]);
    }

    #[test]
    fn a_mode_is_given_whatever_the_umask() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stop-by");

        write_whole_with_mode(&path, "1", 0o604).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o604);
    }

    #[test]
    fn a_failed_write_leaves_nothing_behind() {
        let directory = tempfile::tempdir().unwrap();

        // The rename fails, since a directory is in the way.

        let path = directory.path().join("taken");
        std::fs::create_dir(&path).unwrap();

        assert!(write_whole(&path, "x").is_err());
        assert_eq!(names(directory.path()), ["taken"]);
    }

    #[test]
    fn a_reader_never_finds_the_file_empty() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("relay.toml");

        write_whole(&path, "pid = 0\n").unwrap();

        // Rewrite the file while another thread reads it: under a
        // truncating write, the reader sees it empty within a few rounds.

        let writer = {
            let path = path.clone();

            std::thread::spawn(move || {
                for pid in 1..2000 {
                    write_whole(&path, format!("pid = {pid}\n")).unwrap();
                }
            })
        };

        while !writer.is_finished() {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                text.starts_with("pid = ") && text.ends_with('\n'),
                "{text:?}"
            );
        }

        writer.join().unwrap();
    }
}
