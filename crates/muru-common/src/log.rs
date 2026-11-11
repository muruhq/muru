//! Muru's own log lines and the times on them (section 11.8 of the
//! specification, ADR 0043).
//!
//! Every line gets a time when it is written; whether the time is shown
//! depends on where the line goes. A regular file always carries the full
//! form, RFC 3339 in UTC with milliseconds. Standard output and standard
//! error carry it only when `--timestamps` was given, since a terminal,
//! a container runtime and journald each have their own. The short form,
//! a clock in local time, is for a terminal only.

use std::fmt;
use std::fs::File;
use std::io::{self, IsTerminal as _, Write as _};
use std::os::fd::AsFd as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use jiff::Timestamp;
use jiff::tz::TimeZone;

/// The two streams a long-running process writes its lines to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// Whether `--timestamps` (or the configuration key) asked for the time
/// on standard output and standard error.
static TIMESTAMPS: AtomicBool = AtomicBool::new(false);

/// Put the full time in front of every line written to standard output
/// and standard error, as `--timestamps` does.
pub fn show_timestamps(on: bool) {
    TIMESTAMPS.store(on, Ordering::Relaxed);
}

/// Whether `--timestamps` is in force.
pub fn timestamps_shown() -> bool {
    TIMESTAMPS.load(Ordering::Relaxed)
}

/// Whether lines written to `stream` carry the full time in front: always
/// when the stream is a regular file, otherwise with `--timestamps`.
pub fn stamped(stream: Stream) -> bool {
    timestamps_shown() || is_file(route(stream))
}

/// Whether every line goes to standard output, as under `muructl serve`
/// (section 13.9), so lines stay in order and a log collector does not
/// take everything on standard error for an error.
static ONE_STREAM: AtomicBool = AtomicBool::new(false);

/// Send every line to standard output from now on.
pub fn one_stream(on: bool) {
    ONE_STREAM.store(on, Ordering::Relaxed);
}

/// The stream a line meant for `stream` is written to.
pub fn route(stream: Stream) -> Stream {
    if ONE_STREAM.load(Ordering::Relaxed) {
        Stream::Stdout
    } else {
        stream
    }
}

/// Whether lines are written as JSON objects, one per line, as `muructl
/// serve --log-format json` asks (section 13.9).
static JSON: AtomicBool = AtomicBool::new(false);

/// Write every line as a JSON object from now on.
pub fn json_lines(on: bool) {
    JSON.store(on, Ordering::Relaxed);
}

/// Whether lines are written as JSON objects.
pub fn json() -> bool {
    JSON.load(Ordering::Relaxed)
}

/// The pod's identity, for every JSON line, as fields ready to append
/// after a record's own: `pod`, `namespace` and `node` from the Downward
/// API variables a Deployment sets (`POD_NAME`, `POD_NAMESPACE`,
/// `NODE_NAME`), `service` from `OTEL_SERVICE_NAME` and `resource` from
/// `OTEL_RESOURCE_ATTRIBUTES`. Empty when none is set.
pub fn identity() -> &'static str {
    static IDENTITY: OnceLock<String> = OnceLock::new();

    IDENTITY.get_or_init(|| identity_from(|name| std::env::var(name).ok()))
}

fn identity_from(read: impl Fn(&str) -> Option<String>) -> String {
    let mut fields = String::new();

    for (field, variable) in [
        ("pod", "POD_NAME"),
        ("namespace", "POD_NAMESPACE"),
        ("node", "NODE_NAME"),
        ("service", "OTEL_SERVICE_NAME"),
    ] {
        if let Some(value) = read(variable).filter(|value| !value.is_empty()) {
            let _ = fmt::write(
                &mut fields,
                format_args!(",\"{field}\":{}", json_string(&value)),
            );
        }
    }

    // OpenTelemetry writes the attributes as key=value pairs separated by
    // commas, each part percent-encoded.

    let attributes: Vec<String> = read("OTEL_RESOURCE_ATTRIBUTES")
        .unwrap_or_default()
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| {
            format!(
                "{}:{}",
                json_string(&percent_decoded(key.trim())),
                json_string(&percent_decoded(value.trim()))
            )
        })
        .collect();

    if !attributes.is_empty() {
        let _ = fmt::write(
            &mut fields,
            format_args!(",\"resource\":{{{}}}", attributes.join(",")),
        );
    }

    fields
}

