//! A release's log relay (design specification section 11.3, ADR 0059):
//! the process, run as `muructl relay run`, that reads the output of a
//! host and its workers and writes it to the release log, or under
//! `muructl run` and `muructl serve` to muructl's pipe.
//!
//! The host keeps the read end of every worker's pipes and never reads
//! them. It listens on `output.sock` in its run directory and sends a
//! relay that connects every read end it holds, one per message with
//! SCM_RIGHTS, and each new worker's as it forks; the relay says on the
//! same connection when it has read a pipe to its end, so the host can
//! close its copy. The host's own standard output and error are one more
//! pipe, whose read end the first relay inherits.
//!
//! Reading runs on a current-thread runtime, a task a pipe. Each line is
//! stamped with the time it was read and tagged with its worker and
//! stream, and goes into its worker's queue, bounded in bytes. A writer
//! thread drains the queues in turn into writes of up to 64 KiB, so a
//! destination that stalls holds up that thread alone: under
//! `drop_oldest` a full queue drops its oldest lines, and the writer
//! later says how many where they were; under `block` the relay stops
//! reading a worker whose queue is full. The relay reports its counts
//! and stalls on `relay.sock`, a control socket of its own.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::os::fd::{OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast, mpsc, watch};

/// The socket a host listens on for its relay, in its run directory.
pub const OUTPUT_SOCKET: &str = "output.sock";

/// The relay's control socket, beside the host's.
pub const CONTROL_SOCKET: &str = "relay.sock";

/// The record of a release's relay, beside the host's, for adoption.
pub const RECORD: &str = "relay.toml";

/// The descriptor a relay or a host is given the read end of the host's
/// own output pipe on.
pub const HOST_OUTPUT_FD: RawFd = 3;

/// A queue's bound when nothing sets one: 1 MiB.
pub const DEFAULT_QUEUE: usize = 1024 * 1024;

/// The smallest queue that is accepted, so a single long line fits.
pub const MIN_QUEUE: usize = 64 * 1024;

/// The longest line kept whole; a longer one is cut here, with a marker.
pub const MAX_LINE: usize = 64 * 1024;

/// The most one write to the destination gathers.
const BATCH: usize = 64 * 1024;

/// How much of one queue the writer takes on its turn before moving on
/// to the next, unless one entry is larger.
const QUANTUM: usize = 16 * 1024;

/// How much is read from a pipe at once.
const READ_SIZE: usize = 64 * 1024;

/// How long, once the host has gone, the relay keeps reading pipes that
/// have not reached their end (a process a worker left behind may hold
/// one open).
const DRAIN_AFTER_HOST: Duration = Duration::from_secs(5);

/// How long SIGTERM gives the writer to write what the relay holds.
const TERM_GRACE: Duration = Duration::from_secs(2);

/// How long a relay that has not inherited the host's stream tries to
/// reach a host that is not listening before it gives up.
const CONNECT_GIVE_UP: Duration = Duration::from_secs(10);

/// The longest wait between tries to reach a host that is not listening
/// yet: a worker's first lines wait in its pipe until the relay connects.
const CONNECT_RETRY_MAX: Duration = Duration::from_millis(25);

/// What a full queue does (`[logs] on_overflow`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overflow {
    /// Drop the queue's oldest lines to make room.
    #[default]
    DropOldest,
    /// Stop reading the worker's pipes until the queue has room.
    Block,
}

impl Overflow {
    /// The setting's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DropOldest => "drop_oldest",
            Self::Block => "block",
        }
    }

    /// Parse the setting's spelling.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "drop_oldest" => Ok(Self::DropOldest),
            "block" => Ok(Self::Block),
            other => Err(format!(
                "on_overflow = {other:?} is not drop_oldest or block"
            )),
        }
    }
}

/// What a relay is started with.
#[derive(Debug, Clone)]
pub struct RelayArgs {
    /// The host's output socket, to connect to.
    pub connect: PathBuf,
    /// The control socket to serve.
    pub control: PathBuf,
    /// The read end of the host's own output, when inherited.
    pub host_output_fd: Option<RawFd>,
    /// The release log to write, opened again by name on SIGUSR1; none
    /// writes to standard error.
    pub log_file: Option<PathBuf>,
    /// Each queue's bound in bytes.
    pub queue: usize,
    /// What a full queue does.
    pub overflow: Overflow,
}

/// Where a line came from, which is also its queue. The relay's own
/// lines sort first, then the host's, then the workers' by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Relay,
    Host,
    Worker(u32),
}

impl Source {
    /// The words a gap line uses for what was dropped.
    fn whose(self) -> String {
        match self {
            Self::Relay => "the relay's own output".to_owned(),
            Self::Host => "the host's output".to_owned(),
            Self::Worker(index) => format!("worker {index}'s output"),
        }
    }
}

/// One item of a queue.
enum Entry {
    /// Stamped, tagged lines, each ending in a newline.
    Text { bytes: Vec<u8>, lines: u64 },
    /// Lines dropped from here.
    Gap(u64),
}

/// One source's queue.
#[derive(Default)]
struct Queue {
    entries: VecDeque<Entry>,
    bytes: usize,
    room: Arc<Notify>,
}

