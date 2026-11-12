//! Process titles (design specification section 2.1): each of Muru's
//! processes says what it is, so `ps`, `top` and `htop` tell them apart,
//! by rewriting the argument area the operating system shows as its
//! command line and, on Linux, setting the short name with
//! `prctl(PR_SET_NAME)`.
//!
//! The area is the strings `exec` placed on the stack, which the standard
//! library reads `std::env::args` from each time it is called. The
//! arguments are kept as they were before the first title, and
//! [`args_os`] gives them to whatever needs them later, such as a daemon
//! that runs itself again for an upgrade. The area is found once, before
//! anything is written, and a forked child inherits it, so a worker's
//! title has the room its host's had.
//!
//! On Linux the kernel shows the area as it is, so the title is followed
//! by NULs. On macOS `ps` reads as many strings as the process had
//! arguments, and `pgrep` skips runs of NULs while it does, so the title
//! is followed by spaces and then one single-space string for each
//! argument after the first, which leaves the environment after them
//! where both look for it.
//!
//! A process started with a short command line, such as `muructl serve`
//! in a container, has too little room for its title. [`reserve`], called
//! while the process is still single-threaded, copies the environment
//! strings that follow the arguments elsewhere and points `environ` at
//! the copies, as `setproctitle` and PostgreSQL do, so a title that does
//! not fit the arguments may run on over them; `std::env` and the
//! processes it starts see the environment as before, and only
//! `/proc/PID/environ`, and `ps -E` on macOS, show the title's bytes in
//! its place. On Linux the kernel shows the area past the arguments when
//! their last byte is no longer a NUL, up to the first NUL and at most a
//! page. A title that still does not fit is cut at a space, so it never
//! ends in the middle of a name.

use std::ffi::OsString;
use std::sync::OnceLock;

/// The arguments the process was started with, kept before the first
/// title replaced them.
static ORIGINAL: OnceLock<Vec<OsString>> = OnceLock::new();

/// The argument area, as its address and length, and how many arguments
/// the process was started with; none where it cannot be found.
static AREA: OnceLock<Option<(usize, usize, usize)>> = OnceLock::new();

/// How many bytes of environment strings follow the argument area, which
/// [`reserve`] moved elsewhere so a title may use them; 0 without it.
static SPARE: OnceLock<usize> = OnceLock::new();

/// The most of the area a title is written to: the kernel shows no more
/// than a page of a title that runs past the arguments.
const MOST: usize = 4096;

/// The arguments the process was started with, whatever title it has
/// set since.
pub fn args_os() -> Vec<OsString> {
    ORIGINAL
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::args_os().collect())
}

/// Make room for a title beyond the argument area: copy the environment
/// strings that follow it elsewhere, point `environ` at the copies, and
/// let [`set`] write over the originals when a title needs them. Call it
/// once, at the start of `main`, before any other thread exists, since
/// it changes `environ` beneath them; a process that never calls it has
/// the argument area alone. On another system than Linux and macOS it
/// does nothing.
pub fn reserve() {
    let original = ORIGINAL.get_or_init(|| std::env::args_os().collect());
    let area =
        AREA.get_or_init(|| argument_span().map(|(start, length)| (start, length, original.len())));
    SPARE.get_or_init(|| match *area {
        Some((start, length, _)) => move_environment(start + length),
        None => 0,
    });
}

