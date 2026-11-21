//! Files opened beneath a root directory (design specification section
//! 4.9): the edge's static files and file hints by path, and the path
//! hints a worker opens for its application.
//!
//! A root is a directory opened once and held. Its own path, and the
//! directories above it, are followed with ordinary resolution when it is
//! opened, since they are the operator's choice: a root may be a symbolic
//! link to another disk. Each use compares the path's device and inode
//! with the held directory's, and opens it again when they differ, so a
//! root moved away is never served from after it has gone.
//!
//! What lies beneath the root is resolved from the held descriptor alone,
//! never by a path that was checked earlier, so nothing that can write in
//! the root can swap a component for a link between a check and the
//! open. On Linux that is `openat2` with `RESOLVE_BENEATH` and
//! `RESOLVE_NO_MAGICLINKS`; elsewhere, and on a kernel without `openat2`,
//! the components are walked one at a time with `openat` and
//! `O_NOFOLLOW`. Either way a symbolic link is followed when its target
//! stays beneath the root, and a path is refused when it climbs out, by
//! `..` or by an absolute link or one that climbs out. The file comes
//! back open, so its type, length and times are asked of the descriptor.
//!
//! A root Muru provides by default rather than one an operator names can
//! be marked as owned by a user: it must resolve to a directory that user
//! owns, so an application cannot point it at a directory it may not
//! read and have another user's process read it on its behalf.

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// How many symbolic links the walk follows in one path before giving
/// up, as the kernel does (`MAXSYMLINKS` on Linux).
const MAX_LINKS: usize = 40;

/// Why a path beneath a root was not opened.
#[derive(Debug)]
pub enum Failure {
    /// The path leaves the root: it is relative, lies under no root, or
    /// climbs out by `..`, an absolute symbolic link, a link whose target
    /// climbs out, a magic link or too many links.
    Escapes,
    /// Nothing is there, or a component on the way is not a directory.
    Missing,
    /// The root cannot be used: it cannot be opened or is not a
    /// directory. The text says which.
    Root(String),
    /// The root is one Muru provides, and its directory is not owned by
    /// the user it must be. The text says whose it is.
    NotOwned(String),
    /// The file is there but could not be opened, such as for want of
    /// permission.
    Unreadable(io::Error),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Escapes => formatter.write_str("the path leaves its root"),
            Self::Missing => formatter.write_str("nothing is there"),
            Self::Root(text) | Self::NotOwned(text) => formatter.write_str(text),
            Self::Unreadable(error) => write!(formatter, "{error}"),
        }
    }
}

/// A file opened beneath a root, with what `fstat` says of it.
#[derive(Debug)]
pub struct Opened {
    /// The open file, read-only.
    pub file: File,
    /// Its type, length and times, from the descriptor.
    pub metadata: std::fs::Metadata,
}

/// A directory files are opened beneath, held open once it has been
/// opened.
#[derive(Debug)]
pub struct Root {
    path: PathBuf,
    owner: Option<u32>,
    held: Mutex<Option<Arc<Held>>>,
}

/// The directory a root's path led to when it was opened.
#[derive(Debug)]
struct Held {
    directory: OwnedFd,
    device: u64,
    inode: u64,
    /// The path the root resolved to, for matching a hint that names a
    /// file by the resolved path rather than the configured one.
    resolved: PathBuf,
}

impl PartialEq for Root {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.owner == other.owner
    }
}

impl Eq for Root {}