/// What kind of stall is in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StallKind {
    Dropping,
    Holding,
    Failing,
}

/// A stall in progress: from the first line dropped or held back, or the
/// first write that failed, until every queue is empty again.
struct Stall {
    since: Instant,
    dropped_before: u64,
}

/// What the relay reports on its control socket whenever it changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Status {
    /// Lines dropped since this relay started, for a full queue or a
    /// failed write.
    pub dropped: u64,
    /// Lines waiting in the queues.
    pub queued_lines: u64,
    /// Bytes waiting in the queues.
    pub queued_bytes: u64,
    /// Pipes being read.
    pub pipes: u64,
    /// `dropping`, `holding` or `failing` while a stall lasts, else none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall: Option<String>,
}

/// An event the relay reports on its control socket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Event {
    /// The event's kind: `output.dropping`, `output.held`,
    /// `output.failing` or `output.caught_up`.
    pub kind: String,
    /// `warning` or `info`.
    pub level: String,
    /// What happened, in words.
    pub message: String,
}

/// A line on the relay's control socket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Report {
    /// The relay's counts.
    Status(Status),
    /// A stall began or ended.
    Event(Event),
}

/// The state the readers and the writer share.
struct State {
    queues: BTreeMap<Source, Queue>,
    /// The source served last, so the next turn starts after it.
    cursor: Option<Source>,
    limit: usize,
    overflow: Overflow,
    dropped: u64,
    queued_lines: u64,
    queued_bytes: usize,
    pipes: u64,
    stall: Option<(StallKind, Stall)>,
    /// No more lines will come: the writer writes what is left and ends.
    finished: bool,
}

/// The readers' and the writer's meeting point.
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    reopen: AtomicBool,
    status: watch::Sender<Status>,
    events: broadcast::Sender<Event>,
    tag: String,
}

