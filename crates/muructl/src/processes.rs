//! `muructl status --processes` (design specification section 13.10):
//! the instance's processes as Muru's supervision tree, with what each is
//! for, what the operating system says of it and the last second's
//! figures, once, or redrawn with `--watch` until interrupted.

use std::io::{IsTerminal as _, Write as _};
use std::path::Path;
use std::time::{Duration, SystemTime};

use clap::Args;
use muru_proto::admin::v1 as pb;

/// What `muructl status` takes for the process view.
#[derive(Args, Clone, Default)]
pub struct ProcessArgs {
    /// Show the instance's processes as a tree: what each is for, its
    /// user, uptime, CPU and memory, and each worker's figures over the
    /// last second.
    #[arg(long)]
    pub processes: bool,

    /// With --processes, only one application's processes, or one
    /// release's as APP@RELEASE.
    #[arg(long, value_name = "APP[@RELEASE]", requires = "processes")]
    pub app: Option<String>,

    /// With --processes, also the processes Muru did not start, such as
    /// those a worker ran, beneath the process they descend from.
    #[arg(long, requires = "processes")]
    pub all: bool,

    /// With --processes, show the view again every second, or at the
    /// interval given, such as 5s, until interrupted: redrawn in place on
    /// a terminal, one frame after another otherwise, and one JSON
    /// document a line with --json.
    #[arg(
        long,
        value_name = "INTERVAL",
        num_args = 0..=1,
        default_missing_value = "1s",
        value_parser = muru_config::parse_duration,
        requires = "processes"
    )]
    pub watch: Option<Duration>,
}

/// The request the arguments make.
pub fn request(args: &ProcessArgs) -> pb::ListProcessesRequest {
    let (app, release) = match args.app.as_deref().map(|app| app.split_once('@')) {
        Some(Some((app, release))) => (app.to_owned(), release.to_owned()),
        Some(None) => (args.app.clone().unwrap_or_default(), String::new()),
        None => (String::new(), String::new()),
    };

    pb::ListProcessesRequest {
        app,
        release,
        all: args.all,
    }
}

/// Show the processes, once or with `--watch` until interrupted.
pub async fn show(socket: &Path, args: &ProcessArgs, json: bool) -> anyhow::Result<()> {
    let request = request(args);
    let mut client = muru_controller::client::connect(socket).await?;

    let Some(every) = args.watch else {
        let answer = client
            .list_processes(request)
            .await
            .map_err(|status| anyhow::anyhow!("{}", status.message()))?
            .into_inner();

        if json {
            return crate::admin::print_answer(&answer);
        }

        print!("{}", render(&answer, None));
        return Ok(());
    };

    let redraw = std::io::stdout().is_terminal() && !json;
    let mut ticks = tokio::time::interval(every.max(Duration::from_millis(100)));
    let mut first = true;

    loop {
        ticks.tick().await;
        let answer = client
            .list_processes(request.clone())
            .await
            .map_err(|status| anyhow::anyhow!("{}", status.message()))?
            .into_inner();

        let frame = if json {
            format!("{}\n", serde_json::to_string(&answer)?)
        } else if redraw {
            // Home and clear, then the frame: drawn in place.

            format!("\x1b[H\x1b[2J{}", render(&answer, Some(every)))
        } else {
            let gap = if first { "" } else { "\n" };
            format!("{gap}{}", render(&answer, Some(every)))
        };

        // A reader that has gone ends the watch, as it ends `head`.

        let mut out = std::io::stdout().lock();
        out.write_all(frame.as_bytes())
            .and_then(|()| out.flush())
            .inspect_err(muru_common::log::reader_gone)?;
        first = false;
    }
}

/// One row of the table.
struct Row {
    pid: String,
    role: String,
    what: String,
    user: String,
    up: String,
    cpu: String,
    rss: String,
    figures: Option<pb::Figures>,
}

/// The view as text: with `every`, a line naming the instance, the time
/// and the interval first; then the instance's figures, and the table.
pub fn render(answer: &pb::Processes, every: Option<Duration>) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);

    render_at(answer, every, now)
}

