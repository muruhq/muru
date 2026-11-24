//! `muructl detour` (design specification sections 9.13 and 13.12, ADR
//! 0075): a development run served behind a local daemon's edge as a
//! release of an application, held open by a streaming admin call for as
//! long as the run runs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand};
use muru_proto::admin::v1 as pb;
use pb::detour_message::What;
use tokio_stream::StreamExt as _;

use crate::admin::SocketArgs;

/// How long a run waits between tries to open its detour again.
const RETRY_EVERY: Duration = Duration::from_secs(2);

/// The socket a detour's edge serves on, in the run's own directory.
pub const SOCKET: &str = "detour.sock";

/// The detours of a daemon.
#[derive(Subcommand)]
pub enum DetourCommand {
    /// Serve the project in development mode behind a local daemon's edge,
    /// as a release of an application, for as long as this runs: the
    /// daemon routes previews to it, or more when asked, with reload, the
    /// toolbar and tracebacks of a development run.
    Run(Box<DetourRunArgs>),

    /// Change what a detour takes while it runs: preview, all, share=N or class=NAME.
    Traffic(TrafficArgs),

    /// Open a one-time entry link to a detour's preview in the browser, or print it.
    Preview(PreviewArgs),

    /// List the daemon's open detours.
    List(ListArgs),
}

/// `muructl detour list`.
#[derive(Args)]
pub struct ListArgs {
    #[command(flatten)]
    output: crate::admin::JsonArgs,

    /// Which daemon's detours.
    #[command(flatten)]
    socket: SocketArgs,
}

/// `muructl detour preview`.
#[derive(Args)]
pub struct PreviewArgs {
    /// The detour: its release, its application, its label, the tag
    /// muructl instance list shows or a prefix of its ID (default: the
    /// project's application, else the one detour open).
    #[arg(value_name = "DETOUR")]
    detour: Option<String>,

    /// A link for this host of the application's, in place of its best
    /// address.
    #[arg(long, value_name = "HOST", conflicts_with = "list")]
    host: Option<String>,

    /// Print a link for each of the application's addresses, and open
    /// none.
    #[arg(long)]
    list: bool,

    /// Print the link, and open nothing.
    #[arg(long)]
    print: bool,

    /// Print the Muru-Preview header that previews the detour without a
    /// cookie, for curl or a test client.
    #[arg(long, conflicts_with_all = ["list", "print", "host"])]
    header: bool,

    #[command(flatten)]
    output: crate::admin::JsonArgs,

    /// Which daemon the detour is open on.
    #[command(flatten)]
    socket: SocketArgs,
}

/// `muructl detour traffic`.
#[derive(Args)]
pub struct TrafficArgs {
    /// The detour, then what it is to take. The detour is its release, its
    /// application, its label, the tag muructl instance list shows or a
    /// prefix of its ID (default: the project's application, else the one
    /// detour open); the choice is preview, all, share=N or class=NAME.
    #[arg(value_name = "[DETOUR] CHOICE", num_args = 1..=2, required = true)]
    words: Vec<String>,

    /// Go back to preview on its own after this long, for example 10m.
    #[arg(long = "for", value_name = "DURATION", value_parser = muru_config::parse_duration, conflicts_with = "until")]
    duration: Option<Duration>,

    /// Go back to preview on its own at this time, such as
    /// 2026-11-02T09:00 or an RFC 3339 time.
    #[arg(long, value_name = "TIME")]
    until: Option<String>,

    /// The time zone --until is in; the local zone without it.
    #[arg(long, value_name = "ZONE", requires = "until")]
    timezone: Option<String>,

    #[command(flatten)]
    output: crate::admin::JsonArgs,

    /// Which daemon the detour is open on.
    #[command(flatten)]
    socket: SocketArgs,
}

/// `muructl detour run`.
#[derive(Args)]
pub struct DetourRunArgs {
    /// The application on the daemon the detour is a release of
    /// (default: the project's, `[project] name` from pyproject.toml,
    /// else the module of its entrypoint).
    #[arg(value_name = "APP")]
    app: Option<String>,

