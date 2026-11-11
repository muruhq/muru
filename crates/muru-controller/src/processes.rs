//! The processes of an instance (design specification section 13.10):
//! what the daemon, a development run or `muructl serve` supervises, as
//! Muru's tree rather than the operating system's, with what the
//! operating system says of each process and the figures of the last
//! second. `muructl status --processes`, its `--watch`, the dashboard's
//! Processes section and the debug bundle all read it through the admin
//! API's ListProcesses, so they always agree.
//!
//! The pids and roles come from what the instance supervises: the edge,
//! each release's host and log relay, the workers as their hosts report
//! them, managed server instances and the commands it runs. CPU, memory,
//! the user and the parent the operating system records come from
//! `/proc` on Linux and `proc_pidinfo` on macOS. The live figures come
//! from the per-second sampling of the edge's counters and the workers'
//! reports.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use muru_edge::metrics::Sample;
use muru_proto::admin::v1 as pb;

use crate::host::WorkerStatus;

/// What the operating system says of a process.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OsProcess {
    /// Its pid.
    pub pid: u32,
    /// The parent the operating system records.
    pub parent: u32,
    /// The user it runs as, where it can be read.
    pub uid: Option<u32>,
    /// When it started, Unix milliseconds.
    pub started_unix_ms: Option<u64>,
    /// The CPU time it has used, user and system, in nanoseconds; none
    /// where the system does not say, as macOS does not for another
    /// user's process.
    pub cpu_ns: Option<u64>,
    /// Its resident set size in bytes, likewise.
    pub rss: Option<u64>,
    /// Its command line, or its name where the line cannot be read.
    pub name: String,
}

/// Read one process, or none when there is no such process.
#[cfg(target_os = "linux")]
pub fn read(pid: u32) -> Option<OsProcess> {
    use std::os::unix::fs::MetadataExt;

    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat[open + 1..close].to_owned();
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();

    // The first field after the name is the state, the third of all.

    let field = |number: usize| fields.get(number - 3)?.parse::<u64>().ok();
    let ticks = clock_ticks();
    let cpu_ns = field(14)
        .zip(field(15))
        .map(|(user, system)| (user + system) * 1_000_000_000 / ticks);
    let started_unix_ms = field(22)
        .zip(boot_unix_ms())
        .map(|(start, boot)| boot + start * 1000 / ticks);
    let rss = field(24).map(|pages| pages * page_size());
    let uid = std::fs::metadata(format!("/proc/{pid}"))
        .ok()
        .map(|meta| meta.uid());

    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let line = String::from_utf8_lossy(&cmdline).replace('\0', " ");
    let line = line.trim();

    Some(OsProcess {
        pid,
        parent: u32::try_from(field(4)?).ok()?,
        uid,
        started_unix_ms,
        cpu_ns,
        rss,
        name: if line.is_empty() {
            comm
        } else {
            line.to_owned()
        },
    })
}

/// Every process the operating system lists.
#[cfg(target_os = "linux")]
pub fn all() -> Vec<OsProcess> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(read)
        .collect()
}

#[cfg(target_os = "linux")]
fn clock_ticks() -> u64 {
    // SAFETY: sysconf has no preconditions.

    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks)
        .ok()
        .filter(|ticks| *ticks > 0)
        .unwrap_or(100)
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.

    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size)
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(4096)
}

/// When the system booted, Unix milliseconds, from `btime` in
/// `/proc/stat`, which process start times are counted from.
#[cfg(target_os = "linux")]
fn boot_unix_ms() -> Option<u64> {
    static BOOT: OnceLock<Option<u64>> = OnceLock::new();

    *BOOT.get_or_init(|| {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let line = stat.lines().find(|line| line.starts_with("btime "))?;
        line[6..]
            .trim()
            .parse::<u64>()
            .ok()
            .map(|seconds| seconds * 1000)
    })
}

/// Read one process, or none when there is no such process.
#[cfg(target_os = "macos")]
pub fn read(pid: u32) -> Option<OsProcess> {
    let bsd: libc::proc_bsdinfo = pid_info(pid, libc::PROC_PIDTBSDINFO)?;
    let task: Option<libc::proc_taskinfo> = pid_info(pid, libc::PROC_PIDTASKINFO);
    let (numer, denom) = timebase();

    let name = |chars: &[libc::c_char]| -> String {
        let bytes: Vec<u8> = chars
            .iter()
            .take_while(|char| **char != 0)
            .map(|char| *char as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let long = name(&bsd.pbi_name);

    Some(OsProcess {
        pid,
        parent: bsd.pbi_ppid,
        uid: Some(bsd.pbi_uid),
        started_unix_ms: Some(bsd.pbi_start_tvsec * 1000 + bsd.pbi_start_tvusec / 1000),
        cpu_ns: task.map(|task| {
            let ticks = u128::from(task.pti_total_user + task.pti_total_system);
            (ticks * u128::from(numer) / u128::from(denom.max(1))) as u64
        }),
        rss: task.map(|task| task.pti_resident_size),
        name: if long.is_empty() {
            name(&bsd.pbi_comm)
        } else {
            long
        },
    })
}

/// Every process the operating system lists.
#[cfg(target_os = "macos")]
pub fn all() -> Vec<OsProcess> {
    let mut pids = vec![0 as libc::pid_t; 8192];
    let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;

    // SAFETY: proc_listallpids writes at most `size` bytes of pids to the
    // buffer, which is that large and outlives the call.

    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
    let count = usize::try_from(count).unwrap_or(0).min(pids.len());

    pids[..count]
        .iter()
        .filter_map(|pid| u32::try_from(*pid).ok().filter(|pid| *pid > 0))
        .filter_map(read)
        .collect()
}

/// One flavour of a process's information from proc_pidinfo.
#[cfg(target_os = "macos")]
fn pid_info<T>(pid: u32, flavour: libc::c_int) -> Option<T> {
    let size = std::mem::size_of::<T>() as libc::c_int;

    // SAFETY: the structures asked for are plain data, for which all
    // zeroes is a valid value.

    let mut info: T = unsafe { std::mem::zeroed() };

    // SAFETY: proc_pidinfo writes at most `size` bytes to the buffer it
    // is given, which is a T of that size that outlives the call.

    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            flavour,
            0,
            std::ptr::from_mut(&mut info).cast(),
            size,
        )
    };

    (written == size).then_some(info)
}