/// Set the process's title to `title`, cut to the room the argument area
/// has, or with [`reserve`] the environment strings after it too, and
/// on Linux its short name, the one `top` and `/proc/PID/comm` show, to
/// `short`, cut to 15 bytes. Call it from the main thread, before any
/// other thread reads the process's arguments; on another system than
/// Linux and macOS it does nothing.
pub fn set(title: &str, short: &str) {
    let original = ORIGINAL.get_or_init(|| std::env::args_os().collect());
    let area =
        AREA.get_or_init(|| argument_span().map(|(start, length)| (start, length, original.len())));
    let spare = SPARE.get().copied().unwrap_or(0);

    if let Some((start, length, arguments)) = *area {
        let arguments = if cfg!(target_os = "macos") {
            arguments
        } else {
            1
        };

        // The environment's strings are written over only when the title
        // does not fit the arguments.

        let whole = if fits(length, title, arguments) {
            length
        } else {
            (length + spare).min(MOST.max(length))
        };

        // SAFETY: the span is this process's argument strings and, when
        // `reserve` moved them elsewhere, the environment strings that
        // follow them, which exec placed in the main thread's stack
        // mapping, readable and writable for the life of the process and
        // copied into a forked child at the same address. No Rust
        // reference into it exists: the standard library reads argv
        // through raw pointers each time it is asked, the arguments were
        // copied out above before anything is written, and `environ` no
        // longer points into it.

        let bytes = unsafe { std::slice::from_raw_parts_mut(start as *mut u8, whole) };

        if whole == length {
            write_title(bytes, title, arguments);
        } else {
            write_long_title(bytes, title, arguments);
        }
    }

    set_short_name(short);
}

/// Whether `title` fits an argument area of `length` bytes as
/// `write_title` lays it out for `arguments` arguments.
fn fits(length: usize, title: &str, arguments: usize) -> bool {
    length
        .checked_sub(2 * arguments.saturating_sub(1) + 1)
        .is_some_and(|room| title.len() <= room)
}

/// `title` cut to at most `room` bytes, at the last space that leaves it
/// whole words, or at a character's end when it has no space to cut at.
fn cut(title: &str, room: usize) -> &str {
    if title.len() <= room {
        return title;
    }

    let mut end = room;

    while !title.is_char_boundary(end) {
        end -= 1;
    }

    // At a space, the words before it are whole.

    if title.as_bytes()[end] == b' ' {
        return title[..end].trim_end();
    }

    match title[..end].rfind(' ') {
        Some(space) if space > 0 => title[..space].trim_end(),
        _ => &title[..end],
    }
}

/// Fill an area that runs past the arguments with `title`: the title and
/// a NUL, then on macOS a single-space string for each argument after
/// the first, so `ps` still reads as many strings as the process had,
/// and NULs to the end, where the environment was.
fn write_long_title(area: &mut [u8], title: &str, arguments: usize) {
    let tail = 2 * arguments.saturating_sub(1);

    let Some(room) = area.len().checked_sub(tail + 1) else {
        return;
    };

    let title = cut(title, room).as_bytes();
    area[..title.len()].copy_from_slice(title);
    area[title.len()..].fill(0);

    for pair in area[title.len() + 1..title.len() + 1 + tail]
        .as_chunks_mut::<2>()
        .0
    {
        *pair = *b" \0";
    }
}

/// Fill `area` with `title` and what follows it: on Linux, where
/// `arguments` is 1, NULs; elsewhere spaces and a single-space string
/// for each argument after the first, so the area still holds as many
/// strings as the process had arguments. The title is cut to leave room
/// for those and a terminating NUL.
fn write_title(area: &mut [u8], title: &str, arguments: usize) {
    let tail = 2 * arguments.saturating_sub(1);

    let Some(room) = area.len().checked_sub(tail + 1) else {
        return;
    };

    let bytes = cut(title, room).as_bytes();
    let length = bytes.len();
    area[..length].copy_from_slice(bytes);

    if arguments <= 1 {
        area[length..].fill(0);
        return;
    }

    let head = area.len() - tail;
    area[length..head - 1].fill(b' ');
    area[head - 1] = 0;

    for pair in area[head..].as_chunks_mut::<2>().0 {
        *pair = *b" \0";
    }
}