impl Root {
    /// A root an operator names: its path is trusted as configured.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            owner: None,
            held: Mutex::new(None),
        }
    }

    /// A root Muru provides by default, which must resolve to a directory
    /// owned by `uid`.
    pub fn owned_by(path: impl Into<PathBuf>, uid: u32) -> Self {
        Self {
            owner: Some(uid),
            ..Self::new(path)
        }
    }

    /// The root's path, as configured.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The user the root's directory must be owned by, for a root Muru
    /// provides.
    pub fn owner(&self) -> Option<u32> {
        self.owner
    }

    /// The path the root resolved to when it was last opened, opening it
    /// when it is not held or has moved.
    pub fn resolved(&self) -> Result<PathBuf, Failure> {
        Ok(self.current()?.resolved.clone())
    }

    /// Open `rest`, a path relative to the root, beneath it: read-only,
    /// with the metadata from the descriptor. An empty `rest` opens the
    /// root itself.
    pub fn open(&self, rest: &Path) -> Result<Opened, Failure> {
        let held = self.current()?;
        let file = open_beneath(held.directory.as_fd(), rest)?;
        let metadata = file.metadata().map_err(Failure::Unreadable)?;
        Ok(Opened { file, metadata })
    }

    /// The held directory, opened again when the root's path now leads
    /// elsewhere (one `stat` of the path), or for the first time.
    fn current(&self) -> Result<Arc<Held>, Failure> {
        let now = std::fs::metadata(&self.path);

        {
            let held = self.held.lock().expect("root poisoned");

            if let (Some(held), Ok(now)) = (held.as_ref(), &now)
                && held.device == now.dev()
                && held.inode == now.ino()
            {
                return Ok(Arc::clone(held));
            }
        }

        if let Err(error) = now {
            *self.held.lock().expect("root poisoned") = None;
            return Err(Failure::Root(format!("{}: {error}", self.path.display())));
        }

        let opened = Arc::new(self.open_root()?);
        *self.held.lock().expect("root poisoned") = Some(Arc::clone(&opened));
        Ok(opened)
    }

    /// Open the root's directory by its path, followed normally, and check
    /// it is a directory owned by the user it must be.
    fn open_root(&self) -> Result<Held, Failure> {
        let unusable = |error: &dyn std::fmt::Display| {
            Failure::Root(format!("{}: {error}", self.path.display()))
        };

        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(&self.path)
            .map_err(|error| unusable(&error))?;
        let metadata = file.metadata().map_err(|error| unusable(&error))?;

        if !metadata.is_dir() {
            return Err(unusable(&"not a directory"));
        }

        if let Some(owner) = self.owner
            && metadata.uid() != owner
        {
            return Err(Failure::NotOwned(format!(
                "{}: owned by uid {}, not uid {owner}",
                self.path.display(),
                metadata.uid()
            )));
        }

        let resolved = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());

        Ok(Held {
            directory: OwnedFd::from(file),
            device: metadata.dev(),
            inode: metadata.ino(),
            resolved,
        })
    }
}

/// The root a hinted path lies under, and the rest of the path below it.
/// The path must be absolute; it is matched by its leading components
/// against each root's configured path and then, where none matches,
/// against the path each root resolved to, the longest match first. A
/// relative path, or one under no root, is [`Failure::Escapes`].
pub fn locate<'a>(roots: &'a [Arc<Root>], path: &Path) -> Result<(&'a Root, PathBuf), Failure> {
    if !path.is_absolute() {
        return Err(Failure::Escapes);
    }

    let lexical = |root: &Path| path.strip_prefix(root).ok().map(Path::to_path_buf);

    let by_name = roots
        .iter()
        .filter_map(|root| Some((root, lexical(&root.path)?)))
        .max_by_key(|(root, _)| root.path.components().count());

    if let Some((root, rest)) = by_name {
        return Ok((root, rest));
    }

    // A path that names the file by where the root leads, as an
    // application that resolved the root's symbolic link would send.

    roots
        .iter()
        .filter_map(|root| {
            let resolved = root.resolved().ok()?;
            let rest = lexical(&resolved)?;
            Some((resolved.components().count(), root, rest))
        })
        .max_by_key(|(depth, _, _)| *depth)
        .map(|(_, root, rest)| (root.as_ref(), rest))
        .ok_or(Failure::Escapes)
}

/// The flags every file beneath a root is opened with: read-only, never a
/// controlling terminal, and without waiting for a FIFO's writer.
const FLAGS: libc::c_int = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NONBLOCK;

/// Open `rest` beneath the directory: with `openat2` on Linux, falling
/// back to the walk where the kernel lacks it.
pub fn open_beneath(directory: BorrowedFd<'_>, rest: &Path) -> Result<File, Failure> {
    #[cfg(target_os = "linux")]
    {
        if let Some(opened) = openat2::open(directory, rest) {
            return opened;
        }
    }

    walk(directory, rest)
}