/// The ratio that turns Mach absolute time, which proc_pidinfo counts CPU
/// time in, into nanoseconds.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn timebase() -> (u32, u32) {
    static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();

    *TIMEBASE.get_or_init(|| {
        let mut info = libc::mach_timebase_info { numer: 1, denom: 1 };

        // SAFETY: mach_timebase_info fills the structure it is given,
        // which outlives the call.

        let status = unsafe { libc::mach_timebase_info(&mut info) };

        if status == 0 {
            (info.numer, info.denom)
        } else {
            (1, 1)
        }
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn read(_pid: u32) -> Option<OsProcess> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn all() -> Vec<OsProcess> {
    Vec::new()
}

/// A user's name, by uid, or the uid as a number when it has none.
pub fn user_name(uid: u32) -> String {
    static NAMES: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    let names = NAMES.get_or_init(Mutex::default);

    if let Some(name) = names.lock().expect("user names poisoned").get(&uid) {
        return name.clone();
    }

    let name = look_up_user(uid).unwrap_or_else(|| uid.to_string());
    names
        .lock()
        .expect("user names poisoned")
        .insert(uid, name.clone());
    name
}

fn look_up_user(uid: u32) -> Option<String> {
    let mut buffer = vec![0 as libc::c_char; 4096];
    let mut found: *mut libc::passwd = std::ptr::null_mut();

    // SAFETY: passwd is plain data, for which all zeroes is a valid
    // value.

    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };

    // SAFETY: getpwuid_r fills the entry, with its strings in the buffer
    // of the size given, and sets `found` to the entry or to null; all
    // three outlive the call.

    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };

    if status != 0 || found.is_null() || entry.pw_name.is_null() {
        return None;
    }

    // SAFETY: getpwuid_r succeeded, so pw_name points at a NUL-terminated
    // string in the buffer, which is still alive.

    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

/// The host's name, as the operating system gives it.
pub fn hostname() -> String {
    let mut buffer = [0u8; 256];

    // SAFETY: gethostname writes at most the length given into the
    // buffer, which outlives the call.

    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };

    if status != 0 {
        return String::new();
    }

    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// Cumulative histogram counts by bound, each keyed by its bound's bits:
/// the bound in seconds and the count at or below it.
type Buckets = BTreeMap<u64, (f64, f64)>;

/// A worker by application, release, group and its place in the pool.
type WorkerKey = (String, String, String, String);

/// A worker group by application, release and name.
type GroupKey = (String, String, String);

/// A process's last reading: when, its CPU time then, and the
/// percentage last worked out for it.
type CpuReading = (Instant, u64, Option<f64>);

/// Each process's CPU time when it was last read, so the next reading
/// gives a percentage over the time between.
#[derive(Debug, Default)]
pub struct CpuMeter {
    /// By pid and start time: when the process was read, its CPU time
    /// then, and the percentage last worked out for it.
    seen: Mutex<HashMap<(u32, u64), CpuReading>>,
}

/// How long a reading stays the one the next is measured against.
const CPU_READING_LIFETIME: Duration = Duration::from_secs(60);

impl CpuMeter {
    /// A process's CPU use since it was last read, as a percentage of one
    /// CPU; none the first time, or where the system gives no CPU time.
    /// Read again too soon to measure, it gives the percentage it last
    /// worked out.
    pub fn percent(&self, os: &OsProcess, now: Instant) -> Option<f64> {
        let cpu_ns = os.cpu_ns?;
        let key = (os.pid, os.started_unix_ms.unwrap_or_default());
        let mut seen = self.seen.lock().expect("cpu meter poisoned");
        seen.retain(|_, (when, _, _)| now.duration_since(*when) < CPU_READING_LIFETIME);

        let Some((then, before, last)) = seen.get(&key).copied() else {
            seen.insert(key, (now, cpu_ns, None));
            return None;
        };

        let elapsed = now.duration_since(then).as_nanos() as f64;

        if elapsed < 50_000_000.0 {
            return last;
        }

        let percent = cpu_ns.saturating_sub(before) as f64 / elapsed * 100.0;
        seen.insert(key, (now, cpu_ns, Some(percent)));
        Some(percent)
    }

    /// Whether the process was read recently enough to be measured
    /// against.
    fn knows(&self, os: &OsProcess) -> bool {
        let key = (os.pid, os.started_unix_ms.unwrap_or_default());
        self.seen
            .lock()
            .expect("cpu meter poisoned")
            .contains_key(&key)
    }
}

/// A process the instance supervises, before the operating system is
/// asked about it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Known {
    /// Its pid.
    pub pid: u32,
    /// The pid of the process above it in Muru's tree; 0 for the root.
    pub parent: u32,
    /// The pid of the process that started it, when not the one above
    /// it: a log relay is shown beneath its host, but the daemon or the
    /// run starts it beside the host.
    pub spawned_by: u32,
    /// daemon, run, serve, edge, host, relay, worker, server or command.
    pub role: &'static str,
    /// What it is for, as a phrase.
    pub what: String,
    /// The application, for a process of one.
    pub app: String,
    /// The release, likewise.
    pub release: String,
    /// A worker's group.
    pub group: String,
    /// A worker's index in its group, or a server instance's index.
    pub index: Option<u32>,
    /// A worker's socket, by which the live figures name it.
    pub socket: Option<String>,
    /// For a host: its release's groups, when it has more than one.
    pub groups: Vec<String>,
}