/// Copy the environment strings to the heap and point `environ` at the
/// copies, and give how many bytes of the strings `exec` placed lie one
/// after another from `from`, the end of the argument area, which a title
/// may now use; 0, with nothing changed, when none starts there.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn move_environment(from: usize) -> usize {
    let table = environment_table();

    // SAFETY: `table` is the C runtime's `environ`, a NULL-terminated
    // array of pointers to NUL-terminated strings; nothing else runs, as
    // `reserve` requires, so nothing changes it meanwhile.

    let strings: Vec<*const libc::c_char> = unsafe {
        let mut strings = Vec::new();
        let mut at = *table;

        while !at.is_null() && !(*at).is_null() {
            strings.push(*at as *const libc::c_char);
            at = at.add(1);
        }

        strings
    };

    // The strings exec placed, one after another from the end of the
    // arguments, which are the room a title gains.

    let mut end = from;

    for string in &strings {
        if *string as usize != end {
            break;
        }

        // SAFETY: each pointer is a NUL-terminated string, as above.

        end += unsafe { libc::strlen(*string) } + 1;
    }

    if end == from {
        return 0;
    }

    // Every string is copied, and the copies are kept for the life of the
    // process, as the strings exec placed were.

    let mut copies: Vec<*mut libc::c_char> = strings
        .iter()
        .map(|string| {
            // SAFETY: as above; strdup copies the string to the heap.

            unsafe { libc::strdup(*string) }
        })
        .collect();

    if copies.iter().any(|copy| copy.is_null()) {
        return 0;
    }

    copies.push(std::ptr::null_mut());
    let copies = Box::leak(copies.into_boxed_slice());

    // SAFETY: `environ` is replaced by a NULL-terminated array of
    // NUL-terminated strings that lives for the life of the process, with
    // no other thread to read it meanwhile, as `reserve` requires.

    unsafe {
        *table = copies.as_mut_ptr();
    }

    end - from
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn move_environment(_from: usize) -> usize {
    0
}

/// Where the C runtime keeps `environ`.
#[cfg(target_os = "linux")]
fn environment_table() -> *mut *mut *mut libc::c_char {
    unsafe extern "C" {
        static mut environ: *mut *mut libc::c_char;
    }

    // SAFETY: taking the address of the C runtime's `environ` reads
    // nothing.

    unsafe { &raw mut environ }
}

/// Where the C runtime keeps `environ`.
#[cfg(target_os = "macos")]
fn environment_table() -> *mut *mut *mut libc::c_char {
    // SAFETY: _NSGetEnviron returns a pointer to the C runtime's
    // `environ`, valid for the life of the process.

    unsafe { libc::_NSGetEnviron() }
}

/// The argument area's address and length: on Linux from `arg_start`
/// to `arg_end` in `/proc/self/stat`, the span the kernel shows as
/// `/proc/PID/cmdline`.
#[cfg(target_os = "linux")]
fn argument_span() -> Option<(usize, usize)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let (start, end) = stat_span(&stat)?;
    let length = end.checked_sub(start).filter(|length| *length > 0)?;
    Some((start, length))
}

/// The argument area's address and length: on macOS from the first
/// argument to the end of the last, contiguous as exec lays them out,
/// which `ps` reads the command line from.
#[cfg(target_os = "macos")]
fn argument_span() -> Option<(usize, usize)> {
    // SAFETY: _NSGetArgc and _NSGetArgv return pointers to the C
    // runtime's argc and argv, set before main and valid for the life of
    // the process.

    let (argc, argv) = unsafe { (*libc::_NSGetArgc(), *libc::_NSGetArgv()) };

    if argc < 1 || argv.is_null() {
        return None;
    }

    // SAFETY: argv holds argc pointers to NUL-terminated strings, which
    // exec placed one after another; the walk stops at the first that
    // does not follow the one before.

    let (start, end) = unsafe {
        let start = *argv;
        let mut end = start.add(libc::strlen(start) + 1);

        for index in 1..argc as usize {
            let next = *argv.add(index);

            if next != end {
                break;
            }

            end = next.add(libc::strlen(next) + 1);
        }

        (start, end)
    };

    let length = (end as usize).checked_sub(start as usize)?;
    Some((start as usize, length))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn argument_span() -> Option<(usize, usize)> {
    None
}

/// `arg_start` and `arg_end` from the text of `/proc/self/stat`: the
/// 48th and 49th fields, counted after the command name in parentheses,
/// which may itself hold spaces and parentheses.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn stat_span(stat: &str) -> Option<(usize, usize)> {
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();

    // The first field after the name is the state, the third of all.

    let field = |number: usize| fields.get(number - 3)?.parse::<usize>().ok();
    Some((field(48)?, field(49)?))
}