/// Open `rest` beneath the directory by walking its components with
/// `openat` and `O_NOFOLLOW`, following a symbolic link only when its
/// target is relative, and keeping every directory passed on the way, so
/// a `..`, from the path or a link, goes back to the directory it came
/// from and never above the root.
pub fn walk(directory: BorrowedFd<'_>, rest: &Path) -> Result<File, Failure> {
    let mut pending: std::collections::VecDeque<OsString> = components(rest)?.into();
    let mut passed: Vec<OwnedFd> = Vec::new();
    let mut links = 0;

    if pending.is_empty() {
        return openat(directory, OsStr::new("."), FLAGS | libc::O_DIRECTORY)
            .map(File::from)
            .map_err(missing_or_unreadable);
    }

    while let Some(name) = pending.pop_front() {
        let at = passed.last().map_or(directory, |fd| fd.as_fd());

        if name == ".." {
            if passed.pop().is_none() {
                return Err(Failure::Escapes);
            }

            if pending.is_empty() {
                let at = passed.last().map_or(directory, |fd| fd.as_fd());

                return openat(at, OsStr::new("."), FLAGS | libc::O_DIRECTORY)
                    .map(File::from)
                    .map_err(missing_or_unreadable);
            }

            continue;
        }

        let last = pending.is_empty();
        let flags = FLAGS | libc::O_NOFOLLOW | if last { 0 } else { libc::O_DIRECTORY };

        match openat(at, &name, flags) {
            Ok(fd) if last => return Ok(File::from(fd)),
            Ok(fd) => passed.push(fd),
            Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                // A symbolic link, or a component that is not a directory:
                // only the link is followed, and only by a relative target.

                let Ok(target) = readlinkat(at, &name) else {
                    return Err(missing_or_unreadable(error));
                };

                links += 1;

                if links > MAX_LINKS || Path::new(&target).is_absolute() {
                    return Err(Failure::Escapes);
                }

                for component in components(Path::new(&target))?.into_iter().rev() {
                    pending.push_front(component);
                }

                // A link that leads nowhere further names its directory.

                if pending.is_empty() {
                    return openat(at, OsStr::new("."), FLAGS | libc::O_DIRECTORY)
                        .map(File::from)
                        .map_err(missing_or_unreadable);
                }
            }
            Err(error) => return Err(missing_or_unreadable(error)),
        }
    }

    unreachable!("the last component returns")
}

/// A path's components as names and `..`, with `.` and empty ones
/// dropped; an absolute path cannot lie beneath a root.
fn components(path: &Path) -> Result<Vec<OsString>, Failure> {
    let mut names = Vec::new();

    for component in path.components() {
        match component {
            Component::Normal(name) => names.push(name.to_owned()),
            Component::ParentDir => names.push(OsString::from("..")),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return Err(Failure::Escapes),
        }
    }

    Ok(names)
}

/// An error opening a component, as the caller sees it.
fn missing_or_unreadable(error: io::Error) -> Failure {
    match error.raw_os_error() {
        Some(libc::ENOENT | libc::ENOTDIR | libc::ENAMETOOLONG) => Failure::Missing,
        _ => Failure::Unreadable(error),
    }
}

/// `openat(2)` of one name.
fn openat(directory: BorrowedFd<'_>, name: &OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    let name =
        CString::new(name.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))?;

    // SAFETY: the descriptor is open for the borrow and the name is
    // NUL-terminated; the mode is ignored without O_CREAT.

    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0) };

    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: openat returned a new descriptor that nothing else owns.

    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `readlinkat(2)` of one name: the link's target.
fn readlinkat(directory: BorrowedFd<'_>, name: &OsStr) -> io::Result<OsString> {
    let name =
        CString::new(name.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))?;
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];

    // SAFETY: the descriptor is open for the borrow, the name is
    // NUL-terminated and the buffer is as long as its length says.

    let length = unsafe {
        libc::readlinkat(
            directory.as_raw_fd(),
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };

    if length < 0 {
        return Err(io::Error::last_os_error());
    }

    buffer.truncate(length as usize);
    Ok(OsString::from_vec(buffer))
}