/// The addresses listened on, as a phrase: `:80, :443` for every
/// interface, the address itself otherwise.
pub fn addresses(listeners: &[(std::net::SocketAddr, bool)]) -> String {
    listeners
        .iter()
        .map(|(address, _)| {
            if address.ip().is_unspecified() {
                format!(":{}", address.port())
            } else {
                address.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a worker is, as a phrase: its group, when `with_group`, and its
/// index, and what it is doing unless it is ready.
pub fn worker_phrase(group: &str, index: u32, state: &str, with_group: bool) -> String {
    let mut phrase = if with_group {
        format!("group {group}, worker {index}")
    } else {
        format!("worker {index}")
    };

    if !state.is_empty() && state != "ready" {
        phrase.push_str(&format!(", {state}"));
    }

    phrase
}

/// What the answer says of the instance itself.
#[derive(Debug, Clone, Default)]
pub struct Header {
    /// The version of Muru serving.
    pub version: String,
    /// daemon, run or serve.
    pub mode: String,
    /// When the instance started, Unix milliseconds.
    pub started_unix_ms: u64,
}

/// The answer to ListProcesses: the known processes as a tree, narrowed
/// as the request asks, with what the operating system says of each and
/// the live figures. Processes that have gone since they were listed are
/// left out. A process read for the first time is read again a moment
/// later, so even the first answer has CPU figures.
pub async fn snapshot(
    known: Vec<Known>,
    request: pb::ListProcessesRequest,
    header: Header,
    meter: Arc<CpuMeter>,
    live: &Live,
) -> pb::Processes {
    let figures = live.figures();

    let read = tokio::task::spawn_blocking(move || {
        let every: HashSet<u32> = known.iter().map(|process| process.pid).collect();
        let known = narrowed(known, &request);
        let kept: HashSet<u32> = known.iter().map(|process| process.pid).collect();
        let left_out: HashSet<u32> = every.difference(&kept).copied().collect();
        let pids: Vec<u32> = known.iter().map(|process| process.pid).collect();
        let mut os: HashMap<u32, OsProcess> = pids
            .iter()
            .filter_map(|pid| read(*pid).map(|found| (*pid, found)))
            .collect();

        if os
            .values()
            .any(|found| found.cpu_ns.is_some() && !meter.knows(found))
        {
            let now = Instant::now();

            for found in os.values() {
                meter.percent(found, now);
            }

            std::thread::sleep(Duration::from_millis(250));
            os = pids
                .iter()
                .filter_map(|pid| read(*pid).map(|found| (*pid, found)))
                .collect();
        }

        let table = if request.all { all() } else { Vec::new() };
        let now = Instant::now();
        let cpu: HashMap<u32, f64> = os
            .values()
            .filter_map(|found| {
                meter
                    .percent(found, now)
                    .map(|percent| (found.pid, percent))
            })
            .collect();

        let narrow = !request.app.is_empty();
        (known, os, table, cpu, left_out, narrow)
    })
    .await;

    let (known, os, table, cpu, left_out, narrow) = read.unwrap_or_default();
    let others = Others {
        table: &table,
        left_out: &left_out,
        narrow,
    };

    pb::Processes {
        version: header.version,
        hostname: hostname(),
        mode: header.mode,
        started_unix_ms: header.started_unix_ms,
        sampled_unix_ms: crate::model::now_unix_ms(),
        root: tree(&known, &os, &cpu, &others, &figures),
        figures: Some(figures.instance),
    }
}

/// The known processes the request keeps: with an application named, the
/// root and that application's processes, of the release named if one
/// is.
fn narrowed(known: Vec<Known>, request: &pb::ListProcessesRequest) -> Vec<Known> {
    if request.app.is_empty() {
        return known;
    }

    known
        .into_iter()
        .filter(|process| {
            process.parent == 0
                || (process.app == request.app
                    && (request.release.is_empty() || process.release == request.release))
        })
        .collect()
}

/// What `--all` adds to the tree: every process the operating system
/// lists, those the request narrowed out of the known ones, and whether
/// it was narrowed to one application, when what descends from the root
/// alone is not that application's.
#[derive(Debug)]
struct Others<'a> {
    table: &'a [OsProcess],
    left_out: &'a HashSet<u32>,
    narrow: bool,
}

/// The order a process's role sorts in beneath its parent.
fn rank(role: &str) -> u8 {
    match role {
        "edge" => 0,
        "relay" => 1,
        "host" => 2,
        "worker" => 3,
        "server" => 4,
        "command" => 5,
        _ => 6,
    }
}

/// Build the tree from the root: each known process beneath the one
/// above it in Muru's tree, those the operating system no longer has
/// left out, and with `table`, what `--all` asks for, every process
/// that descends from a known one beneath its operating system parent.
fn tree(
    known: &[Known],
    os: &HashMap<u32, OsProcess>,
    cpu: &HashMap<u32, f64>,
    others: &Others<'_>,
    figures: &Figures,
) -> Option<pb::Process> {
    let root = known.iter().find(|process| process.parent == 0)?;
    let known_pids: HashSet<u32> = known
        .iter()
        .filter(|process| process.parent == 0 || os.contains_key(&process.pid))
        .map(|process| process.pid)
        .collect();

    let mut nodes: HashMap<u32, (u32, pb::Process)> = HashMap::new();

    for process in known {
        if !known_pids.contains(&process.pid) {
            continue;
        }

        let found = os.get(&process.pid);
        let os_parent = found.map_or(0, |found| found.parent);
        let mut node = node(process.pid, found, cpu);
        node.os_parent_pid = os_parent;
        node.role = process.role.to_owned();
        node.what.clone_from(&process.what);
        node.app.clone_from(&process.app);
        node.release.clone_from(&process.release);
        node.group.clone_from(&process.group);
        node.index = process.index;

        let expected = if process.spawned_by == 0 {
            process.parent
        } else {
            process.spawned_by
        };
        node.readopted = process.parent != 0 && found.is_some() && os_parent != expected;

        // A managed server's instance says which program it runs.

        if process.role == "server"
            && let Some(program) = found
                .and_then(|found| found.name.split_whitespace().next())
                .and_then(|first| std::path::Path::new(first).file_name())
        {
            node.what = format!("{}: {}", process.what, program.to_string_lossy());
        }

        node.figures = match process.role {
            "worker" => {
                let mut own = process
                    .socket
                    .as_ref()
                    .and_then(|socket| figures.workers.get(socket).cloned())
                    .unwrap_or_default();

                if let Some((gil, lag)) = figures.signals.get(&process.pid) {
                    own.gil_wait_p95_ms = *gil;
                    own.loop_lag_p95_ms = *lag;
                }

                Some(own)
            }
            "edge" => Some(pb::Figures {
                requests_per_second: Some(figures.instance.requests_per_second),
                ..pb::Figures::default()
            }),
            "host" if process.groups.len() <= 1 => figures
                .groups
                .get(&(
                    process.app.clone(),
                    process.release.clone(),
                    process
                        .groups
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "default".to_owned()),
                ))
                .cloned(),
            _ => None,
        };

        if process.role == "host" && process.groups.len() > 1 {
            node.groups = process
                .groups
                .iter()
                .map(|group| pb::GroupFigures {
                    group: group.clone(),
                    figures: figures
                        .groups
                        .get(&(process.app.clone(), process.release.clone(), group.clone()))
                        .cloned(),
                })
                .collect();
        }

        nodes.insert(process.pid, (process.parent, node));
    }

    // Section 13.10: with --all, what descends from a known process and is
    // not one of them, beneath its operating system parent.

    let parents: HashMap<u32, u32> = others
        .table
        .iter()
        .map(|found| (found.pid, found.parent))
        .collect();
    let mut stops = known_pids.clone();
    stops.extend(others.left_out);

    for found in others.table {
        if nodes.contains_key(&found.pid) || others.left_out.contains(&found.pid) {
            continue;
        }

        // Beneath the nearest known process, unless that one was narrowed
        // out, or is the root of a tree narrowed to one application.

        let Some(ancestor) = nearest(found.pid, &parents, &stops) else {
            continue;
        };

        if others.left_out.contains(&ancestor) || (others.narrow && ancestor == root.pid) {
            continue;
        }

        let mut node = node(found.pid, Some(found), cpu);
        node.os_parent_pid = found.parent;
        node.role = "other".to_owned();
        node.what.clone_from(&found.name);
        nodes.insert(found.pid, (found.parent, node));
    }

    // Children before parents: each node is taken out and put beneath
    // its parent, deepest first.

    let depth = |pid: u32| {
        let mut depth = 0;
        let mut at = pid;

        while let Some((parent, _)) = nodes.get(&at) {
            if *parent == 0 || depth > 64 {
                break;
            }

            depth += 1;
            at = *parent;
        }

        depth
    };

    let mut order: Vec<(usize, u32)> = nodes.keys().map(|pid| (depth(*pid), *pid)).collect();
    order.sort_unstable_by(|a, b| b.cmp(a));

    for (_, pid) in order {
        if pid == root.pid {
            continue;
        }

        let Some((parent, node)) = nodes.remove(&pid) else {
            continue;
        };

        match nodes.get_mut(&parent) {
            Some((_, above)) => above.children.push(node),
            None => {
                if let Some((_, root_node)) = nodes.get_mut(&root.pid) {
                    root_node.children.push(node);
                }
            }
        }
    }

    let (_, mut root_node) = nodes.remove(&root.pid)?;
    sort(&mut root_node);
    Some(root_node)
}

/// The nearest of `stops` a process descends from, by its operating
/// system parents.
fn nearest(pid: u32, parents: &HashMap<u32, u32>, stops: &HashSet<u32>) -> Option<u32> {
    let mut at = pid;

    for _ in 0..64 {
        let parent = parents.get(&at).copied()?;

        if stops.contains(&parent) {
            return Some(parent);
        }

        if parent <= 1 {
            return None;
        }

        at = parent;
    }

    None
}

/// A node with what the operating system says of a process.
fn node(pid: u32, found: Option<&OsProcess>, cpu: &HashMap<u32, f64>) -> pb::Process {
    pb::Process {
        pid,
        user: found
            .and_then(|found| found.uid)
            .map(user_name)
            .unwrap_or_default(),
        started_unix_ms: found.and_then(|found| found.started_unix_ms),
        cpu_percent: cpu.get(&pid).map(|percent| (percent * 10.0).round() / 10.0),
        rss_bytes: found.and_then(|found| found.rss),
        ..pb::Process::default()
    }
}

/// Sort each node's children: by role, then application, release, group
/// and index, then pid.
fn sort(node: &mut pb::Process) {
    node.children.sort_by(|a, b| {
        (rank(&a.role), &a.app, &a.release, &a.group, a.index, a.pid).cmp(&(
            rank(&b.role),
            &b.app,
            &b.release,
            &b.group,
            b.index,
            b.pid,
        ))
    });

    for child in &mut node.children {
        sort(child);
    }
}

/// The live figures as of the last second (section 13.10).
#[derive(Debug, Clone, Default)]
pub struct Figures {
    /// The instance's.
    pub instance: pb::InstanceFigures,
    /// Each worker's, by its socket.
    pub workers: HashMap<String, pb::Figures>,
    /// Each worker group's, by application, release and group.
    pub groups: HashMap<(String, String, String), pb::Figures>,
    /// Each worker's GIL wait and loop lag p95, by pid.
    pub signals: HashMap<u32, (Option<f64>, Option<f64>)>,
}

/// One reading of the counters the figures are the differences of.
#[derive(Debug, Clone, Default)]
struct Totals {
    requests: f64,
    errors: f64,
    tls: f64,
    received: f64,
    sent: f64,
    /// The instance's total-time buckets, by bound.
    buckets: BTreeMap<u64, (f64, f64)>,
    /// Each worker's requests and total-time buckets, by application,
    /// release, group and its place in the pool.
    workers: HashMap<WorkerKey, (f64, Buckets)>,
}

/// The per-second sampling of the live figures: each reading of the
/// edge's counters and the workers' reports is turned into rates and
/// percentiles from its difference with the one before.
#[derive(Debug, Default)]
pub struct Live {
    inner: Mutex<LiveState>,
}

#[derive(Debug, Default)]
struct LiveState {
    previous: Option<(Instant, Totals)>,
    /// Each worker's last GIL wait and loop lag counts, by pid.
    signals: HashMap<u32, (Vec<u64>, Vec<u64>)>,
    /// Each worker's last GIL wait and loop lag p95 over a second in
    /// which there were any, by pid.
    signal_p95: HashMap<u32, (Option<f64>, Option<f64>)>,
    figures: Figures,
}

impl Live {
    /// The figures of the last second.
    pub fn figures(&self) -> Figures {
        self.inner
            .lock()
            .expect("live figures poisoned")
            .figures
            .clone()
    }

    /// Take one second's reading: the edge's samples, and what every
    /// worker of each release last reported, by application and release.
    pub fn observe(&self, samples: &[Sample], reports: &[(String, String, Vec<WorkerStatus>)]) {
        let now = Instant::now();
        let mut state = self.inner.lock().expect("live figures poisoned");
        let totals = totals(samples);
        let previous = state.previous.replace((now, totals.clone()));

        let mut figures = Figures::default();
        gauges(samples, &mut figures);

        if let Some((then, before)) = previous {
            let elapsed = now.duration_since(then).as_secs_f64().max(0.001);
            rates(&totals, &before, elapsed, samples, &mut figures);
        }

        signals(&mut state, reports, &mut figures);
        state.figures = figures;
    }
}

/// The value of a sample's label, or empty.
fn label<'a>(sample: &'a Sample, name: &str) -> &'a str {
    sample
        .labels
        .iter()
        .find(|(key, _)| key == name)
        .map_or("", |(_, value)| value.as_str())
}