/// Text with its `%XX` escapes decoded, as OpenTelemetry's variables are
/// written.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;

    while at < bytes.len() {
        let escaped = (bytes[at] == b'%' && at + 2 < bytes.len())
            .then(|| std::str::from_utf8(&bytes[at + 1..at + 3]).ok())
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());

        match escaped {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// A JSON line with the pod's identity added after the record's own
/// fields, before its closing brace.
pub fn with_identity(mut line: String) -> String {
    let fields = identity();

    if !fields.is_empty()
        && let Some(end) = line.rfind('}')
    {
        line.insert_str(end, fields);
    }

    line
}

/// A string as a JSON string literal, quotes included.
pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');

    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = fmt::write(&mut out, format_args!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }

    out.push('"');
    out
}

/// Whether `stream` is a terminal, decided once.
pub fn is_terminal(stream: Stream) -> bool {
    static STDOUT: OnceLock<bool> = OnceLock::new();
    static STDERR: OnceLock<bool> = OnceLock::new();

    match stream {
        Stream::Stdout => *STDOUT.get_or_init(|| io::stdout().is_terminal()),
        Stream::Stderr => *STDERR.get_or_init(|| io::stderr().is_terminal()),
    }
}

/// The time a record's own line starts with, such as an access log line,
/// whose time is part of the record: the short clock on a terminal, and
/// the full form otherwise or with `--timestamps`.
pub fn record_time(stream: Stream, time: SystemTime) -> String {
    if timestamps_shown() || !is_terminal(route(stream)) {
        full(time)
    } else {
        clock(time)
    }
}

/// Whether `stream` is a regular file, decided once: the daemon and the
/// edge write to their log files through their standard streams, and a
/// reopen after rotation replaces the file with another file.
fn is_file(stream: Stream) -> bool {
    static STDOUT: OnceLock<bool> = OnceLock::new();
    static STDERR: OnceLock<bool> = OnceLock::new();

    let cell = match stream {
        Stream::Stdout => &STDOUT,
        Stream::Stderr => &STDERR,
    };

    *cell.get_or_init(|| {
        let descriptor = match stream {
            Stream::Stdout => io::stdout().as_fd().try_clone_to_owned(),
            Stream::Stderr => io::stderr().as_fd().try_clone_to_owned(),
        };

        descriptor
            .ok()
            .map(File::from)
            .and_then(|file| file.metadata().ok())
            .is_some_and(|metadata| metadata.is_file())
    })
}

