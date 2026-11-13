//! The Processes section (design specification sections 10.7 and 13.10):
//! the same tree, columns and live figures as `muructl status
//! --processes`, from the same admin operation's answer, so the two
//! always agree. A worker's row links to its worker page, and a host's or
//! relay's to the logs. It shows and changes nothing.

use std::sync::Arc;

use askama::Template;

/// Produces the answer to ListProcesses as JSON, as `muructl status
/// --processes --json` prints it, or why it could not be had.
pub type ProcessesSource =
    Arc<dyn Fn() -> crate::BoxFuture<Result<serde_json::Value, String>> + Send + Sync>;

/// One row of the table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessRow {
    /// The pid, or empty for a group's row.
    pub pid: String,
    /// The tree's lines in front of the role.
    pub tree: String,
    /// The role.
    pub role: String,
    /// What the process is for.
    pub what: String,
    /// Where the row links to, relative to the dashboard.
    pub link: Option<String>,
    /// Said of a process the operating system gives another parent.
    pub note: String,
    /// The user.
    pub user: String,
    /// How long it has been up.
    pub up: String,
    /// Its CPU use.
    pub cpu: String,
    /// Its resident memory.
    pub rss: String,
    /// Requests per second.
    pub rate: String,
    /// Busy slots of the slots.
    pub busy: String,
    /// Requests waiting.
    pub queue: String,
    /// p95 total time.
    pub p95: String,
    /// GIL wait or loop lag p95.
    pub wait: String,
}

/// The section's fragment.
#[derive(Template)]
#[template(path = "processes.html")]
pub struct ProcessesFragment<'a> {
    /// The rows, in the tree's order.
    pub rows: &'a [ProcessRow],
    /// Whether the user column is shown: under the daemon.
    pub with_user: bool,
    /// Why there are no rows, or empty.
    pub error: &'a str,
    /// Whether a row's figures count a worker no longer listed, which a
    /// note under the table explains (issue 613).
    pub marked: bool,
}

/// The rows of an answer, with `now` in Unix milliseconds for uptimes.
pub fn rows(answer: &serde_json::Value, now: u64) -> Vec<ProcessRow> {
    let mut rows = Vec::new();

    if let Some(root) = answer.get("root").filter(|root| root.is_object()) {
        flatten(root, "", "", true, now, &mut rows);
    }

    rows
}

fn text(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn number(value: &serde_json::Value, key: &str) -> Option<f64> {
    value.get(key).and_then(serde_json::Value::as_f64)
}

fn flatten(
    process: &serde_json::Value,
    lead: &str,
    branch: &str,
    root: bool,
    now: u64,
    rows: &mut Vec<ProcessRow>,
) {
    let role = text(process, "role");
    let app = text(process, "app");
    let release = text(process, "release");
    let index = number(process, "index").map(|index| index as u32);

    // A worker's page names it by its index within its group, after the
    // group's name outside the default group.

    let link = match role.as_str() {
        "worker" => index.map(|index| {
            let worker = muru_common::worker::WorkerName::new(&text(process, "group"), index);
            format!("worker?release={app}@{release}&worker={worker}")
        }),
        "host" | "relay" => Some("#logs".to_owned()),
        _ => None,
    };

    let note = if process
        .get("readopted")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        format!(
            "re-adopted, OS parent {}",
            number(process, "os_parent_pid").unwrap_or_default() as u64
        )
    } else {
        String::new()
    };

    let mut row = ProcessRow {
        pid: number(process, "pid")
            .map(|pid| (pid as u64).to_string())
            .unwrap_or_default(),
        tree: format!("{lead}{branch}"),
        role,
        what: text(process, "what"),
        link,
        note,
        user: text(process, "user"),
        up: number(process, "started_unix_ms")
            .map(|started| uptime(now.saturating_sub(started as u64)))
            .unwrap_or_default(),
        cpu: number(process, "cpu_percent")
            .map(|percent| format!("{percent:.1}%"))
            .unwrap_or_default(),
        rss: number(process, "rss_bytes")
            .map(|bytes| size(bytes as u64))
            .unwrap_or_default(),
        ..ProcessRow::default()
    };
    let interface = text(process, "interface");
    figures(process.get("figures"), &interface, &mut row);
    rows.push(row);

    let below = if root {
        String::new()
    } else if branch.starts_with('└') {
        format!("{lead}   ")
    } else {
        format!("{lead}│  ")
    };

    let children: &[serde_json::Value] = process
        .get("children")
        .and_then(serde_json::Value::as_array)
        .map_or(&[], Vec::as_slice);
    let groups: &[serde_json::Value] = process
        .get("groups")
        .and_then(serde_json::Value::as_array)
        .map_or(&[], Vec::as_slice);
    let mut shown_group = String::new();

    for (at, child) in children.iter().enumerate() {
        let group = text(child, "group");

        // A group's row before its first worker, when there are several.

        if text(child, "role") == "worker"
            && group != shown_group
            && let Some(figured) = groups
                .iter()
                .find(|figured| text(figured, "group") == group)
        {
            let mut row = ProcessRow {
                tree: format!("{below}├─ "),
                role: "group".to_owned(),
                what: group.clone(),
                ..ProcessRow::default()
            };
            figures(figured.get("figures"), &interface, &mut row);
            rows.push(row);
            shown_group = group;
        }

        let branch = if at + 1 == children.len() {
            "└─ "
        } else {
            "├─ "
        };
        flatten(child, &below, branch, false, now, rows);
    }
}