/// A bucket's bound from its `le` label.
fn bound(sample: &Sample) -> Option<f64> {
    match label(sample, "le") {
        "" => None,
        "+Inf" => Some(f64::INFINITY),
        text => text.parse().ok(),
    }
}

fn totals(samples: &[Sample]) -> Totals {
    let mut totals = Totals::default();

    for sample in samples {
        let value = sample.value;

        match sample.name.as_str() {
            "muru_requests_total" => {
                totals.requests += value;

                if label(sample, "status") == "5xx" {
                    totals.errors += value;
                }
            }
            "muru_tls_handshakes_total" => totals.tls += value,
            "muru_request_bytes_total" => totals.received += value,
            "muru_response_bytes_total" => totals.sent += value,
            "muru_request_duration_seconds_bucket" if label(sample, "phase") == "total" => {
                if let Some(le) = bound(sample) {
                    totals.buckets.entry(le.to_bits()).or_insert((le, 0.0)).1 += value;
                }
            }
            name if name == muru_edge::metrics::LIVE_WORKER_REQUESTS => {
                totals.workers.entry(worker_key(sample)).or_default().0 += value;
            }
            name if name.strip_suffix("_bucket")
                == Some(muru_edge::metrics::LIVE_WORKER_DURATION) =>
            {
                if let Some(le) = bound(sample) {
                    totals
                        .workers
                        .entry(worker_key(sample))
                        .or_default()
                        .1
                        .entry(le.to_bits())
                        .or_insert((le, 0.0))
                        .1 += value;
                }
            }
            _ => {}
        }
    }

    totals
}