/// The full form: RFC 3339 in UTC with milliseconds and a `Z`, for
/// example `2026-11-02T05:42:22.836Z`.
pub fn full(time: SystemTime) -> String {
    let Ok(timestamp) = Timestamp::try_from(time) else {
        return "1970-01-01T00:00:00.000Z".to_owned();
    };
    timestamp
        .to_zoned(TimeZone::UTC)
        .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// The short form: the local clock with milliseconds, for example
/// `16:42:22.836`, for a terminal only.
pub fn clock(time: SystemTime) -> String {
    static LOCAL: OnceLock<TimeZone> = OnceLock::new();

    let Ok(timestamp) = Timestamp::try_from(time) else {
        return "00:00:00.000".to_owned();
    };
    let zone = LOCAL.get_or_init(TimeZone::system).clone();
    timestamp
        .to_zoned(zone)
        .strftime("%H:%M:%S%.3f")
        .to_string()
}

/// Split the full time off the front of a line written by Muru, giving
/// the time, when the line starts with one, and the rest.
pub fn split_time(line: &str) -> (Option<&str>, &str) {
    // 2026-11-02T05:42:22.836Z and a space: 25 bytes.

    let bytes = line.as_bytes();
    let shaped = bytes.len() >= 25
        && bytes[..24].iter().enumerate().all(|(at, byte)| match at {
            4 | 7 => *byte == b'-',
            10 => *byte == b'T',
            13 | 16 => *byte == b':',
            19 => *byte == b'.',
            23 => *byte == b'Z',
            _ => byte.is_ascii_digit(),
        })
        && bytes[24] == b' ';

    if shaped {
        (Some(&line[..24]), &line[25..])
    } else {
        (None, line)
    }
}

/// The time a full-form stamp names, as `split_time` gives it.
pub fn parse_full(stamp: &str) -> Option<SystemTime> {
    stamp.parse::<Timestamp>().ok().map(SystemTime::from)
}

/// Write one line to `stream`: the full time when the stream is stamped,
/// then `tag: ` and the message, in one write so lines from several
/// threads never interleave.
pub fn line(stream: Stream, tag: &str, message: fmt::Arguments<'_>) {
    line_of_kind(stream, "muru", tag, message);
}

/// Write one of Muru's own lines as `line` does, with a `kind` of its own
/// in JSON, so a log collector can pick it out.
pub fn line_of_kind(stream: Stream, kind: &str, tag: &str, message: fmt::Arguments<'_>) {
    let mut text = String::with_capacity(128);

    if json() {
        text.push_str(&format!(
            "{{\"time\":\"{}\",\"kind\":{},\"message\":{}{}}}\n",
            full(SystemTime::now()),
            json_string(kind),
            json_string(&message.to_string()),
            identity()
        ));
        write(stream, text.as_bytes());
        return;
    }

    if stamped(stream) {
        text.push_str(&full(SystemTime::now()));
        text.push(' ');
    }

    text.push_str(tag);
    text.push_str(": ");
    let _ = fmt::write(&mut text, message);
    text.push('\n');
    write(stream, text.as_bytes());
}

/// Write a line that already carries its tag, such as a host's or a
/// worker's relayed output, with the time in front when the stream is
/// stamped. `time` is when the line arrived.
pub fn relayed(stream: Stream, time: SystemTime, text: &str) {
    write(stream, relayed_line(stream, time, text).as_bytes());
}

/// A relayed line as `relayed` writes it, newline included, for a caller
/// that writes it itself.
pub fn relayed_line(stream: Stream, time: SystemTime, text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);

    if json() {
        // A relayed line is the application's output, tagged by the host
        // with the worker or the host it came from.

        let (source, message) = match text.split_once(": ") {
            Some((source, message)) if source.starts_with("muru.") => (source, message),
            _ => ("", text),
        };

        out.push_str(&app_record(time, source, message));
        out.push('\n');
        return out;
    }

    if stamped(stream) {
        out.push_str(&full(time));
        out.push(' ');
    }

    out.push_str(text);
    out.push('\n');
    out
}

/// The JSON record of a line of the application's output. A line that
/// is itself a JSON object, such as a record of Python logging's json
/// format, is the record, with Muru's fields added: its own `time` is
/// kept, and `kind` and `source` say where it came from. Any other line is
/// the record's `message`.
fn app_record(time: SystemTime, source: &str, message: &str) -> String {
    let parsed = message
        .starts_with('{')
        .then(|| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(message).ok())
        .flatten();

    let Some(mut object) = parsed else {
        return format!(
            "{{\"time\":\"{}\",\"kind\":\"app\",\"source\":{},\"message\":{}{}}}",
            full(time),
            json_string(source),
            json_string(message),
            identity()
        );
    };

    let own_time = match object.remove("time") {
        Some(serde_json::Value::String(text)) => text,
        _ => full(time),
    };

    object.remove("kind");
    object.remove("source");

    let mut record = format!(
        "{{\"time\":{},\"kind\":\"app\",\"source\":{}",
        json_string(&own_time),
        json_string(source)
    );

    for (key, value) in &object {
        let _ = fmt::write(&mut record, format_args!(",{}:{value}", json_string(key)));
    }

    record.push_str(identity());
    record.push('}');
    record
}