/// The figure cells of a row from its figures. The wait is loop lag for a
/// release whose default mount is ASGI, `interface`, and GIL wait for any
/// other, either when the host has not said which.
fn figures(figures: Option<&serde_json::Value>, interface: &str, row: &mut ProcessRow) {
    let Some(figures) = figures.filter(|figures| figures.is_object()) else {
        return;
    };

    // Issue 613: marked where the figures count, for a second, a worker
    // that has exited and is no longer listed.

    let mark = if number(figures, "departed").is_some_and(|count| count > 0.0) {
        "*"
    } else {
        ""
    };
    row.rate = number(figures, "requests_per_second")
        .map(|value| format!("{}{mark}", rate(value)))
        .unwrap_or_default();
    row.busy = match (number(figures, "busy"), number(figures, "slots")) {
        (Some(busy), Some(slots)) => format!("{}/{}", busy as u64, slots as u64),
        _ => String::new(),
    };
    row.queue = number(figures, "queue")
        .map(|queue| (queue as u64).to_string())
        .unwrap_or_default();
    row.p95 = number(figures, "p95_ms")
        .map(|ms| format!("{ms:.1} ms"))
        .unwrap_or_default();
    let gil = number(figures, "gil_wait_p95_ms");
    let lag = number(figures, "loop_lag_p95_ms");

    row.wait = match interface {
        "asgi" => lag,
        "" => gil.or(lag),
        _ => gil,
    }
    .map(|ms| format!("{ms:.1} ms"))
    .unwrap_or_default();
}

fn rate(value: f64) -> String {
    if value >= 10.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "KB")];

    for (scale, unit) in UNITS {
        if bytes >= scale {
            return format!("{:.1}{unit}", bytes as f64 / scale as f64);
        }
    }

    format!("{bytes}B")
}

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

    #[test]
    fn a_worker_links_to_its_page_and_a_readopted_host_says_so() {
        let answer = serde_json::json!({
            "root": {
                "pid": 10, "role": "daemon", "what": "user daemon, state ~/.local/state/muru",
                "children": [{
                    "pid": 20, "role": "host", "what": "shop@r1", "app": "shop", "release": "r1",
                    "readopted": true, "os_parent_pid": 1,
                    "groups": [{"group": "default", "figures": {"queue": 2}}, {"group": "crawlers"}],
                    "children": [
                        {"pid": 21, "role": "worker", "app": "shop", "release": "r1", "group": "default", "index": 0,
                         "interface": "asgi",
                         "figures": {"requests_per_second": 12.0, "busy": 1, "slots": 4, "p95_ms": 3.25,
                                     "gil_wait_p95_ms": 0.2, "loop_lag_p95_ms": 30.0}},
                        {"pid": 22, "role": "worker", "app": "shop", "release": "r1", "group": "crawlers", "index": 0}
                    ]
                }]
            }
        });
        let rows = rows(&answer, 0);
        let roles: Vec<&str> = rows.iter().map(|row| row.role.as_str()).collect();
        assert_eq!(
            roles,
            ["daemon", "host", "group", "worker", "group", "worker"]
        );
        assert_eq!(rows[1].note, "re-adopted, OS parent 1");
        assert_eq!(rows[1].link.as_deref(), Some("#logs"));
        assert_eq!(rows[2].queue, "2");
        assert_eq!(
            rows[3].link.as_deref(),
            Some("worker?release=shop@r1&worker=0")
        );
        assert_eq!(
            (rows[3].rate.as_str(), rows[3].busy.as_str()),
            ("12", "1/4")
        );

        // An ASGI worker's wait is its loop's lag, though it has a GIL
        // wait as well.

        assert_eq!(rows[3].wait, "30.0 ms");
        assert_eq!(
            rows[5].link.as_deref(),
            Some("worker?release=shop@r1&worker=crawlers/0")
        );
        assert_eq!(rows[5].tree, "   └─ ");
    }
}