fn worker_key(sample: &Sample) -> (String, String, String, String) {
    (
        label(sample, "app").to_owned(),
        label(sample, "release").to_owned(),
        label(sample, "group").to_owned(),
        label(sample, "worker").to_owned(),
    )
}

/// What the samples say of now: the instance's gauges, and each worker's
/// busy slots of its slots and each group's queue.
fn gauges(samples: &[Sample], figures: &mut Figures) {
    let instance = &mut figures.instance;

    for sample in samples {
        let value = sample.value.max(0.0);

        match sample.name.as_str() {
            "muru_requests_in_flight" => instance.in_flight += value as u64,
            "muru_websocket_connections" => instance.websockets += value as u64,
            "muru_connections_open" => instance.connections += value as u64,
            "muru_queue_depth" => {
                let group = figures.groups.entry(group_key(sample)).or_default();
                group.queue = Some(group.queue.unwrap_or(0) + value as u32);
            }
            name if name == muru_edge::metrics::LIVE_WORKER_BUSY
                || name == muru_edge::metrics::LIVE_WORKER_SLOTS =>
            {
                let busy = name == muru_edge::metrics::LIVE_WORKER_BUSY;
                let worker = figures
                    .workers
                    .entry(label(sample, "socket").to_owned())
                    .or_default();
                let group = figures.groups.entry(group_key(sample)).or_default();

                for figures in [worker, group] {
                    let slot = if busy {
                        &mut figures.busy
                    } else {
                        &mut figures.slots
                    };
                    *slot = Some(slot.unwrap_or(0) + value as u32);
                }
            }
            _ => {}
        }
    }
}