/// Section 11.8: a Python traceback written to a stream arrives a line at
/// a time, and a log collector that splits records at newlines makes a
/// record of each. This gathers one source's traceback, from `Traceback
/// (most recent call last):` to the exception's own line, chained
/// exceptions included, so it can be written as one record.
#[derive(Debug, Default)]
pub struct Traceback {
    lines: Vec<String>,
    /// Whether the exception's line has arrived, so another traceback
    /// chained to it may follow.
    ended: bool,
}

/// What a line does to a traceback being gathered.
#[derive(Debug, PartialEq, Eq)]
pub enum Gathered {
    /// The line is part of the traceback.
    Kept,
    /// The traceback was complete and the line is not part of it: write
    /// the traceback, then the line.
    Finished(String),
}

impl Traceback {
    /// Whether `message` starts a traceback.
    pub fn starts(message: &str) -> bool {
        message.starts_with("Traceback (most recent call last):")
    }

    /// Start gathering with the first line.
    pub fn new(first: &str) -> Self {
        Self {
            lines: vec![first.to_owned()],
            ended: false,
        }
    }

    /// Take the next line of the same source.
    pub fn push(&mut self, message: &str) -> Gathered {
        // A blank line leaves the traceback as it was: the next line says
        // whether another exception is chained to it.

        if message.is_empty() {
            self.lines.push(String::new());
            return Gathered::Kept;
        }

        let chained = Self::starts(message)
            || message.starts_with("During handling of the above exception")
            || message.starts_with("The above exception was the direct cause");

        if self.ended && !chained {
            return Gathered::Finished(self.take());
        }

        let indented = message.starts_with(' ') || message.starts_with('\t');
        self.ended = !indented && !chained;
        self.lines.push(message.to_owned());
        Gathered::Kept
    }

    /// The traceback gathered so far, as one message, for when no more is
    /// coming.
    pub fn take(&mut self) -> String {
        while self.lines.last().is_some_and(String::is_empty) {
            self.lines.pop();
        }

        std::mem::take(&mut self.lines).join("\n")
    }
}

/// Write bytes that hold whole lines to `stream`, or to standard output
/// when every line goes there, in one write.
pub fn write(stream: Stream, bytes: &[u8]) {
    // A stream that cannot be written to (a closed terminal, a full disk)
    // never stops the process that logs to it.

    let _ = match route(stream) {
        Stream::Stdout => io::stdout().lock().write_all(bytes),
        Stream::Stderr => io::stderr().lock().write_all(bytes),
    };
}

/// Whether no panic has been reported yet, for the note about
/// `RUST_BACKTRACE`, which the standard library's own hook gives only for
/// the first.
static FIRST_PANIC: AtomicBool = AtomicBool::new(true);

/// Report panics as Muru's own lines on standard error, through the same
/// helper as every other line: stamped where those are, a single record
/// where lines are JSON. What a panic does next, unwinding or aborting, and
/// the exit status it leads to are unchanged; only the report is.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let location = info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "an unknown location".to_owned());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "Box<dyn Any>".to_owned());

        // A backtrace is taken when RUST_BACKTRACE asks for one, as the
        // standard library's hook takes it, whatever RUST_LIB_BACKTRACE
        // says; `full` keeps the frames the short form leaves out.

        let asked = std::env::var("RUST_BACKTRACE").ok();
        let backtrace = asked.as_deref().filter(|value| *value != "0").map(|value| {
            let captured = std::backtrace::Backtrace::force_capture();

            if value == "full" {
                format!("{captured:#}")
            } else {
                format!("{captured}")
            }
        });

        let full_backtrace = asked.as_deref() == Some("full");
        let report = Panic {
            thread: thread.name().unwrap_or("<unnamed>"),
            location: &location,
            message: &message,
            frames: backtrace.as_deref().map(|backtrace| {
                let frames = frames(backtrace);

                if full_backtrace {
                    frames
                } else {
                    short(frames)
                }
            }),
            note: backtrace.is_none() && FIRST_PANIC.swap(false, Ordering::Relaxed),
        };

        let text = report.render(json(), stamped(Stream::Stderr).then(SystemTime::now));
        write(Stream::Stderr, text.as_bytes());
    }));
}