#[cfg(target_os = "linux")]
fn set_short_name(short: &str) {
    let mut name = [0u8; 16];
    let length = short.len().min(15);
    name[..length].copy_from_slice(&short.as_bytes()[..length]);

    // SAFETY: PR_SET_NAME reads a NUL-terminated string of at most 16
    // bytes from the pointer, which `name` is, and keeps no reference.

    unsafe {
        libc::prctl(libc::PR_SET_NAME, name.as_ptr() as libc::c_ulong, 0, 0, 0);
    }
}

#[cfg(not(target_os = "linux"))]
fn set_short_name(_short: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_fills_the_area_and_is_cut_to_leave_a_nul() {
        let mut area = *b"python -m muru.host --app x\0";
        write_title(&mut area, "muru host shop@r1", 1);
        assert_eq!(&area[..18], b"muru host shop@r1\0");
        assert!(area[18..].iter().all(|byte| *byte == 0));

        let mut small = *b"abc\0";
        write_title(&mut small, "muru host shop@r1", 1);
        assert_eq!(&small, b"mur\0");

        write_title(&mut [], "anything", 1);
    }

    #[test]
    fn a_title_keeps_a_string_for_every_argument_where_asked() {
        // Five arguments: the title, then four single-space strings.

        let mut area = *b"python -m muru.host --app x\0";
        write_title(&mut area, "muru host shop@r1", 5);
        let strings: Vec<&[u8]> = area[..area.len() - 1].split(|byte| *byte == 0).collect();
        assert_eq!(strings.len(), 5);
        assert_eq!(strings[0].trim_ascii_end(), b"muru host shop@r1");
        assert!(strings[1..].iter().all(|string| *string == b" "));

        // Too small for the arguments' strings leaves it as it is.

        let mut tiny = *b"a\0b\0";
        write_title(&mut tiny, "muru", 3);
        assert_eq!(&tiny, b"a\0b\0");
    }

    #[test]
    fn a_title_that_does_not_fit_is_cut_on_a_word() {
        assert_eq!(cut("muru serve shop", 13), "muru serve");
        assert_eq!(cut("muru serve shop", 10), "muru serve");
        assert_eq!(cut("muru serve shop", 15), "muru serve shop");
        assert_eq!(cut("murudaemon", 4), "muru");
        assert_eq!(cut("muru é", 6), "muru");

        // The room `muructl serve` gives, 13 bytes and a NUL.

        let mut area = *b"muructl\0serve\0";
        write_title(&mut area, "muru serve shop", 1);
        assert_eq!(&area, b"muru serve\0\0\0\0");
    }

    #[test]
    fn a_long_title_runs_past_the_arguments_into_the_environment() {
        // `muructl serve` and two environment strings after it.

        let mut area = *b"muructl\0serve\0HOME=/root\0PATH=/bin\0";
        assert!(!fits(14, "muru serve shop", 1));
        write_long_title(&mut area, "muru serve shop", 1);
        assert_eq!(&area[..16], b"muru serve shop\0");
        assert!(area[16..].iter().all(|byte| *byte == 0));

        // On macOS, a single-space string for each argument after the
        // first follows the title, so ps reads as many as there were.

        let mut area = *b"muructl\0serve\0HOME=/root\0PATH=/bin\0";
        write_long_title(&mut area, "muru serve shop", 2);
        assert_eq!(&area[..18], b"muru serve shop\0 \0");
        assert!(area[18..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn the_argument_span_is_read_past_a_name_with_parentheses() {
        let mut fields: Vec<String> = (3..=52).map(|number| number.to_string()).collect();
        fields[48 - 3] = "1000".to_owned();
        fields[49 - 3] = "1040".to_owned();
        let stat = format!("42 (a (b) c) {}", fields.join(" "));
        assert_eq!(stat_span(&stat), Some((1000, 1040)));
        assert_eq!(stat_span("42 (short) R 1"), None);
    }
}