fn group_key(sample: &Sample) -> (String, String, String) {
    (
        label(sample, "app").to_owned(),
        label(sample, "release").to_owned(),
        label(sample, "group").to_owned(),
    )
}

/// The rates and percentiles of the last second, from two readings.
fn rates(
    totals: &Totals,
    before: &Totals,
    elapsed: f64,
    samples: &[Sample],
    figures: &mut Figures,
) {
    let per_second =
        |now: f64, then: f64| ((now - then).max(0.0) / elapsed * 100.0).round() / 100.0;
    let instance = &mut figures.instance;
    instance.requests_per_second = per_second(totals.requests, before.requests);
    instance.errors_per_second = per_second(totals.errors, before.errors);
    instance.tls_handshakes_per_second = per_second(totals.tls, before.tls);
    instance.received_bytes_per_second = per_second(totals.received, before.received);
    instance.sent_bytes_per_second = per_second(totals.sent, before.sent);

    let deltas = differences(&totals.buckets, &before.buckets);
    instance.p50_ms = milliseconds(&deltas, 0.5).unwrap_or(0.0);
    instance.p95_ms = milliseconds(&deltas, 0.95).unwrap_or(0.0);

    // Each worker is named by its socket, which its place in the pool
    // maps to.

    let sockets: HashMap<(String, String, String, String), String> = samples
        .iter()
        .filter(|sample| sample.name == muru_edge::metrics::LIVE_WORKER_SLOTS)
        .map(|sample| (worker_key(sample), label(sample, "socket").to_owned()))
        .collect();

    let mut groups: HashMap<GroupKey, (f64, Buckets)> = HashMap::new();

    for (key, (requests, buckets)) in &totals.workers {
        let (requests_before, buckets_before) =
            before.workers.get(key).cloned().unwrap_or_default();
        let deltas = differences(buckets, &buckets_before);
        let group = groups
            .entry((key.0.clone(), key.1.clone(), key.2.clone()))
            .or_default();
        group.0 += (requests - requests_before).max(0.0);

        for (bits, (le, count)) in &deltas {
            group.1.entry(*bits).or_insert((*le, 0.0)).1 += count;
        }

        if let Some(socket) = sockets.get(key) {
            let worker = figures.workers.entry(socket.clone()).or_default();
            worker.requests_per_second = Some(per_second(*requests, requests_before));
            worker.p95_ms = milliseconds(&deltas, 0.95);
        }
    }

    for (key, (requests, deltas)) in groups {
        let group = figures.groups.entry(key).or_default();
        group.requests_per_second = Some((requests / elapsed * 100.0).round() / 100.0);
        group.p95_ms = milliseconds(&deltas, 0.95);
    }
}

/// The counts a later reading of cumulative buckets adds to an earlier.
fn differences(
    now: &BTreeMap<u64, (f64, f64)>,
    then: &BTreeMap<u64, (f64, f64)>,
) -> BTreeMap<u64, (f64, f64)> {
    now.iter()
        .map(|(bits, (le, count))| {
            let before = then.get(bits).map_or(0.0, |(_, count)| *count);
            (*bits, (*le, (count - before).max(0.0)))
        })
        .collect()
}

