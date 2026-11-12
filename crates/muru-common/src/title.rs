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

use std::ffi::OsString;
use std::sync::OnceLock;

/// The arguments the process was started with, kept before the first
/// title replaced them.
static ORIGINAL: OnceLock<Vec<OsString>> = OnceLock::new();

/// The argument area, as its address and length, and how many arguments
/// the process was started with; none where it cannot be found.
static AREA: OnceLock<Option<(usize, usize, usize)>> = OnceLock::new();

/// The arguments the process was started with, whatever title it has
/// set since.
pub fn args_os() -> Vec<OsString> {
    ORIGINAL
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::args_os().collect())
}

/// Set the process's title to `title`, cut to the room the argument area
/// has, and on Linux its short name, the one `top` and `/proc/PID/comm`
/// show, to `short`, cut to 15 bytes. Call it from the main thread,
/// before any other thread reads the process's arguments; on another
/// system than Linux and macOS it does nothing.
pub fn set(title: &str, short: &str) {
    let original = ORIGINAL.get_or_init(|| std::env::args_os().collect());
    let area =
        AREA.get_or_init(|| argument_span().map(|(start, length)| (start, length, original.len())));

    if let Some((start, length, arguments)) = *area {
        // SAFETY: the span is this process's argument strings, which exec
        // placed in the main thread's stack mapping, readable and writable
        // for the life of the process and copied into a forked child at
        // the same address. No Rust reference into it exists: the standard
        // library reads argv through raw pointers each time it is asked,
        // and the arguments were copied out above before anything is
        // written.

        let bytes = unsafe { std::slice::from_raw_parts_mut(start as *mut u8, length) };
        write_title(
            bytes,
            title,
            if cfg!(target_os = "macos") {
                arguments
            } else {
                1
            },
        );
    }

    set_short_name(short);
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

    let bytes = title.as_bytes();
    let length = bytes.len().min(room);
    area[..length].copy_from_slice(&bytes[..length]);

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
    fn the_argument_span_is_read_past_a_name_with_parentheses() {
        let mut fields: Vec<String> = (3..=52).map(|number| number.to_string()).collect();
        fields[48 - 3] = "1000".to_owned();
        fields[49 - 3] = "1040".to_owned();
        let stat = format!("42 (a (b) c) {}", fields.join(" "));
        assert_eq!(stat_span(&stat), Some((1000, 1040)));
        assert_eq!(stat_span("42 (short) R 1"), None);
    }
}