    /// What the detour takes: preview, only browsers with its preview
    /// cookie; all, every request for the application; share=N, N percent
    /// of its clients; or class=NAME, a client class the daemon defines.
    #[arg(long, value_name = "CHOICE", default_value = "preview")]
    traffic: String,

    /// Open an entry link to the detour's preview in the browser once the
    /// detour is open, for the application's best address.
    #[arg(long)]
    open_preview: bool,

    /// Give the run the application's data directory on the daemon, which
    /// its releases share, in place of the run's own.
    #[arg(long)]
    app_data: bool,

    #[command(flatten)]
    pub label: crate::instances::LabelArgs,

    /// Python to run under: a path or a version (default: as muructl run
    /// finds it).
    #[arg(long, value_name = "PATH or VERSION")]
    python: Option<String>,

    /// Worker processes.
    #[arg(long)]
    processes: Option<usize>,

    /// Threads per worker.
    #[arg(long)]
    threads: Option<usize>,

    /// One process and one thread, nothing forked, for a debugger.
    #[arg(long)]
    single: bool,

    /// The project directory (default: the current directory).
    #[arg(long)]
    chdir: Option<PathBuf>,

    /// Bring the environment up to date with the lock first: always, or
    /// the mode given (auto, always or never).
    #[arg(long, value_name = "MODE", num_args = 0..=1, default_missing_value = "always")]
    build: Option<String>,

    /// A .env file the run starts with.
    #[arg(long, value_name = "FILE")]
    env_file: Option<PathBuf>,

    /// The largest request body the run's edge takes, such as 50MB.
    #[arg(long, value_name = "SIZE")]
    max_body_size: Option<String>,

    /// Reload on change (the default).
    #[arg(long, conflicts_with = "no_reload")]
    reload: bool,

    /// Do not reload on change.
    #[arg(long)]
    no_reload: bool,

    /// A directory to watch for changes instead of the project.
    #[arg(long = "reload-dir", value_name = "DIR")]
    reload_dir: Vec<PathBuf>,

    /// Files to watch beside Python files, as a glob.
    #[arg(long = "reload-include", value_name = "GLOB")]
    reload_include: Vec<String>,

    /// Files whose changes are ignored, as a glob.
    #[arg(long = "reload-exclude", value_name = "GLOB")]
    reload_exclude: Vec<String>,

    /// Insert the development toolbar into HTML pages.
    #[arg(long, conflicts_with = "no_toolbar")]
    toolbar: bool,

    /// Leave the toolbar out, whatever the settings say.
    #[arg(long)]
    no_toolbar: bool,

    /// Trace the run with wrapture, over the settings files' tracing.
    #[arg(long, conflicts_with = "no_tracing")]
    tracing: bool,

    /// No tracing for the run, whatever the settings say.
    #[arg(long)]
    no_tracing: bool,

    /// The address of the run's own dashboard, or off (default: a free
    /// loopback port).
    #[arg(long, value_name = "ADDRESS")]
    dashboard: Option<String>,

    /// Print the settings the detour's run would use, and where each came
    /// from, then exit.
    #[arg(long)]
    print_config: bool,

    /// Which daemon the detour opens on.
    #[command(flatten)]
    socket: SocketArgs,
}

/// A clap parser for `muructl run`'s arguments alone, which a detour
/// fills from its own.
#[derive(Parser)]
struct RunOnly {
    #[command(flatten)]
    args: crate::RunArgs,
}

/// What a run does as a detour, beside what it does as any run.
pub struct Mode {
    /// The application on the daemon.
    pub app: String,
    /// The daemon's admin socket.
    pub daemon: PathBuf,
    /// What the detour takes, as the flag gives it.
    pub traffic: String,
    /// Open an entry link once the detour is open.
    pub open_preview: bool,
    /// Use the application's data directory on the daemon.
    pub app_data: bool,
    /// The run's label, or empty.
    pub label: String,
}

