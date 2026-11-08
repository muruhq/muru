//! Lines the host and its workers wrote, kept for the dashboard, and the
//! log files a daemon tails for `muructl logs` (section 11.2).

use std::collections::VecDeque;
use std::io::SeekFrom;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// How many lines are kept.
const CAPACITY: usize = 2000;

/// A bounded ring of recent log lines, shared between the relay tasks that
/// fill it and the dashboard that reads it.
#[derive(Clone)]
pub struct Logs {
    lines: Arc<Mutex<VecDeque<String>>>,
    /// Each line as it arrives, for a follower such as `muructl logs`
    /// under a development run (section 13.1).
    live: tokio::sync::broadcast::Sender<String>,
}

impl Default for Logs {
    fn default() -> Self {
        Self {
            lines: Arc::default(),
            live: tokio::sync::broadcast::channel(CAPACITY).0,
        }
    }
}

impl Logs {
    /// Keep a line that arrived at `time`, dropping the oldest when full.
    /// It is kept as a log file holds it, the full time in front (section
    /// 11.8), so readers show or hide the time as they would a file's.
    pub fn push(&self, time: SystemTime, line: &str) {
        let line = format!("{} {line}", muru_common::log::full(time));
        let mut lines = self.lines.lock().expect("logs poisoned");

        if lines.len() == CAPACITY {
            lines.pop_front();
        }

        if self.live.receiver_count() > 0 {
            let _ = self.live.send(line.clone());
        }

        lines.push_back(line);
    }

    /// Every line from now on, for a follower; one that falls behind by
    /// more than the ring holds skips the lines it missed.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.live.subscribe()
    }

    /// The most recent lines, oldest first.
    pub fn recent(&self, limit: usize) -> Vec<String> {
        let lines = self.lines.lock().expect("logs poisoned");
        lines.iter().rev().take(limit).rev().cloned().collect()
    }
}

/// How many bytes of a file's end are read for the first lines shown.
const TAIL_BYTES: u64 = 256 * 1024;

/// How often a followed file is checked for new lines.
const POLL: Duration = Duration::from_millis(200);

/// The last `lines` lines of a log file, then, with `follow`, every line
/// appended to it, as a stream. A file renamed away and created again,
/// which is what an external rotation does, is followed from the start of
/// the new one, and a missing file is waited for. The stream ends when
/// the reader drops it, or after the last line when not following.
pub fn tail(path: PathBuf, lines: usize, follow: bool) -> ReceiverStream<String> {
    let (sender, receiver) = mpsc::channel(256);
    tokio::spawn(tail_into(path, lines, follow, sender));
    ReceiverStream::new(receiver)
}

async fn tail_into(path: PathBuf, lines: usize, follow: bool, sender: mpsc::Sender<String>) {
    let mut position = 0;
    let mut inode = None;

    // The end of the file as it is now, line by line from the back.

    if let Ok(metadata) = tokio::fs::metadata(&path).await {
        for line in last_lines(&path, lines).await {
            if sender.send(line).await.is_err() {
                return;
            }
        }

        position = metadata.len();
        inode = Some(metadata.ino());
    }

    if !follow {
        return;
    }

    let mut partial = String::new();

    loop {
        tokio::time::sleep(POLL).await;

        if sender.is_closed() {
            return;
        }

        let Ok(metadata) = tokio::fs::metadata(&path).await else {
            continue;
        };

        // A new inode or a shorter file is a rotated or truncated log,
        // read from its start.

        if inode != Some(metadata.ino()) || metadata.len() < position {
            position = 0;
            inode = Some(metadata.ino());
            partial.clear();
        }

        if metadata.len() == position {
            continue;
        }

        let Ok((text, read)) = read_from(&path, position).await else {
            continue;
        };
        position += read;
        partial.push_str(&text);

        while let Some(end) = partial.find('\n') {
            let line = partial[..end].to_owned();
            partial.replace_range(..=end, "");

            if sender.send(line).await.is_err() {
                return;
            }
        }
    }
}

/// The last `lines` lines of a file, oldest first, from its last
/// `TAIL_BYTES`; none for a file that cannot be read.
pub async fn last_lines(path: &Path, lines: usize) -> Vec<String> {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return Vec::new();
    };

    let start = metadata.len().saturating_sub(TAIL_BYTES);

    let Ok((text, _)) = read_from(path, start).await else {
        return Vec::new();
    };

    let mut recent: Vec<&str> = text.lines().collect();

    // A read that started mid-file begins with a partial line.

    if start > 0 && !recent.is_empty() {
        recent.remove(0);
    }

    let skip = recent.len().saturating_sub(lines);
    recent.into_iter().skip(skip).map(str::to_owned).collect()
}

/// How long a reason taken from a log may be.
const REASON_CHARS: usize = 400;

/// Why a host that did not get its workers ready failed, from what it
/// wrote to its log after `position`: the host's own line saying it could
/// not load the application, a worker's saying it failed to start, or the
/// last line of a traceback. None when the log says nothing of the kind.
pub async fn start_failure(path: &Path, position: u64) -> Option<String> {
    let (text, _) = read_from(path, position).await.ok()?;
    let lines: Vec<&str> = text.lines().collect();

    failure_line(&lines).map(|line| line.chars().take(REASON_CHARS).collect())
}

/// The line of `lines` that says why a start failed, as in `start_failure`.
fn failure_line<'a>(lines: &[&'a str]) -> Option<&'a str> {
    let after = |marker: &str, keep: bool| {
        lines.iter().rev().find_map(|line| {
            line.find(marker).map(|at| {
                if keep {
                    &line[at..]
                } else {
                    &line[at + marker.len()..]
                }
            })
        })
    };

    after("cannot load ", true)
        .or_else(|| after("failed to start: ", false))
        .or_else(|| lines.iter().rev().find_map(|line| exception(line)))
}

/// The exception a traceback's last line names, from its class onwards,
/// past anything written in front of it; none for any other line.
fn exception(line: &str) -> Option<&str> {
    let class_end = line.find(": ").unwrap_or(line.len());
    let class_start = line[..class_end]
        .rfind(char::is_whitespace)
        .map_or(0, |at| at + 1);
    let class = &line[class_start..class_end];

    (class.ends_with("Error") || class.ends_with("Exception")).then(|| &line[class_start..])
}

/// The file from `position` to its end as text, and how many bytes that was.
async fn read_from(path: &Path, position: u64) -> std::io::Result<(String, u64)> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(SeekFrom::Start(position)).await?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        bytes.len() as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::failure_line;

    #[test]
    fn a_failed_start_is_explained_by_the_line_that_says_why() {
        assert_eq!(
            failure_line(&[
                "2026-01-01T10:00:00Z muru.host[1]: cannot load app:application: KeyError: 'GREETING'",
                "muru.host[1]: stopped",
            ]),
            Some("cannot load app:application: KeyError: 'GREETING'"),
        );
        assert_eq!(
            failure_line(&["worker 0 failed to start: ImportError: no such module"]),
            Some("ImportError: no such module"),
        );
        assert_eq!(
            failure_line(&[
                "Traceback (most recent call last):",
                "  File \"app.py\", line 1, in <module>",
                "10:00:00 ModuleNotFoundError: No module named 'markdown'",
            ]),
            Some("ModuleNotFoundError: No module named 'markdown'"),
        );
        assert_eq!(failure_line(&["muru.host[1]: starting"]), None);
    }
}
