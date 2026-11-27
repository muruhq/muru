//! `muructl daemon reset` (design specification section 13.13, ADR 0081):
//! put an instance back to nothing, with everything of it stopped. It
//! removes the state store and what releases left, lists what goes and
//! what stays, and asks. It works on files alone, so it needs no daemon
//! and helps with a store a daemon can no longer read.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Args;
use muru_controller::locations::Locations;
use serde_json::json;

use crate::admin::{DaemonFileArgs, JsonArgs, SocketArgs};

/// What `daemon reset` takes.
#[derive(Args)]
pub struct ResetArgs {
    /// Also remove the applications' data directories under the state
    /// directory.
    #[arg(long)]
    data: bool,

    /// Also remove the log directory's files.
    #[arg(long)]
    logs: bool,

    /// Only list what would be removed and what would stay.
    #[arg(long)]
    dry_run: bool,

    /// Do it without asking.
    #[arg(long)]
    yes: bool,

    #[command(flatten)]
    socket: SocketArgs,

    /// The daemon's own files, as it was started with them.
    #[command(flatten)]
    files: DaemonFileArgs,

    #[command(flatten)]
    output: JsonArgs,
}

/// One thing a reset removes or keeps.
struct Item {
    path: PathBuf,
    what: String,
    bytes: Option<u64>,
}

/// What the store holds, as the list says it, or why it cannot be said.
fn store_counts(path: &Path) -> String {
    let counted = muru_controller::store::Store::open_read_only(path).and_then(|store| {
        Ok((
            store.apps()?.len(),
            store.releases(None)?.len(),
            store.hosts()?.len(),
            store.routes()?.len(),
        ))
    });
    let plural =
        |count: usize, word: &str| format!("{count} {word}{}", if count == 1 { "" } else { "s" });

    match counted {
        Ok((apps, releases, hosts, routes)) => format!(
            "the state store: {}, {}, {} and {}",
            plural(apps, "application"),
            plural(releases, "release"),
            plural(hosts, "host"),
            plural(routes, "route")
        ),
        Err(_) => "the state store, which could not be read".to_owned(),
    }
}

/// What is there to remove, and what stays.
fn plan(locations: &Locations, data: bool, logs: bool) -> (Vec<Item>, Vec<Item>) {
    let state = &locations.state.path;
    let measured = |path: &Path| {
        muru_controller::disk::measure(path)
            .ok()
            .map(|taken| taken.freed)
    };
    let item = |path: PathBuf, what: &str| Item {
        bytes: measured(&path),
        what: what.to_owned(),
        path,
    };

    let mut remove = Vec::new();
    let mut keep = Vec::new();
    let store = locations.state_store();

    if store.exists() {
        remove.push(Item {
            bytes: measured(&store),
            what: store_counts(&store),
            path: store.clone(),
        });
    }

    // SQLite's own files beside the store go with it.

    for suffix in ["-wal", "-shm", "-journal"] {
        let beside = PathBuf::from(format!("{}{suffix}", store.display()));

        if beside.exists() {
            remove.push(item(beside, "the state store's journal"));
        }
    }

    for (name, what) in [
        ("envs", "the environments built for releases"),
        ("pycache", "the releases' bytecode"),
        ("static", "the releases' static directories"),
        ("spool", "the upload spool"),
    ] {
        let path = state.join(name);

        if path.exists() {
            remove.push(item(path, what));
        }
    }

    // Nothing runs, so what the run directory holds is left over: the
    // sockets of an edge and hosts that are gone.

    if let Ok(entries) = std::fs::read_dir(&locations.run.path) {
        for entry in entries.flatten() {
            remove.push(item(entry.path(), "left in the run directory"));
        }
    }

    // Their records are among the runtime files, in the state directory.
    // The default daemon's are beside the files of the user's
    // development runs, each in a directory named `run-` and its ID,
    // which are those runs' and stay.

    if let Ok(entries) = std::fs::read_dir(locations.files()) {
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("run-") {
                remove.push(item(entry.path(), "left among the runtime files"));
            }
        }
    }

    let data_dir = state.join("data");

    if data_dir.exists() {
        if data {
            remove.push(item(data_dir, "the applications' data"));
        } else {
            keep.push(item(data_dir, "the applications' data (--data removes it)"));
        }
    }

    if locations.logs.path.exists() {
        if logs {
            remove.push(item(locations.logs.path.clone(), "the logs"));
        } else {
            keep.push(item(
                locations.logs.path.clone(),
                "the logs (--logs removes them)",
            ));
        }
    }

    for (path, what) in [
        (
            locations.config.path.clone(),
            "the daemon's configuration file",
        ),
        (state.join("secrets"), "the file secret stores"),
        (state.join("instance-id"), "the instance's identity"),
    ] {
        if path.exists() {
            keep.push(Item {
                path,
                what: what.to_owned(),
                bytes: None,
            });
        }
    }

    (remove, keep)
}