impl DetourRunArgs {
    /// The words of the `muructl run` this detour runs, for its parser.
    fn run_words(&self) -> Vec<OsString> {
        let mut words: Vec<OsString> = vec!["muructl-run".into()];
        let mut flag = |name: &str, value: Option<OsString>| {
            words.push(format!("--{name}").into());

            if let Some(value) = value {
                words.push(value);
            }
        };

        let given = |value: &Option<String>| value.clone().map(OsString::from);
        let path = |value: &Option<PathBuf>| value.clone().map(OsString::from);

        for (name, value) in [
            ("python", given(&self.python)),
            ("processes", self.processes.map(|n| n.to_string().into())),
            ("threads", self.threads.map(|n| n.to_string().into())),
            ("max-body-size", given(&self.max_body_size)),
            ("dashboard", given(&self.dashboard)),
        ] {
            if let Some(value) = value {
                flag(name, Some(value));
            }
        }

        for (name, value) in [
            ("chdir", path(&self.chdir)),
            ("env-file", path(&self.env_file)),
        ] {
            if let Some(value) = value {
                flag(name, Some(value));
            }
        }

        // `--build=MODE` is one word, so the mode is never taken for
        // the entrypoint.

        if let Some(mode) = &self.build {
            flag(&format!("build={mode}"), None);
        }

        for (name, on) in [
            ("single", self.single),
            ("reload", self.reload),
            ("no-reload", self.no_reload),
            ("toolbar", self.toolbar),
            ("no-toolbar", self.no_toolbar),
            ("tracing", self.tracing),
            ("no-tracing", self.no_tracing),
            ("print-config", self.print_config),
        ] {
            if on {
                flag(name, None);
            }
        }

        for directory in &self.reload_dir {
            flag("reload-dir", Some(directory.clone().into()));
        }

        for glob in &self.reload_include {
            flag("reload-include", Some(glob.into()));
        }

        for glob in &self.reload_exclude {
            flag("reload-exclude", Some(glob.into()));
        }

        words
    }
}

/// The project directory the detour serves: `--chdir`, else the current
/// directory.
fn project_dir(args: &DetourRunArgs) -> anyhow::Result<PathBuf> {
    match &args.chdir {
        Some(directory) => Ok(directory.clone()),
        None => std::env::current_dir().context("finding the current directory"),
    }
}

/// Run `muructl detour run`.
pub async fn run(args: DetourRunArgs) -> anyhow::Result<()> {
    muru_controller::detours::Traffic::parse(&args.traffic).map_err(anyhow::Error::msg)?;

    let mut run_args = RunOnly::try_parse_from(args.run_words())
        .map_err(|error| anyhow::anyhow!("{error}"))?
        .args;

    // The application is the daemon's name for it, which the run takes
    // as its own, so its records and the daemon's name the same thing.

    let project = project_dir(&args)?;

    let app = match &args.app {
        Some(app) => app.clone(),
        None => crate::open::project_app(&project).ok_or_else(|| {
            anyhow::anyhow!(
                "no application is named and {} names none ([project] name in pyproject.toml, \
                 or a [tool.muru] entrypoint); give one: muructl detour run APP",
                project.display()
            )
        })?,
    };

    anyhow::ensure!(
        muru_config::valid_name(&app),
        "{app:?} is not an application name: lowercase letters, digits and hyphens"
    );

    run_args.name = Some(app.clone());
    run_args.detour = true;

    if args.dashboard.is_none() {
        run_args.dashboard = Some("127.0.0.1:0".to_owned());
    }

    let daemon = if args.print_config {
        PathBuf::new()
    } else {
        match args.socket.endpoint()? {
            muru_controller::client::Endpoint::Unix { path } => path,
        }
    };

    let mode = Mode {
        app,
        daemon,
        traffic: args.traffic.clone(),
        open_preview: args.open_preview,
        app_data: args.app_data,
        label: args.label.label.clone().unwrap_or_default(),
    };

    crate::run_with(run_args, None, None, Some(mode)).await
}

/// The daemon's open detours.
async fn listed(
    client: &mut muru_proto::admin::v1::admin_client::AdminClient<tonic::transport::Channel>,
) -> anyhow::Result<Vec<pb::Detour>> {
    Ok(client
        .list_detours(pb::ListDetoursRequest {})
        .await
        .map_err(crate::admin::explain)?
        .into_inner()
        .detours)
}

