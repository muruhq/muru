//! The period a collection covers, and where it starts in a log.
//!
//! A log can be several gigabytes, and the window wanted is usually its
//! last hour, so the start of the window is found from the times its
//! lines carry, by halving the file, without reading what comes before.
//! Only the lines of the window are then read.

use std::io::{self, BufRead as _, BufReader, Read, Seek, SeekFrom};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Below this much of a log, lines are read one after another to find
/// the first of the window.
const STEP: u64 = 64 * 1024;

/// How far past a place in a log a line with a time is looked for. A
/// stretch this long with none, a traceback of thousands of lines say,
/// is passed over as part of what came before.
const LOOK: u64 = 256 * 1024;

/// A period: from an instant up to another, in Unix milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Window {
    /// The first instant in the window.
    pub since_unix_ms: u64,
    /// The instant the window ends, which is when it was asked for.
    pub until_unix_ms: u64,
}

impl Window {
    /// The window of the last `period` up to `now`.
    pub fn last(period: Duration, now: SystemTime) -> Self {
        let until = now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);

        Self {
            since_unix_ms: until.saturating_sub(period.as_millis() as u64),
            until_unix_ms: until,
        }
    }

    /// Whether an instant is in the window. What happens while the
    /// collection runs is in it too, so only the start is a limit.
    pub fn holds(&self, time_unix_ms: u64) -> bool {
        time_unix_ms >= self.since_unix_ms
    }
}

/// The time a line of a log carries, in Unix milliseconds: the full time
/// Muru puts in front of every line of a file (section 11.8), the `time`
/// of a JSON record such as the access log's, or the bracketed time of a
/// record in the combined format. None for a line that carries no time,
/// as the second line of a traceback.
pub fn line_time(line: &[u8]) -> Option<u64> {
    if let Some(time) = full_time(line) {
        return Some(time);
    }

    if line.first() == Some(&b'{') {
        const KEY: &[u8] = b"\"time\":\"";
        let at = line.windows(KEY.len()).position(|found| found == KEY)?;
        return full_time(&line[at + KEY.len()..]);
    }

    let open = line.iter().position(|byte| *byte == b'[')?;
    combined_time(&line[open + 1..])
}

/// The number a run of ASCII digits spells, when it is one.
fn digits(text: &[u8]) -> Option<u64> {
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }

    Some(
        text.iter()
            .fold(0, |number, digit| number * 10 + u64::from(digit - b'0')),
    )
}

/// The time `text` starts with in the full form, as
/// `2026-11-02T05:42:22.836Z`.
fn full_time(text: &[u8]) -> Option<u64> {
    let text = text.get(..24)?;
    let marks = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];

    if marks.iter().any(|(at, mark)| text[*at] != *mark) || text[19] != b'.' || text[23] != b'Z' {
        return None;
    }

    let seconds = civil_seconds(
        digits(&text[..4])?,
        digits(&text[5..7])?,
        digits(&text[8..10])?,
        digits(&text[11..13])?,
        digits(&text[14..16])?,
        digits(&text[17..19])?,
    )?;
    Some(seconds * 1000 + digits(&text[20..23])?)
}

/// The time `text` starts with in the combined format's form, as
/// `02/Nov/2026:05:42:22 +0000`.
fn combined_time(text: &[u8]) -> Option<u64> {
    const MONTHS: [&[u8]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    let text = text.get(..26)?;
    let marks = [
        (2, b'/'),
        (6, b'/'),
        (11, b':'),
        (14, b':'),
        (17, b':'),
        (20, b' '),
    ];

    if marks.iter().any(|(at, mark)| text[*at] != *mark) {
        return None;
    }

    let month = MONTHS.iter().position(|month| *month == &text[3..6])? as u64 + 1;
    let seconds = civil_seconds(
        digits(&text[7..11])?,
        month,
        digits(&text[..2])?,
        digits(&text[12..14])?,
        digits(&text[15..17])?,
        digits(&text[18..20])?,
    )?;

    // The zone is how far the written time is ahead of UTC.

    let ahead = (digits(&text[22..24])? * 60 + digits(&text[24..26])?) * 60;

    let seconds = match text[21] {
        b'+' => seconds.checked_sub(ahead)?,
        b'-' => seconds + ahead,
        _ => return None,
    };

    Some(seconds * 1000)
}

/// The Unix time of a date and a time of day in UTC, in seconds, for a
/// date from 1970 on.
fn civil_seconds(
    year: u64,
    month: u64,
    day: u64,
    hour: u64,
    minute: u64,
    second: u64,
) -> Option<u64> {
    if year < 1970 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    // Days from the civil date, counting years from March so that the
    // leap day is the last of its year.

    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = (era * 146_097 + day_of_era).checked_sub(719_468)?;

    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// What is found looking forward from a place in a log.
enum Found {
    /// A line with a time: where it starts, and its time.
    Timed(u64, u64),
    /// Nothing but NUL bytes, as a file cut in place under a writer that
    /// does not append has at its start, up to this offset.
    Hole(u64),
    /// No line with a time within the distance looked.
    Nothing,
}

/// Look forward from `from`, which need not be the start of a line, for
/// the first line that starts at or after it and carries a time, as far
/// as `limit` bytes on.
fn look<F: Read + Seek>(file: &mut F, from: u64, limit: u64) -> io::Result<Found> {
    // A line starts at `from` when the byte before it ends one.

    let back = u64::from(from > 0);
    file.seek(SeekFrom::Start(from - back))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file.take(limit + back));
    let mut offset = from - back;
    let mut line = Vec::new();
    let mut whole = from == 0;
    let mut hole = true;

    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)? as u64;

        if read == 0 {
            break;
        }

        hole = hole && line.iter().all(|byte| *byte == 0);

        // A line is judged only when all of it was read from its start,
        // less any NUL bytes of a hole that ends where it begins.

        let (nul, text) = past_hole(&line);

        if (whole || nul > 0)
            && line.ends_with(b"\n")
            && let Some(time) = line_time(text)
        {
            return Ok(Found::Timed(offset + nul, time));
        }

        whole = line.ends_with(b"\n");
        offset += read;
    }

    Ok(if hole && offset > from {
        Found::Hole(offset)
    } else {
        Found::Nothing
    })
}