/// A quantile of bucket counts in milliseconds, or none with nothing
/// observed.
fn milliseconds(buckets: &BTreeMap<u64, (f64, f64)>, q: f64) -> Option<f64> {
    let ordered: Vec<(f64, f64)> = {
        let mut ordered: Vec<(f64, f64)> = buckets.values().copied().collect();
        ordered.sort_by(|a, b| a.0.total_cmp(&b.0));
        ordered
    };

    let observed = ordered.last().map_or(0.0, |(_, count)| *count);

    if observed <= 0.0 {
        return None;
    }

    Some((muru_dashboard::history::quantile(&ordered, q) * 1000.0 * 10.0).round() / 10.0)
}

/// The GIL wait and loop lag p95 of each worker over the reports since
/// the last, from the differences of its cumulative counts, and of each
/// group from its workers' together. A worker with nothing observed since
/// keeps the last figure it had.
fn signals(
    state: &mut LiveState,
    reports: &[(String, String, Vec<WorkerStatus>)],
    figures: &mut Figures,
) {
    let bounds = muru_proto::worker::v1::SIGNAL_BOUNDS_US;
    let as_buckets = |counts: &[u64]| -> BTreeMap<u64, (f64, f64)> {
        counts
            .iter()
            .enumerate()
            .map(|(index, count)| {
                let le = bounds
                    .get(index)
                    .map_or(f64::INFINITY, |bound| *bound as f64 / 1_000_000.0);
                (le.to_bits(), (le, *count as f64))
            })
            .collect()
    };
    let difference = |now: &[u64], then: Option<&Vec<u64>>| -> Vec<u64> {
        now.iter()
            .enumerate()
            .map(|(index, count)| {
                count.saturating_sub(then.and_then(|then| then.get(index)).copied().unwrap_or(0))
            })
            .collect()
    };

    let mut alive = HashSet::new();
    let mut groups: HashMap<GroupKey, (Vec<u64>, Vec<u64>)> = HashMap::new();

    for (app, release, workers) in reports {
        for worker in workers {
            alive.insert(worker.pid);
            let previous = state.signals.get(&worker.pid).cloned();
            let gil = difference(&worker.gil_wait, previous.as_ref().map(|(gil, _)| gil));
            let lag = difference(&worker.loop_lag, previous.as_ref().map(|(_, lag)| lag));
            state.signals.insert(
                worker.pid,
                (worker.gil_wait.clone(), worker.loop_lag.clone()),
            );

            let kept = state.signal_p95.entry(worker.pid).or_default();
            let gil_p95 = (worker.gil_enabled && previous.is_some())
                .then(|| milliseconds(&as_buckets(&gil), 0.95))
                .flatten();
            let lag_p95 = previous
                .is_some()
                .then(|| milliseconds(&as_buckets(&lag), 0.95))
                .flatten();

            if gil_p95.is_some() {
                kept.0 = gil_p95;
            }

            if lag_p95.is_some() {
                kept.1 = lag_p95;
            }

            let group = groups
                .entry((app.clone(), release.clone(), worker.group.clone()))
                .or_default();

            for (sum, part) in [(&mut group.0, &gil), (&mut group.1, &lag)] {
                if sum.len() < part.len() {
                    sum.resize(part.len(), 0);
                }

                for (into, count) in sum.iter_mut().zip(part) {
                    *into += count;
                }
            }

            figures.signals.insert(worker.pid, *kept);
        }
    }

    state.signals.retain(|pid, _| alive.contains(pid));
    state.signal_p95.retain(|pid, _| alive.contains(pid));

    for (key, (gil, lag)) in groups {
        let group = figures.groups.entry(key).or_default();
        group.gil_wait_p95_ms = milliseconds(&as_buckets(&gil), 0.95);
        group.loop_lag_p95_ms = milliseconds(&as_buckets(&lag), 0.95);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, labels: &[(&str, &str)], value: f64) -> Sample {
        Sample::new(name, labels, value)
    }

    #[test]
    fn this_process_is_read_with_its_parent_and_user() {
        let me = read(std::process::id()).expect("this process");
        assert_eq!(me.pid, std::process::id());

        // SAFETY: getppid and getuid have no preconditions.

        let (parent, uid) = unsafe { (libc::getppid(), libc::getuid()) };
        assert_eq!(me.parent, parent as u32);
        assert_eq!(me.uid, Some(uid));
        assert!(me.rss.is_some_and(|rss| rss > 0));
        assert!(me.started_unix_ms.is_some_and(|ms| ms > 1_600_000_000_000));
        assert!(all().iter().any(|found| found.pid == std::process::id()));
    }

    #[test]
    fn the_tree_is_muru_s_and_says_where_the_operating_system_differs() {
        let known = vec![
            Known {
                pid: 10,
                role: "daemon",
                what: "muructl daemon run".to_owned(),
                ..Known::default()
            },
            Known {
                pid: 20,
                parent: 10,
                role: "host",
                app: "shop".to_owned(),
                release: "r1".to_owned(),
                ..Known::default()
            },
            Known {
                pid: 21,
                parent: 20,
                role: "worker",
                app: "shop".to_owned(),
                release: "r1".to_owned(),
                index: Some(1),
                socket: Some("/run/w1.sock".to_owned()),
                ..Known::default()
            },
            Known {
                pid: 22,
                parent: 20,
                role: "worker",
                app: "shop".to_owned(),
                release: "r1".to_owned(),
                index: Some(0),
                ..Known::default()
            },
            Known {
                pid: 30,
                parent: 10,
                role: "edge",
                ..Known::default()
            },
            Known {
                pid: 40,
                parent: 10,
                role: "host",
                app: "gone".to_owned(),
                ..Known::default()
            },
        ];
        let os = |pid: u32, parent: u32| OsProcess {
            pid,
            parent,
            ..OsProcess::default()
        };
        let found: HashMap<u32, OsProcess> =
            [os(10, 1), os(20, 1), os(21, 20), os(22, 20), os(30, 10)]
                .into_iter()
                .map(|found| (found.pid, found))
                .collect();
        let mut figures = Figures::default();
        figures.workers.insert(
            "/run/w1.sock".to_owned(),
            pb::Figures {
                requests_per_second: Some(5.0),
                ..pb::Figures::default()
            },
        );

        // The host adopted again is noted, the host that has gone left
        // out, and the edge sorts first, the workers by index.

        let none = HashSet::new();
        let nothing = Others {
            table: &[],
            left_out: &none,
            narrow: false,
        };
        let root = tree(&known, &found, &HashMap::new(), &nothing, &figures).expect("a root");
        assert_eq!(root.pid, 10);
        let roles: Vec<&str> = root
            .children
            .iter()
            .map(|child| child.role.as_str())
            .collect();
        assert_eq!(roles, ["edge", "host"]);
        let host = &root.children[1];
        assert!(host.readopted);
        assert_eq!(host.os_parent_pid, 1);
        assert!(!root.children[0].readopted);
        let workers: Vec<u32> = host.children.iter().map(|child| child.pid).collect();
        assert_eq!(workers, [22, 21]);
        assert_eq!(
            host.children[1]
                .figures
                .as_ref()
                .and_then(|f| f.requests_per_second),
            Some(5.0)
        );

        // With --all, a worker's own subprocess is beneath it.

        let table = vec![
            os(10, 1),
            os(20, 1),
            os(21, 20),
            os(22, 20),
            os(30, 10),
            os(50, 21),
            os(51, 50),
            os(60, 1),
        ];
        let others = Others {
            table: &table,
            left_out: &none,
            narrow: false,
        };
        let root = tree(&known, &found, &HashMap::new(), &others, &figures).expect("a root");
        let worker = &root.children[1].children[1];
        assert_eq!(worker.children.len(), 1);
        assert_eq!(worker.children[0].role, "other");
        assert_eq!(worker.children[0].children[0].pid, 51);
        assert!(root.children.iter().all(|child| child.pid != 60));

        // Narrowed to an application, the root stays.

        let request = pb::ListProcessesRequest {
            app: "shop".to_owned(),
            ..Default::default()
        };
        let kept: Vec<u32> = narrowed(known, &request)
            .iter()
            .map(|process| process.pid)
            .collect();
        assert_eq!(kept, [10, 20, 21, 22]);
    }

    #[test]
    fn rates_come_from_the_difference_of_two_readings() {
        let live = Live::default();
        let worker = [
            ("app", "shop"),
            ("release", "r1"),
            ("group", "default"),
            ("worker", "0"),
        ];
        let slots = [
            ("app", "shop"),
            ("release", "r1"),
            ("group", "default"),
            ("worker", "0"),
            ("socket", "/run/w0.sock"),
        ];
        let reading = |requests: f64| {
            vec![
                sample("muru_requests_total", &[("status", "2xx")], requests),
                sample(muru_edge::metrics::LIVE_WORKER_REQUESTS, &worker, requests),
                sample(muru_edge::metrics::LIVE_WORKER_BUSY, &slots, 2.0),
                sample(muru_edge::metrics::LIVE_WORKER_SLOTS, &slots, 4.0),
                sample(
                    "muru_queue_depth",
                    &[("app", "shop"), ("release", "r1"), ("group", "default")],
                    3.0,
                ),
            ]
        };

        live.observe(&reading(10.0), &[]);
        assert_eq!(live.figures().instance.requests_per_second, 0.0);
        std::thread::sleep(Duration::from_millis(100));
        live.observe(&reading(20.0), &[]);

        let figures = live.figures();
        assert!(figures.instance.requests_per_second > 0.0);
        let worker = &figures.workers["/run/w0.sock"];
        assert!(worker.requests_per_second.is_some_and(|rate| rate > 0.0));
        assert_eq!((worker.busy, worker.slots), (Some(2), Some(4)));
        let group = &figures.groups[&("shop".to_owned(), "r1".to_owned(), "default".to_owned())];
        assert_eq!(group.queue, Some(3));
        assert_eq!((group.busy, group.slots), (Some(2), Some(4)));
    }

    #[test]
    fn cpu_is_measured_between_readings() {
        let meter = CpuMeter::default();
        let now = Instant::now();
        let mut found = OsProcess {
            pid: 7,
            cpu_ns: Some(1_000_000_000),
            ..OsProcess::default()
        };
        assert_eq!(meter.percent(&found, now), None);
        found.cpu_ns = Some(1_500_000_000);
        let percent = meter.percent(&found, now + Duration::from_secs(1));
        assert_eq!(percent.map(|percent| percent.round()), Some(50.0));

        // Asked again at once, the last figure stands.

        let again = meter.percent(&found, now + Duration::from_millis(1010));
        assert_eq!(again.map(|percent| percent.round()), Some(50.0));
    }
}