/// The frames of a backtrace as the standard library prints it, one
/// string each: a frame's `N: symbol` line with its `at FILE:LINE` lines
/// after it, on one line.
fn frames(backtrace: &str) -> Vec<String> {
    let mut frames: Vec<String> = Vec::new();

    for line in backtrace
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let numbered = line
            .split_once(": ")
            .is_some_and(|(number, _)| number.chars().all(|c| c.is_ascii_digit()));

        match frames.last_mut() {
            Some(frame) if !numbered => {
                frame.push(' ');
                frame.push_str(line);
            }
            _ => frames.push(line.to_owned()),
        }
    }

    frames
}

/// The frames the standard library's hook shows in its short form:
/// those between the panic machinery and the start of the thread, which
/// mark themselves with `__rust_end_short_backtrace` and
/// `__rust_begin_short_backtrace`, numbered again from 0. Without the
/// markers every frame is kept.
fn short(frames: Vec<String>) -> Vec<String> {
    let start = frames
        .iter()
        .position(|frame| frame.contains("__rust_end_short_backtrace"))
        .map_or(0, |end| end + 1);
    let stop = frames[start..]
        .iter()
        .position(|frame| frame.contains("__rust_begin_short_backtrace"))
        .map_or(frames.len(), |begin| start + begin);

    frames[start..stop]
        .iter()
        .enumerate()
        .map(|(number, frame)| {
            let rest = frame
                .split_once(": ")
                .map_or(frame.as_str(), |(_, rest)| rest);
            format!("{number}: {rest}")
        })
        .collect()
}

/// A panic's report, before it is written.
struct Panic<'a> {
    thread: &'a str,
    location: &'a str,
    message: &'a str,
    /// The backtrace's frames, when one was asked for.
    frames: Option<Vec<String>>,
    /// Whether to say how to ask for a backtrace.
    note: bool,
}

/// How to ask for a backtrace, as the standard library's hook says it.
const BACKTRACE_NOTE: &str =
    "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace";

impl Panic<'_> {
    /// The report as it is written: one JSON record, or one Muru line for
    /// the panic and one for each frame or the note, each with the time in
    /// front when `stamped` gives one. A message of several lines keeps
    /// its first on the panic's line and gives each further one a line of
    /// its own, so every line starts the same way.
    fn render(&self, json: bool, stamped: Option<SystemTime>) -> String {
        if json {
            let mut record = format!(
                "{{\"time\":\"{}\",\"kind\":\"panic\",\"message\":{},\"thread\":{},\"location\":{}",
                full(SystemTime::now()),
                json_string(self.message),
                json_string(self.thread),
                json_string(self.location)
            );

            if let Some(frames) = &self.frames {
                let frames: Vec<String> = frames.iter().map(|frame| json_string(frame)).collect();
                record.push_str(&format!(",\"backtrace\":[{}]", frames.join(",")));
            } else if self.note {
                record.push_str(&format!(",\"note\":{}", json_string(BACKTRACE_NOTE)));
            }

            record.push_str(identity());
            record.push_str("}\n");
            return record;
        }

        let mut lines = self.message.lines();
        let first = lines.next().unwrap_or_default();
        let mut text = vec![format!(
            "thread '{}' panicked at {}: {first}",
            self.thread, self.location
        )];
        text.extend(lines.map(str::to_owned));

        if let Some(frames) = &self.frames {
            text.extend(frames.iter().map(|frame| format!("  {frame}")));
        } else if self.note {
            text.push(BACKTRACE_NOTE.to_owned());
        }

        let time = stamped
            .map(|time| format!("{} ", full(time)))
            .unwrap_or_default();

        text.iter()
            .map(|line| format!("{time}muru: panic: {line}\n"))
            .collect()
    }
}