/// The detour a command names, or the one a command run from a checkout
/// means: the project's application's, else the daemon's one detour.
fn chosen<'a>(wanted: Option<&str>, detours: &'a [pb::Detour]) -> anyhow::Result<&'a pb::Detour> {
    let project = std::env::current_dir()
        .ok()
        .and_then(|directory| crate::open::project_app(&directory))
        .filter(|app| detours.iter().any(|detour| &detour.app == app));
    let wanted = wanted.map(str::to_owned).or(project);

    muru_controller::detours::choose(wanted.as_deref(), detours).map_err(anyhow::Error::msg)
}

/// Run `muructl detour list`.
pub async fn list(args: ListArgs) -> anyhow::Result<()> {
    let mut client = muru_controller::client::connect(&args.socket.endpoint()?).await?;
    let detours = listed(&mut client).await?;

    if args.output.json {
        return crate::admin::print_answer(&pb::ListDetoursResponse { detours });
    }

    if detours.is_empty() {
        println!("no detours are open");
        return Ok(());
    }

    for line in table(&detours) {
        println!("{line}");
    }

    Ok(())
}

/// The detours as `detour list` prints them: a heading, then one line
/// for each, the columns lined up.
fn table(detours: &[pb::Detour]) -> Vec<String> {
    let rows: Vec<[String; 7]> = detours
        .iter()
        .map(|detour| {
            [
                detour.app.clone(),
                detour.release.clone(),
                if detour.label.is_empty() {
                    "-".to_owned()
                } else {
                    detour.label.clone()
                },
                detour.pid.to_string(),
                muru_controller::detours::traffic_text(&detour.traffic, detour.until_unix_ms),
                crate::instances::uptime(u64::try_from(detour.opened_unix_ms).unwrap_or_default()),
                muru_controller::registry::home_shortened(Path::new(&detour.directory)),
            ]
        })
        .collect();
    let headings = [
        "APP",
        "RELEASE",
        "LABEL",
        "PID",
        "TRAFFIC",
        "SINCE",
        "DIRECTORY",
    ];
    let widths: Vec<usize> = (0..headings.len() - 1)
        .map(|column| {
            rows.iter()
                .map(|row| row[column].len())
                .chain(std::iter::once(headings[column].len()))
                .max()
                .unwrap_or(0)
        })
        .collect();

    let line = |cells: Vec<&str>| {
        let mut text = String::new();

        for (column, cell) in cells.iter().enumerate() {
            match widths.get(column) {
                Some(width) => text.push_str(&format!("{cell:<width$}  ")),
                None => text.push_str(cell),
            }
        }

        text.trim_end().to_owned()
    };

    std::iter::once(line(headings.to_vec()))
        .chain(
            rows.iter()
                .map(|row| line(row.iter().map(String::as_str).collect())),
        )
        .collect()
}

/// Run `muructl detour traffic`.
pub async fn traffic(args: TrafficArgs) -> anyhow::Result<()> {
    let (wanted, choice) = match args.words.as_slice() {
        [choice] => (None, choice.clone()),
        [wanted, choice] => (Some(wanted.as_str()), choice.clone()),
        _ => anyhow::bail!("give the detour's traffic, as muructl detour traffic [DETOUR] all"),
    };

    let traffic = muru_controller::detours::Traffic::parse(&choice).map_err(anyhow::Error::msg)?;

    // A timed choice goes back to preview, so preview itself takes no
    // time.

    let until = match (args.duration, &args.until) {
        (Some(duration), _) => {
            crate::admin::now_unix_ms().saturating_add(duration.as_millis() as u64)
        }
        (None, Some(text)) => muru_config::parse_moment(text, args.timezone.as_deref())
            .map_err(anyhow::Error::msg)?
            .max(0) as u64,
        (None, None) => 0,
    };

    if until != 0 && traffic == muru_controller::detours::Traffic::Preview {
        anyhow::bail!(
            "a detour goes back to preview when --for or --until ends, so preview takes neither"
        );
    }

    let mut client = muru_controller::client::connect(&args.socket.endpoint()?).await?;
    let detours = listed(&mut client).await?;
    let detour = chosen(wanted, &detours)?;
    let before = detour.traffic.clone();

    let after = client
        .set_detour_traffic(pb::SetDetourTrafficRequest {
            app: detour.app.clone(),
            release: detour.release.clone(),
            traffic: choice,
            until_unix_ms: until,
        })
        .await
        .map_err(crate::admin::explain)?
        .into_inner();

    if args.output.json {
        return crate::admin::print_answer(&after);
    }

    muru_common::muru_println!(
        "detour {}@{} now takes {}{} (was {before})",
        after.app,
        after.release,
        takes(&after.traffic, &after.app),
        ending(after.until_unix_ms)
    );
    Ok(())
}