/// What of the instance still runs, each as a sentence, none when a
/// reset may go ahead.
fn running(args: &ResetArgs, locations: &Locations) -> anyhow::Result<Vec<String>> {
    let mut found = Vec::new();
    let socket = args.socket.socket()?;

    if !crate::admin::no_daemon(&args.socket, &socket) && socket.exists() {
        found.push(format!("a daemon answers at {}", socket.display()));
    }

    // The registry knows a daemon that detached and left its edge and
    // hosts serving, and one that still runs.

    let processes = muru_controller::lookup::muru_processes();
    let registries = crate::instances::registries(false)?;

    for row in crate::instances::rows(&registries, &processes) {
        let ours = row.record.run_dir == locations.run.path
            || row.record.state_dir.as_deref() == Some(locations.state.path.as_path());

        if !ours || !row.alive() {
            continue;
        }

        let owned: Vec<String> = processes
            .iter()
            .filter(|process| row.owns(process))
            .map(|process| format!("{} (pid {})", process.role, process.pid))
            .collect();
        found.push(format!(
            "the instance is {}{}",
            row.state.word(),
            if owned.is_empty() {
                String::new()
            } else {
                format!(": {}", owned.join(", "))
            }
        ));
    }

    Ok(found)
}

/// Remove an instance's state, with everything of it stopped. The exit
/// code is 0 when the reset went ahead or, with `--dry-run`, would be
/// allowed, and 1 when something of the instance runs.
pub fn reset(args: ResetArgs) -> anyhow::Result<i32> {
    let locations = crate::admin::daemon_locations(&args.socket, &args.files);
    let running = running(&args, &locations)?;
    let (remove, keep) = plan(&locations, args.data, args.logs);
    let size = |bytes: Option<u64>| bytes.map(crate::admin::human_size).unwrap_or_default();

    if args.output.json {
        let listed = |items: &[Item]| -> Vec<serde_json::Value> {
            items
                .iter()
                .map(|item| json!({"path": item.path, "what": item.what, "bytes": item.bytes}))
                .collect()
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "allowed": running.is_empty(),
                "running": running,
                "remove": listed(&remove),
                "keep": listed(&keep),
            }))?
        );
    } else {
        println!(
            "muru: this removes the state of the daemon at {}:",
            locations.state.path.display()
        );

        for item in &remove {
            println!(
                "  remove  {}  {} {}",
                item.path.display(),
                item.what,
                size(item.bytes)
            );
        }

        if remove.is_empty() {
            println!("  nothing: there is no state to remove");
        }

        for item in &keep {
            println!("  keep    {}  {}", item.path.display(), item.what);
        }
    }

    // Nothing is removed while any part of the instance runs: a host
    // would go on serving from files taken from under it.

    if !running.is_empty() {
        for line in &running {
            eprintln!("muru: {line}");
        }

        eprintln!(
            "muru: error: nothing was removed: stop it all first with muructl daemon stop \
             --shutdown"
        );
        return Ok(1);
    }

    if args.dry_run || remove.is_empty() {
        return Ok(0);
    }

    if !args.yes && args.output.json {
        anyhow::bail!("give --yes to go ahead; with --json nothing is asked");
    }

    // With nobody to ask and no --yes, the list is all there is.

    if !args.yes && !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!("muru: nothing was removed: give --yes to go ahead without a terminal");
        return Ok(1);
    }

    if !crate::admin::confirm("Remove?", args.yes)? {
        eprintln!("muru: nothing was changed");
        return Ok(0);
    }

    for item in &remove {
        let gone = if item.path.is_dir() {
            std::fs::remove_dir_all(&item.path)
        } else {
            std::fs::remove_file(&item.path)
        };
        gone.with_context(|| format!("removing {}", item.path.display()))?;
    }

    if !args.output.json {
        println!("muru: removed; the next start finds nothing to serve");
    }

    Ok(0)
}