/// Print one of Muru's own lines to standard output, as `println!` does,
/// with `muru: ` in front and the time when the stream is stamped.
#[macro_export]
macro_rules! muru_println {
    ($($arg:tt)*) => {
        $crate::log::line($crate::log::Stream::Stdout, "muru", format_args!($($arg)*))
    };
}

/// Print one of Muru's own lines to standard error, as `eprintln!` does,
/// with `muru: ` in front and the time when the stream is stamped.
#[macro_export]
macro_rules! muru_eprintln {
    ($($arg:tt)*) => {
        $crate::log::line($crate::log::Stream::Stderr, "muru", format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn a_panic_is_one_line_with_the_note_or_a_line_a_frame() {
        let time = UNIX_EPOCH + Duration::from_millis(1_793_597_342_836);
        let mut report = Panic {
            thread: "main",
            location: "src/main.rs:10:5",
            message: "boom\nsecond",
            frames: None,
            note: true,
        };

        assert_eq!(
            report.render(false, Some(time)),
            "2026-11-02T05:29:02.836Z muru: panic: thread 'main' panicked at src/main.rs:10:5: boom\n\
             2026-11-02T05:29:02.836Z muru: panic: second\n\
             2026-11-02T05:29:02.836Z muru: panic: note: run with `RUST_BACKTRACE=1` environment \
             variable to display a backtrace\n"
        );

        report.message = "boom";
        report.frames = Some(frames(
            "   0: muru::main\n             at ./src/main.rs:10:5\n   1: std::rt::lang_start\n",
        ));
        assert_eq!(
            report.render(false, None),
            "muru: panic: thread 'main' panicked at src/main.rs:10:5: boom\n\
             muru: panic:   0: muru::main at ./src/main.rs:10:5\n\
             muru: panic:   1: std::rt::lang_start\n"
        );

        let trimmed = short(frames(
            "   0: trace\n   1: __rust_end_short_backtrace\n   2: app::f\n   3: \
             __rust_begin_short_backtrace\n   4: main\n",
        ));
        assert_eq!(trimmed, ["0: app::f"]);

        let record: serde_json::Value =
            serde_json::from_str(report.render(true, None).trim_end()).unwrap();
        assert_eq!(record["kind"], "panic");
        assert_eq!(record["message"], "boom");
        assert_eq!(record["location"], "src/main.rs:10:5");
        assert_eq!(record["backtrace"][1], "1: std::rt::lang_start");
    }

    /// Set in the copy of the test binary that panics.
    const PANIC_CHILD: &str = "MURU_TEST_PANIC_CHILD";

    #[test]
    fn a_panic_with_standard_error_on_a_file_is_reported_stamped() {
        if std::env::var_os(PANIC_CHILD).is_some() {
            install_panic_hook();
            panic!("on purpose");
        }

        // The test binary runs this test again with standard error on a
        // regular file, where it panics.

        let path = std::env::temp_dir().join(format!("muru-panic-{}.log", std::process::id()));
        let file = File::create(&path).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "log::tests::a_panic_with_standard_error_on_a_file_is_reported_stamped",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PANIC_CHILD, "1")
            .env_remove("RUST_BACKTRACE")
            .stdout(std::process::Stdio::null())
            .stderr(file)
            .status()
            .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(!status.success(), "the child's test panicked");

        let reported: Vec<&str> = written
            .lines()
            .filter(|line| line.contains(" muru: panic: "))
            .collect();
        assert_eq!(reported.len(), 2, "{written}");
        assert!(
            reported.iter().all(|line| split_time(line).0.is_some()),
            "{written}"
        );
        assert!(
            reported[0].contains("panicked at crates/muru-common/src/log.rs:"),
            "{written}"
        );
        assert!(reported[0].ends_with(": on purpose"), "{written}");
        assert!(reported[1].ends_with("to display a backtrace"), "{written}");
    }

    #[test]
    fn the_full_form_is_rfc_3339_in_utc_with_milliseconds() {
        let time = UNIX_EPOCH + Duration::from_millis(1_793_597_342_836);
        assert_eq!(full(time), "2026-11-02T05:29:02.836Z");
        assert_eq!(full(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(
            json_string("a \"b\" \\ c\n\u{1}"),
            "\"a \\\"b\\\" \\\\ c\\n\\u0001\""
        );
    }

    #[test]
    fn a_leading_time_splits_off() {
        assert_eq!(
            split_time("2026-11-02T05:42:22.836Z muru.host[42]: ready"),
            (Some("2026-11-02T05:42:22.836Z"), "muru.host[42]: ready")
        );
        assert_eq!(
            split_time("muru.host[42]: ready"),
            (None, "muru.host[42]: ready")
        );
        assert_eq!(
            split_time("2026-11-02 not a time at all, no"),
            (None, "2026-11-02 not a time at all, no")
        );
    }

    #[test]
    fn the_pod_identity_comes_from_the_environment() {
        let identity = identity_from(|name| match name {
            "POD_NAME" => Some("shop-7d9f".to_owned()),
            "POD_NAMESPACE" => Some("prod".to_owned()),
            "OTEL_RESOURCE_ATTRIBUTES" => {
                Some("deployment.environment=prod, team=a%2Cb".to_owned())
            }
            _ => None,
        });
        assert_eq!(
            identity,
            ",\"pod\":\"shop-7d9f\",\"namespace\":\"prod\",\"resource\":{\"deployment.environment\":\"prod\",\"team\":\"a,b\"}"
        );
        assert_eq!(identity_from(|_| None), "");
    }

    #[test]
    fn a_json_line_from_the_application_is_the_record() {
        let time = UNIX_EPOCH + Duration::from_millis(1_793_597_342_836);
        let record = app_record(
            time,
            "muru.worker[0]",
            r#"{"time": "2026-11-02T05:29:01.000Z", "level": "ERROR", "message": "failed"}"#,
        );
        let parsed: serde_json::Value = serde_json::from_str(&record).expect("json");
        assert_eq!(parsed["time"], "2026-11-02T05:29:01.000Z");
        assert_eq!(parsed["kind"], "app");
        assert_eq!(parsed["source"], "muru.worker[0]");
        assert_eq!(parsed["level"], "ERROR");
        assert!(record.starts_with(r#"{"time":"2026-11-02T05:29:01.000Z","kind":"app""#));

        let record = app_record(time, "muru.worker[0]", "{not json");
        let parsed: serde_json::Value = serde_json::from_str(&record).expect("json");
        assert_eq!(parsed["message"], "{not json");
        assert_eq!(parsed["time"], "2026-11-02T05:29:02.836Z");
    }

    #[test]
    fn a_traceback_is_gathered_to_its_exception_with_what_is_chained() {
        let mut traceback = Traceback::new("Traceback (most recent call last):");

        for line in [
            "  File \"app.py\", line 3, in view",
            "    1 / 0",
            "    ~~^~~",
            "ZeroDivisionError: division by zero",
            "",
            "During handling of the above exception, another exception occurred:",
            "",
            "Traceback (most recent call last):",
            "  File \"app.py\", line 5, in view",
            "KeyError: 'sku'",
            "",
        ] {
            assert_eq!(traceback.push(line), Gathered::Kept, "{line}");
        }

        let Gathered::Finished(message) = traceback.push("served /next") else {
            panic!("the traceback did not finish");
        };
        assert!(message.starts_with("Traceback (most recent call last):\n  File"));
        assert!(message.ends_with("KeyError: 'sku'"), "{message}");
        assert_eq!(message.lines().count(), 11);
    }

    #[test]
    fn the_short_form_is_a_clock_with_milliseconds() {
        let time = UNIX_EPOCH + Duration::from_millis(1_793_597_342_836);
        let text = clock(time);
        assert_eq!(text.len(), 12, "{text}");
        assert!(text.ends_with(":02.836"), "{text}");
    }
}