/// A line less the NUL bytes it starts with, and how many there were: a
/// hole in a file runs straight into the first line written after it.
fn past_hole(line: &[u8]) -> (u64, &[u8]) {
    let nul = line.iter().take_while(|byte| **byte == 0).count();
    (nul as u64, &line[nul..])
}

/// Where the window starts in a log `length` bytes long: the offset of
/// the first line whose time is at or after `since`, or `length` when
/// every line is older. Lines are taken to be in the order of their
/// times, as a log's are, and a line without a time belongs with the one
/// before it.
///
/// The file is halved until the place is within a few thousand lines,
/// each step reading only as far as the next line with a time, and the
/// rest is read line by line.
pub fn window_start<F: Read + Seek>(
    file: &mut F,
    length: u64,
    since_unix_ms: u64,
) -> io::Result<u64> {
    // Every line with a time that starts before `low` is older than the
    // window, and the window's first line starts at or before the first
    // line with a time at or after `high`.

    let mut low = 0;
    let mut high = length;

    while high - low > STEP {
        let middle = low + (high - low) / 2;

        match look(file, middle, LOOK.min(high - middle))? {
            Found::Timed(offset, time) if time < since_unix_ms => low = offset,
            Found::Hole(end) => low = end,
            Found::Timed(..) | Found::Nothing => high = middle,
        }
    }

    // From a line that is known to be older, or the start of the file,
    // the lines are read until the first of the window.

    file.seek(SeekFrom::Start(low))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file.take(length - low));
    let mut offset = low;
    let mut line = Vec::new();

    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)? as u64;

        if read == 0 {
            return Ok(length);
        }

        let (nul, text) = past_hole(&line);

        if line_time(text).is_some_and(|time| time >= since_unix_ms) {
            return Ok(offset + nul);
        }

        offset += read;
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write as _};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// 2026-11-02T05:42:22Z.
    const BASE: u64 = 1_793_598_142;

    fn stamp(unix_ms: u64) -> String {
        muru_common::log::full(UNIX_EPOCH + Duration::from_millis(unix_ms))
    }

    #[test]
    fn a_lines_time_is_read_in_each_form_a_log_writes() {
        assert_eq!(
            line_time(b"2026-11-02T05:42:22.836Z muru.worker[0]: started\n"),
            Some(BASE * 1000 + 836)
        );
        assert_eq!(
            line_time(br#"{"time":"2026-11-02T05:42:22.836Z","client":"10.0.0.9"}"#),
            Some(BASE * 1000 + 836)
        );
        assert_eq!(
            line_time(br#"10.0.0.9 - - [02/Nov/2026:16:42:22 +1100] "GET / HTTP/1.1" 200 5"#),
            Some(BASE * 1000)
        );
        assert_eq!(
            line_time(br#"10.0.0.9 - - [01/Nov/2026:22:12:22 -0730] "GET / HTTP/1.1" 200 5"#),
            Some(BASE * 1000)
        );
        assert_eq!(line_time(b"1970-01-01T00:00:00.000Z x"), Some(0));
        assert_eq!(
            line_time(b"2024-02-29T00:00:00.000Z x"),
            Some(1_709_164_800_000)
        );

        for line in [
            &b"  File \"/srv/shop/views.py\", line 3, in view\n"[..],
            b"2026-13-02T05:42:22.836Z no such month",
            b"2026-11-02 05:42:22.836Z no T",
            b"{\"client\":\"10.0.0.9\"}",
            b"[not a time] at all",
            b"",
        ] {
            assert_eq!(line_time(line), None, "{}", String::from_utf8_lossy(line));
        }

        // What Muru writes in front of a line is what is read back.

        assert_eq!(
            line_time(stamp(BASE * 1000 + 7).as_bytes()),
            Some(BASE * 1000 + 7)
        );
    }

    #[test]
    fn a_window_is_the_last_period_and_what_follows() {
        let now = UNIX_EPOCH + Duration::from_secs(BASE);
        let window = Window::last(Duration::from_secs(3600), now);

        assert_eq!(window.since_unix_ms, (BASE - 3600) * 1000);
        assert_eq!(window.until_unix_ms, BASE * 1000);
        assert!(window.holds((BASE - 3600) * 1000));
        assert!(!window.holds((BASE - 3601) * 1000));
        assert!(window.holds((BASE + 5) * 1000));
    }

    /// A log of `lines` lines a second apart from `BASE`, every third
    /// followed by a line without a time.
    fn log(lines: u64) -> (Vec<u8>, Vec<u64>) {
        let mut text = Vec::new();
        let mut starts = Vec::new();

        for at in 0..lines {
            starts.push(text.len() as u64);
            writeln!(
                text,
                "{} muru.worker[0]: request {at}",
                stamp((BASE + at) * 1000)
            )
            .unwrap();

            if at % 3 == 0 {
                writeln!(text, "  continued from request {at}").unwrap();
            }
        }

        (text, starts)
    }

    #[test]
    fn the_window_starts_at_its_first_line() {
        let (text, starts) = log(5000);
        let length = text.len() as u64;
        let mut file = Cursor::new(text);
        let at = |file: &mut Cursor<Vec<u8>>, second: u64| {
            window_start(file, length, (BASE + second) * 1000).unwrap()
        };

        for second in [0, 1, 2, 3, 700, 2499, 2500, 4998, 4999] {
            assert_eq!(at(&mut file, second), starts[second as usize], "{second}");
        }

        // A time between two lines starts at the later, one before the
        // log at its start, and one after it at its end.

        assert_eq!(
            window_start(&mut file, length, (BASE + 10) * 1000 + 1).unwrap(),
            starts[11]
        );
        assert_eq!(window_start(&mut file, length, 0).unwrap(), 0);
        assert_eq!(at(&mut file, 5000), length);

        // An empty log, and one with no time on any line.

        assert_eq!(window_start(&mut Cursor::new(Vec::new()), 0, 5).unwrap(), 0);
        let plain = b"one\ntwo\nthree\n".to_vec();
        assert_eq!(
            window_start(&mut Cursor::new(plain.clone()), plain.len() as u64, 5).unwrap(),
            plain.len() as u64
        );
    }

    /// A reader that counts what is read through it.
    struct Counted<F> {
        inner: F,
        read: &'static AtomicU64,
    }

    impl<F: Read> Read for Counted<F> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buffer)?;
            self.read.fetch_add(read as u64, Ordering::Relaxed);
            Ok(read)
        }
    }

    impl<F: Seek> Seek for Counted<F> {
        fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
            self.inner.seek(to)
        }
    }

    #[test]
    fn a_large_log_is_not_read_to_find_the_window() {
        static READ: AtomicU64 = AtomicU64::new(0);

        // Some 16 MiB of lines, generated here, with the window in the
        // last tenth.

        let lines = 250_000;
        let (text, starts) = log(lines);
        let length = text.len() as u64;
        assert!(length > 12 * 1024 * 1024);

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("access.log");
        std::fs::write(&path, &text).unwrap();
        drop(text);

        let mut file = Counted {
            inner: std::fs::File::open(&path).unwrap(),
            read: &READ,
        };
        let first = lines - lines / 10;
        let start = window_start(&mut file, length, (BASE + first) * 1000).unwrap();

        assert_eq!(start, starts[first as usize]);
        assert!(
            READ.load(Ordering::Relaxed) < 1024 * 1024,
            "{} bytes were read of {length}",
            READ.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn a_hole_and_a_long_stretch_without_times_are_passed_over() {
        // A file cut in place under a writer that kept its offset starts
        // with NUL bytes, which are older than anything.

        let (text, starts) = log(3000);
        let hole = 8 * 1024 * 1024;
        let mut holed = vec![0u8; hole];
        holed.extend_from_slice(&text);
        let length = holed.len() as u64;

        let mut holed = Cursor::new(holed);

        for second in [0, 1, 2000] {
            assert_eq!(
                window_start(&mut holed, length, (BASE + second) * 1000).unwrap(),
                hole as u64 + starts[second as usize],
                "{second}"
            );
        }

        // A traceback longer than is looked through sits between two
        // lines with times: the window after it starts after it.

        let mut long = Vec::new();
        writeln!(long, "{} before", stamp(BASE * 1000)).unwrap();

        for _ in 0..40_000 {
            writeln!(long, "  File \"views.py\", line 3, in view").unwrap();
        }

        let after = long.len() as u64;
        writeln!(long, "{} after", stamp((BASE + 60) * 1000)).unwrap();
        let length = long.len() as u64;

        assert_eq!(
            window_start(&mut Cursor::new(long), length, (BASE + 30) * 1000).unwrap(),
            after
        );
    }
}