/// When a timed choice ends, as a line says it after what the detour
/// takes; nothing for one that lasts until changed.
pub fn ending(until_unix_ms: u64) -> String {
    if until_unix_ms == 0 {
        String::new()
    } else {
        format!(
            ", until {}, then previews only",
            crate::admin::format_time(until_unix_ms)
        )
    }
}

/// Ask the daemon for entry links to a detour's preview.
async fn mint(
    client: &mut muru_proto::admin::v1::admin_client::AdminClient<tonic::transport::Channel>,
    detour: &pb::Detour,
    host: Option<&str>,
    all: bool,
) -> anyhow::Result<pb::DetourPreview> {
    Ok(client
        .preview_detour(pb::PreviewDetourRequest {
            app: detour.app.clone(),
            release: detour.release.clone(),
            host: host.unwrap_or_default().to_owned(),
            all,
        })
        .await
        .map_err(crate::admin::explain)?
        .into_inner())
}

/// Open an entry link in the browser, saying where it leads and how
/// many other addresses there are.
fn open_link(preview: &pb::DetourPreview, detour: &str) {
    let Some(link) = preview.links.first() else {
        return;
    };

    let others = match preview.others {
        0 => String::new(),
        1 => " (1 other address: muructl detour preview --list)".to_owned(),
        count => format!(" ({count} other addresses: muructl detour preview --list)"),
    };

    muru_common::muru_println!("opened {} in preview of {detour}{others}", link.address);
    let url = link.url.clone();
    std::thread::spawn(move || crate::browser::open(&url));
}

/// Run `muructl detour preview`.
pub async fn preview(args: PreviewArgs) -> anyhow::Result<()> {
    let mut client = muru_controller::client::connect(&args.socket.endpoint()?).await?;
    let detours = listed(&mut client).await?;
    let detour = chosen(args.detour.as_deref(), &detours)?.clone();
    let name = format!("{}@{}", detour.app, detour.release);
    let preview = mint(&mut client, &detour, args.host.as_deref(), args.list).await?;

    // An agent or a script is given the links and opens nothing.

    if args.output.json {
        return crate::admin::print_answer(&preview);
    }

    if args.header {
        println!("Muru-Preview: {}", preview.header);
        return Ok(());
    }

    if args.list {
        for link in &preview.links {
            println!("{}  {}", link.url, link.address);
        }

        return Ok(());
    }

    if args.print {
        if let Some(link) = preview.links.first() {
            println!("{}", link.url);
        }

        return Ok(());
    }

    open_link(&preview, &name);

    // The browser is given a moment to start before muructl exits.

    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(())
}

/// `--open-preview`: an entry link for the detour just opened, in the
/// browser.
pub async fn open_preview(daemon: &Path, opened: &pb::DetourOpened) -> anyhow::Result<()> {
    let Some(detour) = &opened.detour else {
        return Ok(());
    };

    let mut client =
        muru_controller::client::connect(&muru_controller::client::Endpoint::unix(daemon)).await?;
    let preview = mint(&mut client, detour, None, false).await?;
    open_link(&preview, &name(opened));
    Ok(())
}

/// Bind the detour's socket in the run's directory, before anything
/// starts, so the daemon can check it when the detour opens; connections
/// wait in its backlog until the edge takes them.
pub fn bind(directory: &Path) -> anyhow::Result<(std::os::unix::net::UnixListener, PathBuf)> {
    let socket = directory.join(SOCKET);
    let _ = std::fs::remove_file(&socket);
    let listener = std::os::unix::net::UnixListener::bind(&socket).map_err(|error| {
        anyhow::anyhow!(
            "cannot listen on the detour's socket {}: {error}; a shorter MURU_RUN_DIR may be \
             needed, since socket paths are limited to about a hundred bytes",
            socket.display()
        )
    })?;

    Ok((listener, socket))
}