#[cfg(target_os = "linux")]
mod openat2 {
    //! `openat2(2)` with `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`, when the
    //! kernel has it (Linux 5.6 and later).

    use std::ffi::CString;
    use std::fs::File;
    use std::os::fd::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{FLAGS, Failure, missing_or_unreadable};

    /// Set once the kernel has said it has no `openat2`, so the walk is
    /// used from then on without asking again.
    pub(super) static MISSING: AtomicBool = AtomicBool::new(false);

    /// How often a resolution that a rename beside it disturbed is tried
    /// again before the path is refused.
    const RETRIES: usize = 3;

    /// Open `rest` beneath the directory, or none when the kernel has no
    /// `openat2`, for the caller to walk instead.
    pub(super) fn open(directory: BorrowedFd<'_>, rest: &Path) -> Option<Result<File, Failure>> {
        if MISSING.load(Ordering::Relaxed) {
            return None;
        }

        let text = if rest.as_os_str().is_empty() {
            CString::new(".")
        } else {
            CString::new(rest.as_os_str().as_bytes())
        };

        let Ok(path) = text else {
            return Some(Err(Failure::Missing));
        };

        // An absolute path is refused by RESOLVE_BENEATH itself. The type
        // is non-exhaustive, so it cannot be written as a literal.
        //
        // SAFETY: an all-zero open_how is valid (no flags, no mode, no
        // resolve flags); the fields that matter are set below.

        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = FLAGS as u64;
        how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS;

        for _ in 0..RETRIES {
            // SAFETY: the descriptor is open for the borrow, the path is
            // NUL-terminated and open_how is passed with its own size, as
            // the system call requires.

            let fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    directory.as_raw_fd(),
                    path.as_ptr(),
                    &raw const how,
                    std::mem::size_of::<libc::open_how>(),
                )
            };