/// The view as `render` draws it, with `now`, in Unix milliseconds, as
/// the time the uptimes are counted to.
fn render_at(answer: &pb::Processes, every: Option<Duration>, now: u64) -> String {
    let mut text = String::new();

    if let Some(every) = every {
        let up = uptime(now.saturating_sub(answer.started_unix_ms));
        let sampled = SystemTime::UNIX_EPOCH + Duration::from_millis(answer.sampled_unix_ms);
        let clock = muru_common::log::clock(sampled);
        let clock = clock.split('.').next().unwrap_or_default();
        text.push_str(&format!(
            "muru {} on {}   {} up {up}   {clock}   every {}\n",
            answer.version,
            answer.hostname,
            answer.mode,
            muru_config::format_duration(every)
        ));
    }

    if let Some(figures) = &answer.figures {
        text.push_str(&format!(
            "edge  {} req/s  {} err/s  p50 {} ms  p95 {} ms  in flight {}  ws {}  conns {}  tls {}/s  in {}/s  out {}/s\n\n",
            rate(figures.requests_per_second),
            rate(figures.errors_per_second),
            milliseconds(figures.p50_ms),
            milliseconds(figures.p95_ms),
            thousands(figures.in_flight),
            thousands(figures.websockets),
            thousands(figures.connections),
            rate(figures.tls_handshakes_per_second),
            crate::admin::human_size(figures.received_bytes_per_second as u64),
            crate::admin::human_size(figures.sent_bytes_per_second as u64),
        ));
    }

    let mut rows = Vec::new();

    if let Some(root) = &answer.root {
        flatten(root, "", "", true, now, &mut rows);
    }

    // The user column is the daemon's: a development run's processes are
    // all its own user's.

    let with_user = answer.mode == "daemon";
    let with_figures = rows.iter().any(|row| row.figures.is_some());
    let mut header = vec!["PID", "ROLE", "WHAT"];

    if with_user {
        header.push("USER");
    }

    header.extend(["UP", "CPU", "RSS"]);

    if with_figures {
        header.extend(["REQ/S", "BUSY", "QUEUE", "P95", "GIL/LAG"]);
    }

    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let mut cells = vec![row.pid.clone(), row.role.clone(), row.what.clone()];

            if with_user {
                cells.push(row.user.clone());
            }

            cells.extend([row.up.clone(), row.cpu.clone(), row.rss.clone()]);

            if with_figures {
                cells.extend(figure_cells(row.figures.as_ref()));
            }

            cells
        })
        .collect();

    let header: Vec<String> = header.into_iter().map(str::to_owned).collect();
    let widths: Vec<usize> = (0..header.len())
        .map(|column| {
            std::iter::once(&header)
                .chain(&table)
                .map(|cells| cells[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();

    // Text columns are aligned left, figures right.

    let numeric = |name: &str| !matches!(name, "PID" | "ROLE" | "WHAT" | "USER");

    for cells in std::iter::once(&header).chain(&table) {
        let line: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                let pad = widths[column].saturating_sub(cell.chars().count());

                if numeric(&header[column]) && column > 0 {
                    format!("{}{cell}", " ".repeat(pad))
                } else {
                    format!("{cell}{}", " ".repeat(pad))
                }
            })
            .collect();
        text.push_str(line.join("   ").trim_end());
        text.push('\n');
    }

    text
}

/// The rows of a process and those beneath it, with the tree's lines: a
/// host's groups, when it has several, as rows of their own before its
/// workers.
fn flatten(
    process: &pb::Process,
    lead: &str,
    branch: &str,
    root: bool,
    now: u64,
    rows: &mut Vec<Row>,
) {
    let mut what = process.what.clone();

    if process.readopted {
        what.push_str(&format!(
            " (re-adopted, OS parent {})",
            process.os_parent_pid
        ));
    }

    rows.push(Row {
        pid: process.pid.to_string(),
        role: format!("{lead}{branch}{}", process.role),
        what,
        user: process.user.clone(),
        up: process
            .started_unix_ms
            .map(|started| uptime(now.saturating_sub(started)))
            .unwrap_or_default(),
        cpu: process
            .cpu_percent
            .map(|percent| format!("{percent:.1}%"))
            .unwrap_or_default(),
        rss: process
            .rss_bytes
            .map(crate::admin::human_size)
            .unwrap_or_default(),
        figures: process.figures,
    });

    let below = if root {
        String::new()
    } else if branch.starts_with('└') {
        format!("{lead}   ")
    } else {
        format!("{lead}│  ")
    };

    let count = process.children.len();
    let mut shown_group = String::new();

    for (at, child) in process.children.iter().enumerate() {
        let last = at + 1 == count;

        // A group's row before its first worker.

        if child.role == "worker"
            && child.group != shown_group
            && let Some(group) = process
                .groups
                .iter()
                .find(|group| group.group == child.group)
        {
            rows.push(Row {
                pid: String::new(),
                role: format!("{below}├─ group"),
                what: group.group.clone(),
                user: String::new(),
                up: String::new(),
                cpu: String::new(),
                rss: String::new(),
                figures: group.figures,
            });
            shown_group.clone_from(&child.group);
        }

        let branch = if last { "└─ " } else { "├─ " };
        flatten(child, &below, branch, false, now, rows);
    }
}

/// The figure columns: requests per second, busy slots of the slots,
/// requests waiting, p95 total time, and GIL wait or loop lag p95.
fn figure_cells(figures: Option<&pb::Figures>) -> Vec<String> {
    let Some(figures) = figures else {
        return vec![String::new(); 5];
    };

    let busy = match (figures.busy, figures.slots) {
        (Some(busy), Some(slots)) => format!("{busy}/{slots}"),
        _ => String::new(),
    };

    let wait = figures
        .gil_wait_p95_ms
        .or(figures.loop_lag_p95_ms)
        .map(|ms| format!("{} ms", milliseconds(ms)))
        .unwrap_or_default();

    vec![
        figures.requests_per_second.map(rate).unwrap_or_default(),
        busy,
        figures
            .queue
            .map(|queue| queue.to_string())
            .unwrap_or_default(),
        figures
            .p95_ms
            .map(|ms| format!("{} ms", milliseconds(ms)))
            .unwrap_or_default(),
        wait,
    ]
}

/// A rate: whole with thousands separated from 10 up, one decimal below.
fn rate(value: f64) -> String {
    if value >= 10.0 {
        thousands(value.round() as u64)
    } else if value == 0.0 {
        "0".to_owned()
    } else {
        format!("{value:.1}")
    }
}

/// Milliseconds, whole from 10 up and to one decimal below.
fn milliseconds(value: f64) -> String {
    if value >= 10.0 {
        thousands(value.round() as u64)
    } else {
        format!("{value:.1}")
    }
}

/// A count with its thousands separated by commas.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();

    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }

        out.push(digit);
    }

    out
}