/// What the run tells the daemon when it opens its detour.
pub fn request(
    mode: &Mode,
    socket: &Path,
    instance_id: &str,
    project: &Path,
) -> pb::OpenDetourRequest {
    pb::OpenDetourRequest {
        app: mode.app.clone(),
        socket: socket.display().to_string(),
        label: mode.label.clone(),
        instance_id: instance_id.to_owned(),
        pid: std::process::id(),
        directory: project.display().to_string(),
        traffic: mode.traffic.clone(),
    }
}

/// A detour's stream, as the daemon sends it.
pub type Stream = tonic::Streaming<pb::DetourMessage>;

/// Open the detour on the daemon: its stream, and what the daemon opened.
pub async fn open(
    daemon: &Path,
    request: &pb::OpenDetourRequest,
) -> anyhow::Result<(Stream, pb::DetourOpened)> {
    let mut client =
        muru_controller::client::connect(&muru_controller::client::Endpoint::unix(daemon))
            .await
            .with_context(|| format!("reaching the daemon at {}", daemon.display()))?;
    let mut stream = client
        .open_detour(request.clone())
        .await
        .map_err(|status| anyhow::anyhow!("the daemon refused the detour: {}", status.message()))?
        .into_inner();

    match stream.next().await {
        Some(Ok(pb::DetourMessage {
            what: Some(What::Opened(opened)),
        })) => Ok((stream, opened)),
        Some(Err(status)) => anyhow::bail!("the daemon refused the detour: {}", status.message()),
        _ => anyhow::bail!("the daemon ended the detour as it opened"),
    }
}

/// The detour, as the start-up and the toolbar name it: `shop@dev-7f3a`.
pub fn name(opened: &pb::DetourOpened) -> String {
    opened
        .detour
        .as_ref()
        .map(|detour| format!("{}@{}", detour.app, detour.release))
        .unwrap_or_default()
}

/// What a traffic choice means, as a line says it.
pub fn takes(traffic: &str, app: &str) -> String {
    match traffic {
        "" | "preview" => "previews only".to_owned(),
        "all" => format!("all of {app}'s traffic"),
        other => match other.split_once('=') {
            Some(("share", share)) => format!("{share}% of {app}'s clients"),
            Some(("class", class)) => format!("the requests of client class {class}"),
            _ => other.to_owned(),
        },
    }
}