impl Shared {
    fn new(queue: usize, overflow: Overflow) -> Self {
        Self {
            state: Mutex::new(State {
                queues: BTreeMap::new(),
                cursor: None,
                limit: queue.max(MIN_QUEUE),
                overflow,
                dropped: 0,
                queued_lines: 0,
                queued_bytes: 0,
                pipes: 0,
                stall: None,
                finished: false,
            }),
            ready: Condvar::new(),
            reopen: AtomicBool::new(false),
            status: watch::channel(Status::default()).0,
            events: broadcast::channel(16).0,
            tag: format!("muru.relay[{}]: ", std::process::id()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The notifier a reader of `source` waits on for room.
    fn room(&self, source: Source) -> Arc<Notify> {
        Arc::clone(&self.lock().queues.entry(source).or_default().room)
    }

    /// Whether `source`'s queue has room for more, under `block`; a
    /// queue that has none starts a stall when none is in progress.
    fn has_room(&self, source: Source) -> bool {
        let mut state = self.lock();

        if state.overflow != Overflow::Block {
            return true;
        }

        let limit = state.limit;
        let full = state
            .queues
            .get(&source)
            .is_some_and(|queue| queue.bytes >= limit);

        if full && state.stall.is_none() {
            let dropped = state.dropped;
            state.stall = Some((
                StallKind::Holding,
                Stall {
                    since: Instant::now(),
                    dropped_before: dropped,
                },
            ));
            drop(state);

            self.event(
                "output.held",
                "warning",
                format!(
                    "{} is held on its output: the log is not keeping up",
                    match source {
                        Source::Worker(index) => format!("worker {index}"),
                        _ => "the host".to_owned(),
                    }
                ),
            );
            self.publish();
        }

        !full
    }

    /// Queue an entry of `lines` lines for `source`, dropping the queue's
    /// oldest under `drop_oldest` to make room.
    fn push(&self, source: Source, bytes: Vec<u8>, lines: u64) {
        let mut state = self.lock();
        let limit = state.limit;
        let overflow = state.overflow;
        let size = bytes.len();
        let queue = state.queues.entry(source).or_default();
        let mut dropped = 0u64;
        let mut carried = 0u64;
        let mut freed = 0usize;

        if overflow == Overflow::DropOldest {
            while queue.bytes + size > limit {
                match queue.entries.pop_front() {
                    Some(Entry::Text { bytes, lines }) => {
                        queue.bytes -= bytes.len();
                        freed += bytes.len();
                        dropped += lines;
                    }
                    Some(Entry::Gap(count)) => carried += count,
                    None => break,
                }
            }

            // The gap stands where the dropped lines were, ahead of what
            // is left, merged with a gap already there.

            if dropped + carried > 0 {
                queue.entries.push_front(Entry::Gap(dropped + carried));
            }
        }

        queue.bytes += size;
        queue.entries.push_back(Entry::Text { bytes, lines });

        state.queued_bytes = state.queued_bytes + size - freed;
        state.queued_lines = (state.queued_lines + lines).saturating_sub(dropped);
        state.dropped += dropped;

        let started = dropped > 0 && state.stall.is_none();

        if started {
            let before = state.dropped - dropped;
            state.stall = Some((
                StallKind::Dropping,
                Stall {
                    since: Instant::now(),
                    dropped_before: before,
                },
            ));
        }

        drop(state);
        self.ready.notify_one();

        if started {
            self.event(
                "output.dropping",
                "warning",
                format!(
                    "the oldest lines of {} are being dropped: the log is not keeping up",
                    source.whose()
                ),
            );
        }

        if dropped > 0 || started {
            self.publish();
        }
    }

    /// Queue one of the relay's own lines.
    fn note(&self, message: &str) {
        let line = format!(
            "{} {}{message}\n",
            muru_common::log::full(SystemTime::now()),
            self.tag
        );
        self.push(Source::Relay, line.into_bytes(), 1);
    }

    /// Take up to `BATCH` bytes from the queues, a turn of each in order,
    /// waiting while there is nothing; none once the relay has finished
    /// and every queue is empty. Gaps become lines saying what was
    /// dropped. Gives the batch and how many lines it holds.
    fn take(&self) -> Option<(Vec<u8>, u64)> {
        let mut state = self.lock();

        while state.queued_bytes == 0 && !gaps_waiting(&state) {
            if state.finished {
                return None;
            }

            if self.reopen.load(Ordering::Relaxed) {
                return Some((Vec::new(), 0));
            }

            state = self
                .ready
                .wait_timeout(state, Duration::from_millis(500))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }

        let mut batch = Vec::with_capacity(BATCH);
        let mut lines = 0u64;
        let mut queued = 0u64;
        let mut freed = 0usize;
        let mut served = Vec::new();
        let tag = self.tag.clone();

        // Turns go round the sources, starting after the one served last,
        // until the batch is full or every queue is empty.

        while batch.len() < BATCH {
            let order: Vec<Source> = turn_order(&state);

            if order.is_empty() {
                break;
            }

            let mut progress = false;

            for source in order {
                let Some(queue) = state.queues.get_mut(&source) else {
                    continue;
                };
                let mut taken = 0usize;

                while let Some(entry) = queue.entries.front() {
                    let size = match entry {
                        Entry::Text { bytes, .. } => bytes.len(),
                        Entry::Gap(_) => 0,
                    };

                    if (taken > 0 && taken + size > QUANTUM)
                        || (!batch.is_empty() && batch.len() + size > BATCH)
                    {
                        break;
                    }

                    match queue.entries.pop_front() {
                        Some(Entry::Text {
                            bytes,
                            lines: count,
                        }) => {
                            queue.bytes -= bytes.len();
                            freed += bytes.len();
                            taken += bytes.len();
                            lines += count;
                            queued += count;
                            batch.extend_from_slice(&bytes);
                        }
                        Some(Entry::Gap(count)) => {
                            let line = format!(
                                "{} {tag}dropped {count} line{} of {} here: the log was not \
                                 keeping up\n",
                                muru_common::log::full(SystemTime::now()),
                                if count == 1 { "" } else { "s" },
                                source.whose()
                            );
                            taken += line.len();
                            lines += 1;
                            batch.extend_from_slice(line.as_bytes());
                        }
                        None => break,
                    }
                }

                if taken > 0 {
                    served.push(Arc::clone(&queue.room));
                    state.cursor = Some(source);
                    progress = true;
                }

                if batch.len() >= BATCH {
                    break;
                }
            }

            // Whatever is next is too big for what is left of this write.

            if !progress {
                break;
            }
        }

        state.queued_bytes -= freed;
        state.queued_lines = state.queued_lines.saturating_sub(queued);
        drop(state);

        // A reader held back under `block` may read again.

        for room in served {
            room.notify_one();
        }

        Some((batch, lines))
    }

    /// Count lines a failed write lost, starting a stall if none is in
    /// progress.
    fn lost(&self, lines: u64, why: &str) {
        let mut state = self.lock();
        state.dropped += lines;
        let started = state.stall.is_none();

        if started {
            let before = state.dropped - lines;
            state.stall = Some((
                StallKind::Failing,
                Stall {
                    since: Instant::now(),
                    dropped_before: before,
                },
            ));
        }

        drop(state);

        if started {
            self.event(
                "output.failing",
                "warning",
                format!("the release's output cannot be written: {why}"),
            );
        }

        self.publish();
    }

    /// After a write that succeeded: a stall ends once every queue is
    /// empty, with an event saying how long it lasted and what it cost.
    fn wrote(&self) {
        let mut state = self.lock();

        if state.queued_bytes > 0 || gaps_waiting(&state) {
            return;
        }

        let Some((kind, stall)) = state.stall.take() else {
            return;
        };

        let dropped = state.dropped - stall.dropped_before;
        drop(state);

        let seconds = stall.since.elapsed().as_secs_f64();

        let what = match (kind, dropped) {
            (StallKind::Holding, 0) => "no lines were dropped".to_owned(),
            (_, 1) => "1 line was dropped".to_owned(),
            (_, count) => format!("{count} lines were dropped"),
        };

        self.event(
            "output.caught_up",
            "info",
            format!("the log caught up after {seconds:.1} s; {what}"),
        );
        self.publish();
    }

    /// Change the number of pipes being read.
    fn pipes(&self, change: i64) {
        let mut state = self.lock();
        state.pipes = state.pipes.saturating_add_signed(change);
        drop(state);
        self.publish();
    }

    /// No more lines will come.
    fn finish(&self) {
        self.lock().finished = true;
        self.ready.notify_all();
    }

    /// Tell the control socket's clients the counts as they are now.
    fn publish(&self) {
        let state = self.lock();
        let status = Status {
            dropped: state.dropped,
            queued_lines: state.queued_lines,
            queued_bytes: state.queued_bytes as u64,
            pipes: state.pipes,
            stall: state.stall.as_ref().map(|(kind, _)| {
                match kind {
                    StallKind::Dropping => "dropping",
                    StallKind::Holding => "holding",
                    StallKind::Failing => "failing",
                }
                .to_owned()
            }),
        };
        drop(state);
        self.status.send_replace(status);
    }

    /// Report an event on the control socket and write it as a line of
    /// the relay's own.
    fn event(&self, kind: &str, level: &str, message: String) {
        self.note(&format!("{kind}: {message}"));
        let _ = self.events.send(Event {
            kind: kind.to_owned(),
            level: level.to_owned(),
            message,
        });
    }
}

/// Whether some queue holds a gap still to be written.
fn gaps_waiting(state: &State) -> bool {
    state
        .queues
        .values()
        .any(|queue| matches!(queue.entries.front(), Some(Entry::Gap(_))))
}

/// The sources with something waiting, starting after the one served
/// last.
fn turn_order(state: &State) -> Vec<Source> {
    let waiting: Vec<Source> = state
        .queues
        .iter()
        .filter(|(_, queue)| !queue.entries.is_empty())
        .map(|(source, _)| *source)
        .collect();

    let start = match state.cursor {
        Some(cursor) => waiting
            .iter()
            .position(|source| *source > cursor)
            .unwrap_or(0),
        None => 0,
    };

    waiting[start..]
        .iter()
        .chain(&waiting[..start])
        .copied()
        .collect()
}

/// Where the writer writes.
enum Destination {
    /// The release log, opened again by name on SIGUSR1.
    File { path: PathBuf, file: std::fs::File },
    /// Standard error: muructl's pipe under `muructl run` and `serve`.
    Stderr,
}

impl Destination {
    fn open(log_file: Option<&Path>) -> anyhow::Result<Self> {
        match log_file {
            Some(path) => Ok(Self::File {
                path: path.to_owned(),
                file: open_log(path).with_context(|| format!("opening {}", path.display()))?,
            }),
            None => Ok(Self::Stderr),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Self::File { file, .. } => file.write_all(bytes),
            Self::Stderr => std::io::stderr().lock().write_all(bytes),
        }
    }

    /// Open the file again by name, saying so in the new file. A file
    /// that cannot be opened keeps the old one, and the failure is
    /// written there.
    fn reopen(&mut self, shared: &Shared) {
        let Self::File { path, file } = self else {
            return;
        };

        match open_log(path) {
            Ok(opened) => {
                *file = opened;
                shared.note(&format!("reopened {}", path.display()));
            }
            Err(error) => shared.note(&format!("could not reopen {}: {error}", path.display())),
        }
    }
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o644)
        .open(path)
}

/// The writer thread: take batches and write them, until the relay has
/// finished and the queues are empty.
fn write_loop(shared: &Shared, mut destination: Destination) {
    let mut failed = 0u64;
    let mut failure = String::new();

    while let Some((batch, lines)) = shared.take() {
        // SIGUSR1 asks for the file again, in order with the lines: what
        // was taken before it goes to the old file.

        let reopen = shared.reopen.swap(false, Ordering::Relaxed);

        if !batch.is_empty() {
            match destination.write(&batch) {
                Ok(()) => {
                    if failed > 0 {
                        let line = format!(
                            "{} {}dropped {failed} line{} that could not be written ({failure})\n",
                            muru_common::log::full(SystemTime::now()),
                            shared.tag,
                            if failed == 1 { "" } else { "s" },
                        );

                        if destination.write(line.as_bytes()).is_ok() {
                            failed = 0;
                        }
                    }

                    shared.wrote();
                }
                Err(error) => {
                    failure = error
                        .raw_os_error()
                        .map_or_else(|| error.to_string(), describe_os_error);
                    failed += lines;
                    shared.lost(lines, &failure);
                }
            }
        }

        if reopen {
            destination.reopen(shared);
        }
    }
}

/// An OS error in words, without Rust's `(os error N)`.
fn describe_os_error(code: i32) -> String {
    let text = std::io::Error::from_raw_os_error(code).to_string();

    match text.find(" (os error") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

/// Run a relay until the host and every pipe it was given have ended.
pub fn run(args: RelayArgs) -> anyhow::Result<()> {
    let shared = Arc::new(Shared::new(args.queue, args.overflow));
    let destination = Destination::open(args.log_file.as_deref())?;

    let writer = {
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("muru-relay-writer".to_owned())
            .spawn(move || write_loop(&shared, destination))
            .context("starting the relay's writer")?
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the relay's runtime")?;
    let terminated = runtime.block_on(read_loop(args, Arc::clone(&shared)))?;
    shared.finish();

    // SIGTERM gives the writer a moment; otherwise it writes everything,
    // however long the destination takes.

    if terminated {
        let deadline = Instant::now() + TERM_GRACE;

        while !writer.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        return Ok(());
    }

    let _ = writer.join();
    Ok(())
}

/// What a pipe task says when it ends.
struct Ended {
    pipe: u64,
}

/// A pipe as the host names it.
#[derive(Debug, serde::Deserialize)]
struct Offered {
    pipe: u64,
    #[serde(default)]
    worker: Option<u32>,
    stream: String,
}

/// Read until the host has gone and every pipe has ended; true when
/// SIGTERM ended it instead.
async fn read_loop(args: RelayArgs, shared: Arc<Shared>) -> anyhow::Result<bool> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
    let mut usr1 = signal(SignalKind::user_defined1()).context("installing the SIGUSR1 handler")?;

    // Ctrl-C and a closed terminal are muructl's to act on: the relay
    // ends when the host's output does.

    let _int = signal(SignalKind::interrupt()).context("installing the SIGINT handler")?;
    let _hup = signal(SignalKind::hangup()).context("installing the SIGHUP handler")?;

    serve_control(&args.control, &shared)?;

    let (ended_tx, mut ended_rx) = mpsc::unbounded_channel::<Ended>();
    let mut tasks = tokio::task::JoinSet::new();

    // Pipes being read, by the host's number; the host's own stream is
    // number 0 whether inherited or offered.

    let mut reading: std::collections::HashSet<u64> = std::collections::HashSet::new();

    if let Some(fd) = args.host_output_fd {
        let owned = crate::fdpass::take_inherited(fd);
        reading.insert(0);
        spawn_pipe(&mut tasks, &shared, 0, Source::Host, None, owned, &ended_tx)?;
    }

    let inherited = args.host_output_fd.is_some();
    let started = Instant::now();
    let mut connection: Option<Connection> = None;
    let mut connected_once = false;
    let mut host_gone_at: Option<Instant> = None;
    let mut retry = Duration::from_millis(10);
    let mut next_attempt = Instant::now();

    loop {
        // The relay is done once the host has gone and the pipes have
        // ended, or once they have been given their time.

        if let Some(gone) = host_gone_at
            && (reading.is_empty() || gone.elapsed() >= DRAIN_AFTER_HOST)
        {
            break;
        }

        if host_gone_at.is_none() && connection.is_none() {
            let gave_up = !inherited && !connected_once && started.elapsed() >= CONNECT_GIVE_UP;
            let host_stream_ended = inherited && !reading.contains(&0);

            if gave_up || connected_once || host_stream_ended {
                host_gone_at = Some(Instant::now());
                continue;
            }

            if Instant::now() >= next_attempt {
                match UnixStream::connect(&args.connect).await {
                    Ok(stream) => {
                        connection = Some(Connection::new(stream));
                        connected_once = true;
                        continue;
                    }
                    Err(_) => {
                        next_attempt = Instant::now() + retry;
                        retry = (retry * 2).min(CONNECT_RETRY_MAX);
                    }
                }
            }
        }

        let wait = next_attempt.saturating_duration_since(Instant::now());

        let tick = if host_gone_at.is_some() {
            Duration::from_millis(100)
        } else if connection.is_none() {
            wait.max(Duration::from_millis(5))
        } else {
            Duration::from_secs(1)
        };

        tokio::select! {
            _ = term.recv() => return Ok(true),
            _ = usr1.recv() => {
                shared.reopen.store(true, Ordering::Relaxed);
                shared.ready.notify_all();
            }
            Some(ended) = ended_rx.recv() => {
                reading.remove(&ended.pipe);

                if let Some(connection) = &mut connection {
                    connection.ended(ended.pipe).await;
                }
            }
            offered = next_offer(&mut connection) => match offered {
                Some((offer, fd)) => {
                    if reading.contains(&offer.pipe) {
                        drop(fd);
                        continue;
                    }

                    let source = match offer.worker {
                        Some(index) => Source::Worker(index),
                        None => Source::Host,
                    };

                    let stream = (offer.stream != "host").then_some(offer.stream);

                    if spawn_pipe(&mut tasks, &shared, offer.pipe, source, stream, fd, &ended_tx).is_ok() {
                        reading.insert(offer.pipe);
                    }
                }
                None => connection = None,
            },
            _ = tokio::time::sleep(tick) => {}
        }
    }

    tasks.abort_all();
    Ok(false)
}

/// The connection to the host's output socket. Offers are read on a
/// task of their own, since a read of a line that a `select!` cancels
/// could lose part of it.
struct Connection {
    offers: mpsc::Receiver<(Offered, OwnedFd)>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Connection {
    fn new(stream: UnixStream) -> Self {
        let (read, write) = stream.into_split();
        let (sender, offers) = mpsc::channel(64);

        tokio::spawn(read_offers(
            BufReader::new(muru_proto::descriptors::FdReader::new(read)),
            sender,
        ));

        Self {
            offers,
            writer: write,
        }
    }

    /// Tell the host a pipe has reached its end, so it closes its copy.
    async fn ended(&mut self, pipe: u64) {
        let line = format!("{{\"ended\":{pipe}}}\n");
        let _ = self.writer.write_all(line.as_bytes()).await;
    }
}

/// Read the host's offers until the connection closes. Every offer
/// carries exactly one descriptor, on its first byte, so the oldest one
/// received belongs to the line just read.
async fn read_offers(
    mut reader: BufReader<muru_proto::descriptors::FdReader>,
    offers: mpsc::Sender<(Offered, OwnedFd)>,
) {
    let mut line = String::new();

    loop {
        line.clear();

        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }

        let Ok(offer) = serde_json::from_str::<Offered>(line.trim_end()) else {
            continue;
        };
        let Some(fd) = reader.get_mut().take_fd() else {
            continue;
        };

        if offers.send((offer, fd)).await.is_err() {
            return;
        }
    }
}

/// The next pipe the host offers, with its read end; none once the
/// connection has closed. Waits forever without a connection.
async fn next_offer(connection: &mut Option<Connection>) -> Option<(Offered, OwnedFd)> {
    match connection {
        Some(connection) => connection.offers.recv().await,
        None => std::future::pending().await,
    }
}

/// Start reading one pipe into `source`'s queue.
fn spawn_pipe(
    tasks: &mut tokio::task::JoinSet<()>,
    shared: &Arc<Shared>,
    pipe: u64,
    source: Source,
    stream: Option<String>,
    fd: OwnedFd,
    ended: &mpsc::UnboundedSender<Ended>,
) -> std::io::Result<()> {
    let receiver = tokio::net::unix::pipe::Receiver::from_owned_fd(fd)?;

    let tag = match (source, stream.as_deref()) {
        (Source::Worker(index), Some("stderr") | None) => format!("muru.worker[{index}]: "),
        (Source::Worker(index), Some(stream)) => format!("muru.worker[{index}].{stream}: "),
        _ => String::new(),
    };

    let shared = Arc::clone(shared);
    let ended = ended.clone();
    shared.pipes(1);

    tasks.spawn(async move {
        read_pipe(receiver, &shared, source, &tag).await;
        shared.pipes(-1);
        let _ = ended.send(Ended { pipe });
    });

    Ok(())
}

/// Read a pipe to its end, a read at a time: every complete line read is
/// stamped with the time of the read and tagged, and a partial line waits
/// for the rest, or is written as it is at the end.
async fn read_pipe(
    mut receiver: tokio::net::unix::pipe::Receiver,
    shared: &Shared,
    source: Source,
    tag: &str,
) {
    let room = shared.room(source);
    let mut buffer = vec![0u8; READ_SIZE];
    let mut lines = Lines::new(tag);

    loop {
        // Under `block` a full queue stops the reading here, so the
        // worker's own writes wait and nothing else does.

        loop {
            let notified = room.notified();

            if shared.has_room(source) {
                break;
            }

            notified.await;
        }

        let count = match receiver.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };

        for (bytes, count) in lines.feed(&buffer[..count], SystemTime::now()) {
            shared.push(source, bytes, count);
        }
    }

    if let Some((bytes, count)) = lines.finish(SystemTime::now()) {
        shared.push(source, bytes, count);
    }
}

/// Splits what a pipe gives into stamped, tagged lines, in entries of up
/// to `BATCH` bytes, cutting a line longer than `MAX_LINE`.
struct Lines {
    tag: String,
    partial: Vec<u8>,
    /// The rest of a line already cut is being skipped.
    cutting: bool,
}

impl Lines {
    fn new(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
            partial: Vec::new(),
            cutting: false,
        }
    }

    /// The entries the complete lines in `data` make, all with `time`.
    fn feed(&mut self, data: &[u8], time: SystemTime) -> Vec<(Vec<u8>, u64)> {
        let stamp = muru_common::log::full(time);
        let mut entries = Vec::new();
        let mut entry = Vec::new();
        let mut count = 0u64;
        let mut rest = data;

        while let Some(at) = rest.iter().position(|byte| *byte == b'\n') {
            let (line, after) = rest.split_at(at);
            rest = &after[1..];

            if self.cutting {
                self.cutting = false;
                continue;
            }

            if self.partial.is_empty() {
                self.format(&mut entry, &stamp, line, false);
            } else {
                let mut whole = std::mem::take(&mut self.partial);
                whole.extend_from_slice(line);
                self.format(&mut entry, &stamp, &whole, false);
            }

            count += 1;

            if entry.len() >= BATCH {
                entries.push((std::mem::take(&mut entry), count));
                count = 0;
            }
        }

        // What is left has no newline yet; past the longest line it is
        // written cut, and the rest of the line skipped.

        if !self.cutting {
            self.partial.extend_from_slice(rest);

            if self.partial.len() > MAX_LINE {
                let whole = std::mem::take(&mut self.partial);
                self.format(&mut entry, &stamp, &whole, true);
                count += 1;
                self.cutting = true;
            }
        }

        if count > 0 {
            entries.push((entry, count));
        }

        entries
    }

    /// The last partial line, at the pipe's end.
    fn finish(&mut self, time: SystemTime) -> Option<(Vec<u8>, u64)> {
        if self.partial.is_empty() {
            return None;
        }

        let whole = std::mem::take(&mut self.partial);
        let mut entry = Vec::new();
        self.format(&mut entry, &muru_common::log::full(time), &whole, false);
        Some((entry, 1))
    }

    /// One line: the time, the tag, the line (cut at `MAX_LINE`, with a
    /// marker, when `cut`) and a newline. Bytes that are not UTF-8 are
    /// replaced, so every reader can take the line as text.
    fn format(&self, entry: &mut Vec<u8>, stamp: &str, line: &[u8], cut: bool) {
        let line = if line.len() > MAX_LINE {
            &line[..MAX_LINE]
        } else {
            line
        };

        entry.extend_from_slice(stamp.as_bytes());
        entry.push(b' ');
        entry.extend_from_slice(self.tag.as_bytes());

        match std::str::from_utf8(line) {
            Ok(_) => entry.extend_from_slice(line),
            Err(_) => entry.extend_from_slice(String::from_utf8_lossy(line).as_bytes()),
        }

        if cut {
            entry.extend_from_slice(b" [cut: the line was longer than 64 KiB]");
        }

        entry.push(b'\n');
    }
}

/// Serve the control socket: each client gets the status at once and
/// whenever it changes, at most once a second, and every event.
fn serve_control(path: &Path, shared: &Arc<Shared>) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    let shared = Arc::clone(shared);

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let status = shared.status.subscribe();
            let events = shared.events.subscribe();
            tokio::spawn(report_to(stream, status, events));
        }
    });

    Ok(())
}