/// How long a process has been up, in its two largest units: 3d2h,
/// 5h12m, 12m, 45s.
fn uptime(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3600 % 24, seconds / 60 % 60);

    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, role: &str, what: &str, children: Vec<pb::Process>) -> pb::Process {
        pb::Process {
            pid,
            role: role.to_owned(),
            what: what.to_owned(),
            children,
            ..pb::Process::default()
        }
    }

    #[test]
    fn the_tree_is_drawn_with_its_lines_and_what_differs_from_the_os() {
        let mut host = process(
            4301,
            "host",
            "shop@r42",
            vec![
                process(4302, "relay", "shop@r42 output to shop/r42.log", vec![]),
                process(4310, "worker", "group default, worker 0", vec![]),
            ],
        );
        host.readopted = true;
        host.os_parent_pid = 1;
        host.children[1].figures = Some(pb::Figures {
            requests_per_second: Some(512.4),
            busy: Some(14),
            slots: Some(16),
            p95_ms: Some(40.2),
            gil_wait_p95_ms: Some(1.25),
            ..pb::Figures::default()
        });
        let answer = pb::Processes {
            mode: "daemon".to_owned(),
            root: Some(process(
                4242,
                "daemon",
                "muructl daemon run",
                vec![process(4250, "edge", "listening on :80", vec![]), host],
            )),
            ..pb::Processes::default()
        };

        let text = render(&answer, None);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("PID"), "{text}");
        assert!(
            lines[0].contains("USER") && lines[0].contains("GIL/LAG"),
            "{text}"
        );
        assert!(lines[1].starts_with("4242   daemon"), "{text}");
        assert!(lines[2].contains("├─ edge"), "{text}");
        assert!(
            lines[3].contains("└─ host") && lines[3].contains("(re-adopted, OS parent 1)"),
            "{text}"
        );
        assert!(lines[4].contains("   ├─ relay"), "{text}");
        assert!(lines[5].contains("   └─ worker"), "{text}");
        assert!(
            lines[5].contains("512") && lines[5].contains("14/16") && lines[5].contains("40 ms"),
            "{text}"
        );
        assert!(
            lines[5].contains("1.2 ms") || lines[5].contains("1.3 ms"),
            "{text}"
        );
    }

    // The guides' examples of `muructl status --processes` are what the
    // command prints for an instance built here as the daemon and a run
    // build their answers, so they cannot drift from it (issue 601).

    use std::collections::HashMap;

    use muru_controller::processes::{Figures, Known, OsProcess, tree_from, worker_phrase};

    /// When the examples are taken, Unix milliseconds.
    const NOW: u64 = 1_800_000_000_000;

    /// An instance for an example: what is known of each process, what
    /// the operating system says of it, and the live figures.
    #[derive(Default)]
    struct Example {
        known: Vec<Known>,
        os: HashMap<u32, OsProcess>,
        cpu: HashMap<u32, f64>,
        users: HashMap<u32, &'static str>,
        figures: Figures,
    }

    impl Example {
        /// A process, with its operating system parent, its user, how long
        /// it has been up, its CPU percentage and its resident memory.
        fn add(
            &mut self,
            known: Known,
            os_parent: u32,
            user: &'static str,
            up_s: u64,
            cpu: f64,
            rss_mb: f64,
        ) {
            let pid = known.pid;
            self.os.insert(
                pid,
                OsProcess {
                    pid,
                    parent: os_parent,
                    started_unix_ms: Some(NOW - up_s * 1000),
                    rss: Some((rss_mb * 1024.0 * 1024.0) as u64),
                    name: if known.role == "server" {
                        "/srv/blog/.venv/bin/gunicorn --workers 2 blog:app".to_owned()
                    } else {
                        String::new()
                    },
                    ..OsProcess::default()
                },
            );
            self.cpu.insert(pid, cpu);
            self.users.insert(pid, user);
            self.known.push(known);
        }

        /// A worker's figures over the last second, with its GIL wait.
        #[allow(clippy::too_many_arguments)]
        fn worker(
            &mut self,
            pid: u32,
            socket: &str,
            rate: f64,
            busy: u32,
            slots: u32,
            p95: f64,
            gil: f64,
        ) {
            self.figures.workers.insert(
                socket.to_owned(),
                pb::Figures {
                    requests_per_second: Some(rate),
                    busy: Some(busy),
                    slots: Some(slots),
                    p95_ms: Some(p95),
                    ..pb::Figures::default()
                },
            );
            self.figures.signals.insert(pid, (Some(gil), None));
        }

        /// A worker group's figures over the last second.
        #[allow(clippy::too_many_arguments)]
        fn group(
            &mut self,
            app: &str,
            release: &str,
            group: &str,
            rate: f64,
            busy: u32,
            slots: u32,
            queue: u32,
            p95: f64,
            gil: f64,
        ) {
            self.figures.groups.insert(
                (app.to_owned(), release.to_owned(), group.to_owned()),
                pb::Figures {
                    requests_per_second: Some(rate),
                    busy: Some(busy),
                    slots: Some(slots),
                    queue: Some(queue),
                    p95_ms: Some(p95),
                    gil_wait_p95_ms: Some(gil),
                    ..pb::Figures::default()
                },
            );
        }

        /// The answer to ListProcesses for the instance.
        fn answer(self, mode: &str) -> pb::Processes {
            let mut root = tree_from(&self.known, &self.os, &self.cpu, &self.figures);

            fn name_users(node: &mut pb::Process, users: &HashMap<u32, &'static str>) {
                node.user = users.get(&node.pid).copied().unwrap_or_default().to_owned();

                for child in &mut node.children {
                    name_users(child, users);
                }
            }

            if let Some(root) = &mut root {
                name_users(root, &self.users);
            }

            pb::Processes {
                mode: mode.to_owned(),
                root,
                figures: Some(self.figures.instance),
                ..pb::Processes::default()
            }
        }
    }

    fn known(pid: u32, parent: u32, role: &'static str, what: &str) -> Known {
        Known {
            pid,
            parent,
            role,
            what: what.to_owned(),
            ..Known::default()
        }
    }

    /// A worker of a release, as the daemon or a run knows it.
    fn worker(
        pid: u32,
        host: u32,
        app: &str,
        release: &str,
        group: &str,
        index: u32,
        with_group: bool,
    ) -> Known {
        Known {
            app: app.to_owned(),
            release: release.to_owned(),
            group: group.to_owned(),
            index: Some(index),
            socket: Some(format!("{app}/{release}/{group}-{index}.sock")),
            ..known(
                pid,
                host,
                "worker",
                &worker_phrase(group, index, "ready", with_group),
            )
        }
    }

    /// A release's host and its log relay, as the daemon knows them.
    fn host(pid: u32, daemon: u32, app: &str, release: &str, groups: &[&str]) -> [Known; 2] {
        let subject = format!("{app}@{release}");

        [
            Known {
                app: app.to_owned(),
                release: release.to_owned(),
                groups: groups.iter().map(|group| (*group).to_owned()).collect(),
                ..known(pid, daemon, "host", &subject)
            },
            Known {
                app: app.to_owned(),
                release: release.to_owned(),
                spawned_by: daemon,
                ..known(
                    pid + 1,
                    pid,
                    "relay",
                    &format!("{subject} output to apps/{app}/{release}.log"),
                )
            },
        ]
    }

    /// The daemon guide's example: a daemon with two applications in
    /// worker processes, one with a named group, and a managed server.
    fn daemon_example() -> pb::Processes {
        let mut example = Example::default();
        let day = 86_400;
        let listening = [
            ("0.0.0.0:80".parse().expect("an address"), false),
            ("0.0.0.0:443".parse().expect("an address"), true),
        ];

        example.add(
            known(4242, 0, "daemon", "muructl daemon run"),
            1,
            "muru",
            3 * day + 2 * 3600,
            0.3,
            38.0,
        );
        example.add(
            known(
                4250,
                4242,
                "edge",
                &format!(
                    "listening on {}",
                    muru_controller::processes::addresses(&listening)
                ),
            ),
            4242,
            "muru",
            3 * day + 2 * 3600,
            2.1,
            61.0,
        );

        let [shop, relay] = host(4301, 4242, "shop", "r42", &["default", "crawlers"]);
        example.add(shop, 4242, "shop", 5 * 3600 + 12 * 60, 0.0, 92.0);
        example.add(relay, 4242, "shop", 5 * 3600 + 12 * 60, 0.1, 7.0);
        example.add(
            worker(4310, 4301, "shop", "r42", "default", 0, true),
            4301,
            "shop",
            5 * 3600 + 12 * 60,
            12.3,
            140.0,
        );
        example.add(
            worker(4311, 4301, "shop", "r42", "default", 1, true),
            4301,
            "shop",
            2 * 3600 + 3 * 60,
            11.8,
            138.0,
        );
        example.add(
            worker(4320, 4301, "shop", "r42", "crawlers", 0, true),
            4301,
            "shop",
            5 * 3600 + 12 * 60,
            1.0,
            120.0,
        );
        example.worker(4310, "shop/r42/default-0.sock", 512.0, 14, 16, 40.2, 2.0);
        example.worker(4311, "shop/r42/default-1.sock", 508.0, 13, 16, 42.0, 2.2);
        example.worker(4320, "shop/r42/crawlers-0.sock", 61.0, 4, 8, 120.0, 0.4);
        example.group("shop", "r42", "default", 1020.0, 27, 32, 3, 41.0, 2.1);
        example.group("shop", "r42", "crawlers", 61.0, 4, 8, 0, 120.0, 0.4);

        let [api, relay] = host(4330, 4242, "api", "r7", &["default"]);
        example.add(api, 4242, "api", day + 4 * 3600, 0.0, 88.0);
        example.add(relay, 4242, "api", day + 4 * 3600, 0.0, 7.0);
        example.add(
            worker(4340, 4330, "api", "r7", "default", 0, true),
            4330,
            "api",
            day + 4 * 3600,
            4.2,
            131.0,
        );
        example.worker(4340, "api/r7/default-0.sock", 203.0, 6, 64, 12.0, 1.1);
        example.group("api", "r7", "default", 203.0, 6, 64, 0, 12.0, 1.1);

        example.add(
            Known {
                app: "blog".to_owned(),
                release: "r3".to_owned(),
                index: Some(0),
                ..known(4401, 4242, "server", "blog@r3 instance 0")
            },
            4242,
            "blog",
            day,
            0.8,
            74.0,
        );

        example.figures.instance = pb::InstanceFigures {
            requests_per_second: 1284.0,
            errors_per_second: 3.0,
            p50_ms: 4.0,
            p95_ms: 38.0,
            in_flight: 41,
            websockets: 212,
            connections: 1903,
            tls_handshakes_per_second: 12.0,
            received_bytes_per_second: 2.1 * 1024.0 * 1024.0,
            sent_bytes_per_second: 46.4 * 1024.0 * 1024.0,
        };

        example.answer("daemon")
    }

    /// The architecture page's example: a development run of one
    /// application with two workers.
    fn run_example() -> pb::Processes {
        let mut example = Example::default();

        example.add(
            known(4100, 0, "run", "controller and edge, :8080"),
            4000,
            "you",
            12 * 60,
            1.1,
            24.0,
        );
        example.add(
            Known {
                app: "app".to_owned(),
                release: "r1".to_owned(),
                ..known(4105, 4100, "host", "app:application (r1)")
            },
            4100,
            "you",
            12 * 60,
            0.0,
            71.0,
        );
        example.add(
            Known {
                app: "app".to_owned(),
                release: "r1".to_owned(),
                spawned_by: 4100,
                ..known(4106, 4105, "relay", "output to muructl")
            },
            4100,
            "you",
            12 * 60,
            0.0,
            7.0,
        );
        example.add(
            worker(4107, 4105, "app", "r1", "default", 0, false),
            4105,
            "you",
            12 * 60,
            3.0,
            96.0,
        );
        example.add(
            worker(4108, 4105, "app", "r1", "default", 1, false),
            4105,
            "you",
            12 * 60,
            2.8,
            95.0,
        );
        example.worker(4107, "app/r1/default-0.sock", 0.3, 0, 4, 9.4, 0.1);
        example.worker(4108, "app/r1/default-1.sock", 0.2, 0, 4, 8.8, 0.1);
        example.group("app", "r1", "default", 0.5, 0, 8, 0, 9.4, 0.1);
        example.figures.instance = pb::InstanceFigures {
            requests_per_second: 0.5,
            p50_ms: 3.1,
            p95_ms: 9.4,
            connections: 1,
            sent_bytes_per_second: 118.0,
            ..pb::InstanceFigures::default()
        };

        example.answer("run")
    }

    /// The example in a page: the block that follows the command line.
    fn shown(page: &str) -> String {
        let marker = "```console\n$ muructl status --processes\n";
        let start = page.find(marker).expect("the page has the example") + marker.len();
        let end = start + page[start..].find("```").expect("the example ends");
        page[start..end].to_owned()
    }

    /// With `MURU_WRITE_EXAMPLES=1` the test writes what the command
    /// prints into the pages instead, for a change to the view.
    #[test]
    fn the_guides_examples_are_what_the_command_prints() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let pages = [
            ("docs/howto/daemon.md", daemon_example()),
            ("docs/explanation/architecture.md", run_example()),
        ];
        let write = std::env::var_os("MURU_WRITE_EXAMPLES").is_some();

        for (name, answer) in pages {
            let path = root.join(name);
            let page = std::fs::read_to_string(&path).expect("the page");
            let printed = render_at(&answer, None, NOW);
            let example = shown(&page);

            if write {
                std::fs::write(&path, page.replacen(&example, &printed, 1)).expect("the page");
                continue;
            }

            assert_eq!(
                example, printed,
                "{name}: the example is not what the command prints; \
                 MURU_WRITE_EXAMPLES=1 writes it"
            );
        }
    }

    #[test]
    fn figures_read_as_a_person_would_say_them() {
        assert_eq!(thousands(1_903), "1,903");
        assert_eq!(thousands(12), "12");
        assert_eq!(rate(1284.4), "1,284");
        assert_eq!(rate(0.5), "0.5");
        assert_eq!(uptime(3 * 86_400_000 + 2 * 3_600_000), "3d2h");
        assert_eq!(uptime(5 * 3_600_000 + 12 * 60_000), "5h12m");
        assert_eq!(uptime(45_000), "45s");
    }

    #[test]
    fn an_application_and_a_release_are_named_together() {
        let args = ProcessArgs {
            processes: true,
            app: Some("shop@r42".to_owned()),
            ..ProcessArgs::default()
        };
        let asked = request(&args);
        assert_eq!(
            (asked.app.as_str(), asked.release.as_str()),
            ("shop", "r42")
        );
    }
}