/// Hold the detour open for as long as the run runs: what the daemon
/// sends goes to the toolbar and the terminal, and when the stream ends
/// with the run still running it is opened again every two seconds, as
/// a daemon that restarts has no detours. Aborting the task closes the
/// stream, which ends the detour.
pub fn hold(
    mut stream: Stream,
    daemon: PathBuf,
    request: pb::OpenDetourRequest,
    links: Arc<muru_edge::development::Links>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // What the daemon says while the detour is open.

            let mut why = "the daemon's stream ended".to_owned();

            while let Some(message) = stream.next().await {
                match message {
                    Ok(pb::DetourMessage {
                        what: Some(What::Changed(detour)),
                    }) => {
                        links.set_detour(Some(detour.traffic.clone()));
                        muru_common::muru_println!(
                            "detour {}@{} now takes {}{}",
                            detour.app,
                            detour.release,
                            takes(&detour.traffic, &detour.app),
                            ending(detour.until_unix_ms)
                        );
                    }
                    Ok(pb::DetourMessage {
                        what: Some(What::Closing(reason)),
                    }) => why = format!("the daemon ended it: {reason}"),
                    Ok(_) => {}
                    Err(status) => {
                        why = format!("its stream failed: {}", status.message());
                        break;
                    }
                }
            }

            muru_common::muru_println!(
                "the detour is closed, {why}; opening it again every {} seconds while this runs",
                RETRY_EVERY.as_secs()
            );

            // The run stays as it is meanwhile, and says each new reason
            // the daemon gives once.

            let mut said = String::new();

            stream = loop {
                tokio::time::sleep(RETRY_EVERY).await;

                match open(&daemon, &request).await {
                    Ok((stream, opened)) => {
                        links.set_hosts(opened.hosts.clone());
                        let traffic = opened
                            .detour
                            .as_ref()
                            .map(|detour| detour.traffic.clone())
                            .unwrap_or_default();
                        links.set_detour(Some(traffic));
                        muru_common::muru_println!("detour {} is open again", name(&opened));
                        break stream;
                    }
                    Err(error) => {
                        let text = format!("{error:#}");

                        if text != said {
                            muru_common::muru_println!("the detour is not open yet: {text}");
                            said = text;
                        }
                    }
                }
            };
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `muructl detour run` alone, for its parser.
    #[derive(Parser)]
    struct DetourOnly {
        #[command(flatten)]
        args: DetourRunArgs,
    }

    #[test]
    fn a_detours_flags_become_the_runs_it_serves_with() {
        let args = DetourOnly::try_parse_from([
            "detour",
            "shop",
            "--processes",
            "2",
            "--threads",
            "4",
            "--single",
            "--build",
            "--max-body-size",
            "50MB",
            "--no-reload",
            "--reload-exclude",
            "tests/**",
            "--toolbar",
            "--env-file",
            ".env.dev",
            "--traffic",
            "all",
        ])
        .expect("parsed")
        .args;

        assert_eq!(args.app.as_deref(), Some("shop"));
        assert_eq!(args.traffic, "all");

        let run = RunOnly::try_parse_from(args.run_words())
            .expect("the run's words parse")
            .args;

        assert_eq!(run.processes, Some(2));
        assert_eq!(run.threads, Some(4));
        assert!(run.single);
        assert_eq!(run.build.as_deref(), Some("always"));
        assert_eq!(run.max_body_size, Some(50 * 1024 * 1024));
        assert!(run.no_reload);
        assert_eq!(run.reload_exclude, ["tests/**"]);
        assert!(run.toolbar);
        assert_eq!(run.env_file.as_deref(), Some(Path::new(".env.dev")));

        // The entrypoint is the project's, never the application's name.

        assert!(run.app.is_none());
    }

    #[test]
    fn a_detour_has_no_listener_flags() {
        for flag in [
            "--bind",
            "--port",
            "--allow-public",
            "--open",
            "--https",
            "--host",
        ] {
            assert!(
                DetourOnly::try_parse_from(["detour", flag, "x"]).is_err(),
                "{flag} was taken"
            );
        }
    }

    #[test]
    fn the_list_lines_up_each_detour_and_shows_no_secret() {
        let detour = pb::Detour {
            app: "shop".to_owned(),
            release: "dev-7f3a".to_owned(),
            label: "feature-x".to_owned(),
            pid: 41872,
            traffic: "preview".to_owned(),
            opened_unix_ms: i64::try_from(crate::admin::now_unix_ms()).unwrap() - 720_000,
            directory: "/srv/shop-feature-x".to_owned(),
            ..Default::default()
        };
        let other = pb::Detour {
            app: "blog".to_owned(),
            release: "dev-1b2c".to_owned(),
            pid: 7,
            traffic: "all".to_owned(),
            opened_unix_ms: i64::try_from(crate::admin::now_unix_ms()).unwrap() - 5_000,
            directory: "/srv/blog".to_owned(),
            ..Default::default()
        };

        assert_eq!(
            table(&[detour, other]),
            [
                "APP   RELEASE   LABEL      PID    TRAFFIC  SINCE  DIRECTORY",
                "shop  dev-7f3a  feature-x  41872  preview  12m    /srv/shop-feature-x",
                "blog  dev-1b2c  -          7      all      5s     /srv/blog",
            ]
        );
    }

    #[test]
    fn a_traffic_choice_is_said_as_what_it_sends() {
        assert_eq!(takes("preview", "shop"), "previews only");
        assert_eq!(takes("all", "shop"), "all of shop's traffic");
        assert_eq!(takes("share=10", "shop"), "10% of shop's clients");
        assert_eq!(
            takes("class=crawlers", "shop"),
            "the requests of client class crawlers"
        );
    }
}