            if fd >= 0 {
                // SAFETY: openat2 returned a new descriptor that nothing
                // else owns.

                return Some(Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd as i32) })));
            }

            let error = std::io::Error::last_os_error();

            match error.raw_os_error() {
                Some(libc::EAGAIN) => continue,
                Some(libc::ENOSYS) => {
                    MISSING.store(true, Ordering::Relaxed);
                    return None;
                }
                Some(libc::EXDEV | libc::ELOOP) => return Some(Err(Failure::Escapes)),
                _ => return Some(Err(missing_or_unreadable(error))),
            }
        }

        Some(Err(Failure::Escapes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A root with a file, a nested file, links that stay and links that
    /// leave, and a file outside it.
    fn layout() -> (tempfile::TempDir, PathBuf) {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        std::fs::create_dir_all(root.join("nested/deeper")).unwrap();
        std::fs::write(root.join("file.txt"), b"file").unwrap();
        std::fs::write(root.join("nested/inner.txt"), b"inner").unwrap();
        std::fs::write(base.path().join("secret.txt"), b"secret").unwrap();
        symlink("nested/inner.txt", root.join("inside")).unwrap();
        symlink("../file.txt", root.join("nested/up")).unwrap();
        symlink("deeper/../../file.txt", root.join("nested/around")).unwrap();
        symlink("../secret.txt", root.join("outside")).unwrap();
        symlink(base.path().join("root/file.txt"), root.join("absolute")).unwrap();
        symlink("nested", root.join("directory")).unwrap();
        symlink("loop", root.join("loop")).unwrap();
        (base, root)
    }

    fn read(opened: Result<Opened, Failure>) -> String {
        let mut text = String::new();
        io::Read::read_to_string(&mut opened.unwrap().file, &mut text).unwrap();
        text
    }

    /// Both ways of resolving, so the walk is tested on Linux too.
    fn both(root: &Path, rest: &str) -> [Result<File, Failure>; 2] {
        let directory = File::open(root).unwrap();
        [
            open_beneath(directory.as_fd(), Path::new(rest)),
            walk(directory.as_fd(), Path::new(rest)),
        ]
    }

    #[test]
    fn files_beneath_the_root_open_and_links_that_stay_beneath_are_followed() {
        let (_base, root) = layout();
        let held = Root::new(&root);

        assert_eq!(read(held.open(Path::new("file.txt"))), "file");
        assert_eq!(read(held.open(Path::new("nested/inner.txt"))), "inner");

        for rest in [
            "inside",
            "nested/up",
            "nested/around",
            "directory/inner.txt",
        ] {
            for opened in both(&root, rest) {
                assert!(opened.is_ok(), "{rest}: {opened:?}");
            }
        }

        // A `..` that stays beneath the root is allowed, as RESOLVE_BENEATH
        // allows it.

        for opened in both(&root, "nested/../file.txt") {
            assert!(opened.is_ok());
        }

        assert!(held.open(Path::new("")).unwrap().metadata.is_dir());

        // A Linux kernel of 5.6 or later resolved these with openat2.

        #[cfg(target_os = "linux")]
        assert!(!openat2::MISSING.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn paths_and_links_that_leave_the_root_are_refused() {
        let (_base, root) = layout();

        for rest in [
            "../secret.txt",
            "outside",
            "absolute",
            "nested/../../secret.txt",
            "loop",
            "/etc/passwd",
        ] {
            for opened in both(&root, rest) {
                assert!(
                    matches!(opened, Err(Failure::Escapes)),
                    "{rest}: {opened:?}"
                );
            }
        }

        for rest in ["nowhere.txt", "file.txt/below", "nested/nowhere"] {
            for opened in both(&root, rest) {
                assert!(
                    matches!(opened, Err(Failure::Missing)),
                    "{rest}: {opened:?}"
                );
            }
        }
    }

    #[test]
    fn a_root_that_is_a_link_is_followed_and_reopened_when_it_moves() {
        let (base, root) = layout();
        let link = base.path().join("link");
        symlink(&root, &link).unwrap();
        let held = Root::new(&link);
        assert_eq!(read(held.open(Path::new("file.txt"))), "file");
        assert_eq!(held.resolved().unwrap(), root.canonicalize().unwrap());

        // The operator moves the root to another directory and points the
        // link there: the next use opens the new one.

        let moved = base.path().join("moved");
        std::fs::create_dir(&moved).unwrap();
        std::fs::write(moved.join("file.txt"), b"moved").unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&moved, &link).unwrap();
        assert_eq!(read(held.open(Path::new("file.txt"))), "moved");

        // A root that has gone is not served from.

        std::fs::remove_file(&link).unwrap();
        assert!(matches!(
            held.open(Path::new("file.txt")),
            Err(Failure::Root(_))
        ));
    }

    #[test]
    fn a_root_muru_provides_must_be_owned_by_its_user() {
        let (_base, root) = layout();

        // SAFETY: getuid cannot fail.

        let uid = unsafe { libc::getuid() };
        assert!(
            Root::owned_by(&root, uid)
                .open(Path::new("file.txt"))
                .is_ok()
        );

        let refused = Root::owned_by(&root, uid.wrapping_add(1)).open(Path::new("file.txt"));
        assert!(
            matches!(&refused, Err(Failure::NotOwned(text)) if text.contains("owned by")),
            "{refused:?}"
        );

        assert!(matches!(
            Root::new(root.join("file.txt")).open(Path::new("")),
            Err(Failure::Root(_))
        ));
    }

    #[test]
    fn a_hint_is_matched_to_the_longest_root_by_name_or_by_where_it_leads() {
        let (base, root) = layout();
        let link = base.path().join("link");
        symlink(&root, &link).unwrap();
        let roots = vec![
            Arc::new(Root::new(&link)),
            Arc::new(Root::new(link.join("nested"))),
        ];

        let (found, rest) = locate(&roots, &link.join("nested/inner.txt")).unwrap();
        assert_eq!(found.path(), link.join("nested"));
        assert_eq!(rest, Path::new("inner.txt"));

        let (found, rest) = locate(&roots, &root.canonicalize().unwrap().join("file.txt")).unwrap();
        assert_eq!(found.path(), link);
        assert_eq!(rest, Path::new("file.txt"));

        assert!(matches!(
            locate(&roots, Path::new("file.txt")),
            Err(Failure::Escapes)
        ));
        assert!(matches!(
            locate(&roots, &base.path().join("secret.txt")),
            Err(Failure::Escapes)
        ));
    }
}