/// Write reports to one control client until it goes away.
async fn report_to(
    mut stream: UnixStream,
    mut status: watch::Receiver<Status>,
    mut events: broadcast::Receiver<Event>,
) {
    let mut last_sent = Instant::now() - Duration::from_secs(1);
    status.mark_changed();

    loop {
        tokio::select! {
            changed = status.changed() => {
                if changed.is_err() {
                    return;
                }

                // At most once a second; what changes meanwhile is sent
                // with the next.

                let wait = Duration::from_secs(1).saturating_sub(last_sent.elapsed());
                tokio::time::sleep(wait).await;
                let current = status.borrow_and_update().clone();
                last_sent = Instant::now();

                if send(&mut stream, &Report::Status(current)).await.is_err() {
                    return;
                }
            }
            event = events.recv() => match event {
                Ok(event) => {
                    if send(&mut stream, &Report::Event(event)).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

async fn send(stream: &mut UnixStream, report: &Report) -> std::io::Result<()> {
    let mut line = serde_json::to_string(report).map_err(std::io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await
}

/// Read a relay's reports from its control socket, handing each to
/// `report`, until the relay goes away.
pub async fn watch_reports(path: &Path, mut report: impl FnMut(Report)) -> std::io::Result<()> {
    let stream = UnixStream::connect(path).await?;
    let mut lines = BufReader::new(stream).lines();

    while let Some(line) = lines.next_line().await? {
        if let Ok(parsed) = serde_json::from_str::<Report>(&line) {
            report(parsed);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(entries: &[(Vec<u8>, u64)]) -> String {
        entries
            .iter()
            .map(|(bytes, _)| String::from_utf8_lossy(bytes).into_owned())
            .collect()
    }

    #[test]
    fn lines_are_stamped_tagged_and_kept_whole_across_reads() {
        let mut lines = Lines::new("muru.worker[0]: ");
        let time = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);

        let first = lines.feed(b"one\ntw", time);
        let second = lines.feed(b"o\n", time);

        assert_eq!(
            text(&first),
            "2023-11-14T22:13:20.123Z muru.worker[0]: one\n"
        );
        assert_eq!(
            text(&second),
            "2023-11-14T22:13:20.123Z muru.worker[0]: two\n"
        );
        assert!(lines.finish(time).is_none());
    }

    #[test]
    fn a_line_past_the_longest_is_cut_with_a_marker_and_its_rest_skipped() {
        let mut lines = Lines::new("");
        let time = SystemTime::UNIX_EPOCH;
        let long = vec![b'x'; MAX_LINE + 10];

        let cut = lines.feed(&long, time);
        let after = lines.feed(b"yyy\nnext\n", time);

        let cut = text(&cut);
        assert!(
            cut.ends_with("x [cut: the line was longer than 64 KiB]\n"),
            "{cut}"
        );
        assert_eq!(cut.matches('x').count(), MAX_LINE);
        assert_eq!(text(&after), "1970-01-01T00:00:00.000Z next\n");
    }

    #[test]
    fn bytes_that_are_not_utf8_are_replaced() {
        let mut lines = Lines::new("");
        let entries = lines.feed(b"a\xffb\n", SystemTime::UNIX_EPOCH);

        assert!(text(&entries).ends_with("a\u{fffd}b\n"));
    }

    fn entry(text: &str) -> (Vec<u8>, u64) {
        (text.as_bytes().to_vec(), 1)
    }

    fn drain(shared: &Shared) -> String {
        shared.finish();
        let mut out = String::new();

        while let Some((batch, _)) = shared.take() {
            out.push_str(&String::from_utf8_lossy(&batch));
        }

        out
    }

    #[test]
    fn a_full_queue_drops_its_oldest_lines_and_says_where() {
        let shared = Shared::new(MIN_QUEUE, Overflow::DropOldest);
        let line = "x".repeat(1023) + "\n";

        for _ in 0..80 {
            let (bytes, lines) = entry(&line);
            shared.push(Source::Worker(3), bytes, lines);
        }

        let (bytes, lines) = entry("last\n");
        shared.push(Source::Worker(3), bytes, lines);
        assert_eq!(shared.lock().dropped, 17);

        let out = drain(&shared);
        let gap = out
            .lines()
            .position(|line| line.contains("dropped 17 lines of worker 3's output here"))
            .expect("a gap line");
        let first_kept = out.lines().position(|line| line.starts_with('x')).unwrap();

        // The relay's own line about the stall comes first, then the gap,
        // then what was kept.

        assert!(gap < first_kept, "{out}");
        assert!(out.lines().next().unwrap().contains("output.dropping"));
        assert!(out.ends_with("last\n"));
    }

    #[test]
    fn a_quiet_worker_is_not_kept_behind_a_noisy_ones_backlog() {
        let shared = Shared::new(DEFAULT_QUEUE, Overflow::DropOldest);
        let line = "n".repeat(1023) + "\n";

        for _ in 0..200 {
            let (bytes, lines) = entry(&line);
            shared.push(Source::Worker(0), bytes, lines);
        }

        let (bytes, lines) = entry("quiet\n");
        shared.push(Source::Worker(1), bytes, lines);

        let (first, _) = shared.take().unwrap();
        let first = String::from_utf8_lossy(&first);

        assert!(first.contains("quiet"), "the first write leaves it out");
        assert!(first.len() <= BATCH);
    }

    #[test]
    fn under_block_a_full_queue_has_no_room_until_the_writer_takes_from_it() {
        let shared = Shared::new(MIN_QUEUE, Overflow::Block);
        let line = "b".repeat(1023) + "\n";

        for _ in 0..64 {
            let (bytes, lines) = entry(&line);
            shared.push(Source::Worker(0), bytes, lines);
        }

        assert!(!shared.has_room(Source::Worker(0)));
        assert!(shared.has_room(Source::Worker(1)));
        assert_eq!(shared.lock().dropped, 0);

        let _ = shared.take().unwrap();
        assert!(shared.has_room(Source::Worker(0)));
    }

    #[test]
    fn a_stall_is_one_event_when_it_starts_and_one_when_it_ends() {
        let shared = Shared::new(MIN_QUEUE, Overflow::DropOldest);
        let mut events = shared.events.subscribe();
        let line = "s".repeat(1023) + "\n";

        for _ in 0..200 {
            let (bytes, lines) = entry(&line);
            shared.push(Source::Worker(0), bytes, lines);
        }

        loop {
            let waiting = {
                let state = shared.lock();
                state.queued_bytes > 0 || gaps_waiting(&state)
            };

            if !waiting {
                break;
            }

            let _ = shared.take();
            shared.wrote();
        }

        let started = events.try_recv().unwrap();
        let ended = events.try_recv().unwrap();

        assert_eq!(started.kind, "output.dropping");
        assert_eq!(started.level, "warning");
        assert_eq!(ended.kind, "output.caught_up");
        assert!(
            ended.message.contains("136 lines were dropped"),
            "{}",
            ended.message
        );
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn lines_a_failed_write_lost_are_counted() {
        let shared = Shared::new(DEFAULT_QUEUE, Overflow::DropOldest);
        shared.lost(5, "No space left on device");

        assert_eq!(shared.status.borrow().dropped, 5);
        assert_eq!(shared.status.borrow().stall.as_deref(), Some("failing"));
    }

    #[test]
    fn on_overflow_is_drop_oldest_or_block() {
        assert_eq!(Overflow::parse("block"), Ok(Overflow::Block));
        assert_eq!(Overflow::parse("drop_oldest"), Ok(Overflow::DropOldest));
        assert!(Overflow::parse("drop").is_err());
    }

    #[tokio::test]
    async fn a_relay_reads_the_pipes_a_host_offers_and_tells_it_their_end() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join(OUTPUT_SOCKET);
        let log = dir.path().join("release.log");
        let listener = UnixListener::bind(&output).unwrap();

        let args = RelayArgs {
            connect: output.clone(),
            control: dir.path().join(CONTROL_SOCKET),
            host_output_fd: None,
            log_file: Some(log.clone()),
            queue: DEFAULT_QUEUE,
            overflow: Overflow::DropOldest,
        };
        let relay = std::thread::spawn(move || run(args));

        // The host's side: offer one worker's stderr, write to it, close
        // it, and wait for the relay to say it has read it to its end.

        let (host, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = host.into_split();
        let (read, mut write) = std::io::pipe().unwrap();

        muru_proto::descriptors::write_with_fd(
            &mut write_half,
            b"{\"pipe\":7,\"worker\":2,\"stream\":\"stderr\"}\n",
            std::os::fd::AsFd::as_fd(&read),
        )
        .await
        .unwrap();
        drop(read);
        write.write_all(b"hello\n").unwrap();
        drop(write);

        let mut ended = BufReader::new(read_half).lines();
        let line = tokio::time::timeout(Duration::from_secs(10), ended.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(line, "{\"ended\":7}");

        // The host goes away, and the relay ends once it has written.

        drop(ended);
        drop(write_half);
        tokio::task::spawn_blocking(move || relay.join().unwrap().unwrap())
            .await
            .unwrap();

        let written = std::fs::read_to_string(&log).unwrap();
        assert!(written.ends_with(" muru.worker[2]: hello\n"), "{written}");
    }
}
