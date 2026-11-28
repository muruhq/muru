//! `muructl service`: the services the daemon keeps running (design
//! specification section 9.17), listed, shown, started, stopped,
//! restarted and signalled, and their logs.

use std::time::{Duration, Instant, UNIX_EPOCH};

use clap::{Args, Subcommand};
use muru_controller::client;
use muru_proto::admin::v1 as pb;
use tokio_stream::StreamExt;

use crate::admin::{JsonArgs, SocketArgs, explain, now_unix_ms, print_answer};
use crate::instances::uptime;

/// How often a command that waits looks at the service again.
const POLL: Duration = Duration::from_millis(200);

/// How long a command waits for a service when no `--timeout` says.
const WAIT: Duration = Duration::from_secs(120);

/// The services a command is for.
#[derive(Args, Clone)]
pub struct Named {
    /// The services.
    #[arg(value_name = "NAME", required_unless_present = "all")]
    names: Vec<String>,

    /// Every service.
    #[arg(long, conflicts_with = "names")]
    all: bool,
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// List the services, with what runs of each.
    List {
        /// Only the services in these states, separated by commas:
        /// stopped, starting, running, backoff, stopping, exited or
        /// failed.
        #[arg(long, value_name = "STATES", value_delimiter = ',')]
        state: Vec<String>,

        /// Print nothing, and exit 1 when any service is listed: for a
        /// check that no service is in the states named.
        #[arg(long)]
        quiet: bool,

        /// Only what this deployment owns.
        #[arg(long, value_name = "NAME")]
        deployment: Option<String>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Show services: their definitions, each instance and its last
    /// exits. Exits 0 when every one is running, 3 when one is not and 4
    /// when one does not exist.
    Status {
        #[command(flatten)]
        named: Named,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Start services that are stopped, exited or failed, and start now
    /// an instance that waits to be started again.
    Start {
        #[command(flatten)]
        named: Named,

        /// One instance, by its index.
        #[arg(long, value_name = "N")]
        index: Option<u32>,

        /// Return once every instance is running (exit 0), the service
        /// has exited or failed (exit 1), or the timeout has passed
        /// (exit 2).
        #[arg(long)]
        wait: bool,

        /// How long --wait waits (default 2m).
        #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration, requires = "wait")]
        timeout: Option<Duration>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Stop services. A service stopped whole stays stopped, through
    /// restarts of the daemon, until it is started by hand.
    Stop {
        #[command(flatten)]
        named: Named,

        /// One instance, by its index.
        #[arg(long, value_name = "N")]
        index: Option<u32>,

        /// How long each instance has to stop before it is killed, in
        /// place of the service's stop_timeout.
        #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration, conflicts_with = "kill")]
        timeout: Option<Duration>,

        /// Kill the instances at once, without the stop signal.
        #[arg(long)]
        kill: bool,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Stop and start services, with no restart counted. A stopped or
    /// failed service is started.
    Restart {
        #[command(flatten)]
        named: Named,

        /// One instance, by its index.
        #[arg(long, value_name = "N")]
        index: Option<u32>,

        /// One instance at a time, each ready before the next is stopped.
        #[arg(long, conflicts_with = "index")]
        rolling: bool,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Print a service's log: the output of its instances.
    Logs {
        /// The service.
        #[arg(value_name = "NAME")]
        name: String,

        /// Keep printing lines as they are written.
        #[arg(long, short = 'f')]
        follow: bool,

        /// How many of the last lines to print first.
        #[arg(long, short = 'n', value_name = "N", default_value_t = 100)]
        lines: u32,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Send a signal to a service's instances: TERM, INT, QUIT, HUP,
    /// USR1, USR2 or WINCH, or `reload` for the service's reload_signal.
    Signal {
        /// The service.
        #[arg(value_name = "NAME")]
        name: String,

        /// The signal, or `reload`.
        #[arg(value_name = "SIGNAL")]
        signal: String,

        /// One instance, by its index.
        #[arg(long, value_name = "N")]
        index: Option<u32>,

        #[command(flatten)]
        socket: SocketArgs,

        #[command(flatten)]
        output: JsonArgs,
    },
}

impl ServiceCommand {
    /// What the command changes, in words, when it changes something.
    pub fn change(&self) -> Option<String> {
        let named = |named: &Named| match named.all {
            true => "every service".to_owned(),
            false => named.names.join(", "),
        };

        match self {
            Self::List { .. } | Self::Status { .. } | Self::Logs { .. } => None,
            Self::Start { named: of, .. } => Some(format!("starting {}", named(of))),
            Self::Stop { named: of, .. } => Some(format!("stopping {}", named(of))),
            Self::Restart { named: of, .. } => Some(format!("restarting {}", named(of))),
            Self::Signal { name, .. } => Some(format!("signalling {name}")),
        }
    }
}

type Client = pb::admin_client::AdminClient<tonic::transport::Channel>;

/// The services asked for, as the daemon has them.
async fn listed(client: &mut Client, names: &[String]) -> anyhow::Result<pb::ListServicesResponse> {
    Ok(client
        .list_services(pb::ListServicesRequest {
            names: names.to_vec(),
        })
        .await
        .map_err(explain)?
        .into_inner())
}

/// The names a command is for: those given, or every service's.
async fn names_of(client: &mut Client, named: &Named) -> anyhow::Result<Vec<String>> {
    if !named.all {
        return Ok(named.names.clone());
    }

    Ok(listed(client, &[])
        .await?
        .services
        .into_iter()
        .map(|service| service.name)
        .collect())
}

/// One service as the daemon has it now.
async fn one(client: &mut Client, name: &str) -> anyhow::Result<pb::ServiceStatus> {
    listed(client, &[name.to_owned()])
        .await?
        .services
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("there is no service named {name}"))
}

/// Do something to a service, or one instance of it.
async fn act(
    client: &mut Client,
    request: pb::ActOnServiceRequest,
) -> anyhow::Result<pb::ActOnServiceResponse> {
    Ok(client
        .act_on_service(request)
        .await
        .map_err(explain)?
        .into_inner())
}

/// The pid of a service of one instance, in brackets, for a line about it.
fn pid_note(service: &pb::ServiceStatus) -> String {
    match service.instance_list.as_slice() {
        [only] if only.pid != 0 => format!(" (pid {})", only.pid),
        _ => String::new(),
    }
}

/// A time as `service status` shows it: to the second, in UTC.
fn at(unix_ms: u64) -> String {
    let time = UNIX_EPOCH + Duration::from_millis(unix_ms);
    let full = muru_common::log::full(time);
    format!("{}Z", &full[..19])
}

/// The time of day of an exit, in UTC.
fn clock(unix_ms: u64) -> String {
    let time = UNIX_EPOCH + Duration::from_millis(unix_ms);
    muru_common::log::full(time)[11..19].to_owned()
}

/// Where an instance stands, for `service status`.
fn standing(instance: &pb::ServiceInstance) -> String {
    let in_a_row = match instance.quick_exits {
        0 | 1 => String::new(),
        count => format!(" after {count} quick exits in a row"),
    };

    match instance.state.as_str() {
        "running" | "starting" | "stopping" => format!(
            "{}, pid {}, up {}",
            instance.state,
            instance.pid,
            uptime(instance.started_unix_ms)
        ),
        "backoff" => {
            let wait = Duration::from_secs(
                instance
                    .next_start_unix_ms
                    .saturating_sub(now_unix_ms())
                    .div_ceil(1000),
            );
            format!(
                "backoff, starting again in {}{in_a_row}",
                muru_config::format_duration(wait)
            )
        }
        "failed" => format!("failed, not started again{in_a_row}"),
        "exited" => "exited, not started again".to_owned(),
        other => other.to_owned(),
    }
}

/// `service list`.
pub async fn list(
    socket: SocketArgs,
    json: bool,
    states: Vec<String>,
    quiet: bool,
    deployment: Option<String>,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let mut answer = listed(&mut client, &[]).await?;
    answer.services.retain(|service| {
        (states.is_empty() || states.contains(&service.state))
            && deployment
                .as_deref()
                .is_none_or(|deployment| service.deployment == deployment)
    });

    if quiet {
        std::process::exit(i32::from(!answer.services.is_empty()));
    }

    if json {
        return print_answer(&answer);
    }

    if answer.services.is_empty() {
        println!("no services");
        return Ok(());
    }

    let width = answer
        .services
        .iter()
        .map(|service| service.name.len())
        .max()
        .unwrap_or(0)
        .max("SERVICE".len());
    println!(
        "{:<width$}  {:<9} {:<5} {:<7} {:<9} {:<9} COMMAND",
        "SERVICE", "STATE", "UP", "PID", "SINCE", "RESTARTS"
    );

    for service in &answer.services {
        let pid = match service.instance_list.as_slice() {
            [only] if only.pid != 0 => only.pid.to_string(),
            _ => "-".to_owned(),
        };

        // A service that never ran has been stopped since nothing.

        let since = match service.state.as_str() {
            "stopped" if service.restarts == 0 && !service.held => "-".to_owned(),
            _ => uptime(service.since_unix_ms),
        };

        println!(
            "{:<width$}  {:<9} {:<5} {:<7} {:<9} {:<9} {}",
            service.name,
            service.state,
            format!("{}/{}", service.running, service.instances),
            pid,
            since,
            service.restarts,
            service.command.join(" ")
        );
    }

    Ok(())
}

/// The lines of `service status` for one service.
fn status_lines(service: &pb::ServiceStatus) -> Vec<String> {
    let mut rows: Vec<(String, String)> = vec![("service".to_owned(), service.name.clone())];

    if !service.description.is_empty() {
        rows.push(("description".to_owned(), service.description.clone()));
    }

    let state = match (service.state.as_str(), service.held, service.autostart) {
        ("stopped", true, _) => "stopped by hand; muructl service start starts it".to_owned(),
        ("stopped", false, false) => "stopped: not started automatically".to_owned(),
        (state, _, _) => format!(
            "{state} since {} ({})",
            at(service.since_unix_ms),
            uptime(service.since_unix_ms)
        ),
    };

    rows.extend([
        ("state".to_owned(), state),
        ("command".to_owned(), service.command.join(" ")),
        ("cwd".to_owned(), service.cwd.clone()),
    ]);

    if !service.user.is_empty() {
        rows.push(("user".to_owned(), service.user.clone()));
    }

    rows.extend([
        (
            "autostart".to_owned(),
            if service.autostart { "yes" } else { "no" }.to_owned(),
        ),
        ("restart".to_owned(), service.restart.clone()),
        ("readiness".to_owned(), service.readiness.clone()),
        (
            "stop".to_owned(),
            format!(
                "{}, killed after {}",
                service.stop_signal,
                muru_config::format_duration(Duration::from_millis(service.stop_timeout_ms))
            ),
        ),
        ("log".to_owned(), service.log.clone()),
    ]);

    for instance in &service.instance_list {
        rows.push((format!("instance {}", instance.index), standing(instance)));
    }

    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let mut lines: Vec<String> = rows
        .into_iter()
        .map(|(label, value)| format!("{label:<width$}  {value}"))
        .collect();

    for instance in &service.instance_list {
        if instance.exits.is_empty() {
            continue;
        }

        lines.push(String::new());
        lines.push(format!("last exits of instance {}", instance.index));

        for exit in &instance.exits {
            lines.push(format!("  {}  {}", clock(exit.unix_ms), exit.message));
        }
    }

    lines
}

/// `service status`: each service named, and an exit status that says
/// the worst of them.
pub async fn status(named: Named, socket: SocketArgs, json: bool) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let names = names_of(&mut client, &named).await?;
    let answer = listed(&mut client, &names).await?;

    let code = if !answer.missing.is_empty() {
        4
    } else if answer
        .services
        .iter()
        .any(|service| service.state != "running")
    {
        3
    } else {
        0
    };

    if json {
        print_answer(&answer)?;
        std::process::exit(code);
    }

    for (place, service) in answer.services.iter().enumerate() {
        if place > 0 {
            println!();
        }

        for line in status_lines(service) {
            println!("{line}");
        }
    }

    for name in &answer.missing {
        eprintln!("muru: there is no service named {name}");
    }

    std::process::exit(code)
}

/// Wait until a service is as `settled` wants it, or the time is up.
async fn wait_for(
    client: &mut Client,
    name: &str,
    within: Duration,
    settled: impl Fn(&pb::ServiceStatus) -> bool,
) -> anyhow::Result<Option<pb::ServiceStatus>> {
    let deadline = Instant::now() + within;

    loop {
        let service = one(client, name).await?;

        if settled(&service) {
            return Ok(Some(service));
        }

        if Instant::now() >= deadline {
            return Ok(None);
        }

        tokio::time::sleep(POLL).await;
    }
}

/// `service start`.
pub async fn start(
    named: Named,
    index: Option<u32>,
    wait: bool,
    timeout: Option<Duration>,
    socket: SocketArgs,
    json: bool,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let mut answers = Vec::new();
    let mut code = 0;

    for name in names_of(&mut client, &named).await? {
        // With --all, a service that is not started automatically is
        // left: it runs only when it is named.

        if named.all && !one(&mut client, &name).await?.autostart {
            continue;
        }

        let answer = act(
            &mut client,
            pb::ActOnServiceRequest {
                name: name.clone(),
                index,
                action: "start".to_owned(),
                ..pb::ActOnServiceRequest::default()
            },
        )
        .await?;

        if !json {
            let service = answer.service.clone().unwrap_or_default();

            match (answer.instances.as_slice(), index) {
                ([], _) => println!("muru: {name} is already running{}", pid_note(&service)),
                (_, Some(index)) => println!("muru: {name} instance {index} started"),
                (_, None) => println!("muru: {name} started{}", pid_note(&service)),
            }
        }

        if wait {
            let settled = wait_for(&mut client, &name, timeout.unwrap_or(WAIT), |service| {
                matches!(service.state.as_str(), "running" | "failed" | "exited")
            })
            .await?;

            match settled {
                Some(service) if service.state == "running" => {
                    if !json {
                        println!("muru: {name} is running{}", pid_note(&service));
                    }
                }
                Some(service) => {
                    eprintln!("muru: {name} is {}", service.state);
                    code = code.max(1);
                }
                None => {
                    eprintln!("muru: {name} was not running in time");
                    code = code.max(2);
                }
            }
        }

        answers.push(answer);
    }

    if json {
        print_answer(&serde_json::json!({ "services": answers }))?;
    }

    if code != 0 {
        std::process::exit(code);
    }

    Ok(())
}

/// `service stop`: returns once the processes are gone.
pub async fn stop(
    named: Named,
    index: Option<u32>,
    timeout: Option<Duration>,
    kill: bool,
    socket: SocketArgs,
    json: bool,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let mut answers = Vec::new();

    for name in names_of(&mut client, &named).await? {
        let answer = act(
            &mut client,
            pb::ActOnServiceRequest {
                name: name.clone(),
                index,
                action: "stop".to_owned(),
                timeout_ms: timeout.map(|timeout| timeout.as_millis() as u64),
                kill,
                ..pb::ActOnServiceRequest::default()
            },
        )
        .await?;

        // The processes have the time they were given, and a little
        // more for the daemon to see them gone.

        let stop_timeout = answer.service.as_ref().map_or(Duration::ZERO, |service| {
            Duration::from_millis(service.stop_timeout_ms)
        });

        let within = match kill {
            true => Duration::ZERO,
            false => timeout.unwrap_or(stop_timeout),
        } + Duration::from_secs(10);

        let gone = |service: &pb::ServiceStatus| {
            service
                .instance_list
                .iter()
                .filter(|instance| index.is_none_or(|index| instance.index == index))
                .all(|instance| instance.pid == 0)
        };

        let Some(service) = wait_for(&mut client, &name, within, gone).await? else {
            anyhow::bail!(
                "{name} had not stopped in time; muructl service status {name} says where it stands"
            );
        };

        if !json {
            match index {
                Some(index) => {
                    let others = service
                        .instance_list
                        .iter()
                        .filter(|other| other.pid != 0)
                        .count();
                    println!(
                        "muru: {name} instance {index} stopped; the other {others} keep{} running",
                        if others == 1 { "s" } else { "" }
                    );
                }
                None if kill => println!("muru: {name} killed"),
                None => println!("muru: {name} stopped"),
            }
        }

        answers.push(answer);
    }

    if json {
        print_answer(&serde_json::json!({ "services": answers }))?;
    }

    Ok(())
}

/// `service restart`: returns once every instance restarted has a new
/// process.
pub async fn restart(
    named: Named,
    index: Option<u32>,
    rolling: bool,
    socket: SocketArgs,
    json: bool,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let mut answers = Vec::new();

    for name in names_of(&mut client, &named).await? {
        let before = one(&mut client, &name).await?;
        let answer = act(
            &mut client,
            pb::ActOnServiceRequest {
                name: name.clone(),
                index,
                action: "restart".to_owned(),
                rolling,
                ..pb::ActOnServiceRequest::default()
            },
        )
        .await?;
        let touched: Vec<u32> = answer
            .instances
            .iter()
            .map(|instance| instance.index)
            .collect();

        // Each has a new process once the restart is done: the stops
        // have their time, and with --rolling each start its readiness.

        let within =
            Duration::from_millis(before.stop_timeout_ms) * (touched.len().max(1) as u32) + WAIT;

        let replaced = |service: &pb::ServiceStatus| {
            service
                .instance_list
                .iter()
                .filter(|instance| touched.contains(&instance.index))
                .all(|instance| {
                    let old = before
                        .instance_list
                        .iter()
                        .find(|old| old.index == instance.index)
                        .map_or(0, |old| old.pid);
                    instance.pid != 0
                        && instance.pid != old
                        && (!rolling || instance.state == "running")
                })
        };

        let Some(service) = wait_for(&mut client, &name, within, replaced).await? else {
            anyhow::bail!(
                "{name} had not restarted in time; muructl service status {name} says where it stands"
            );
        };

        if !json {
            match index {
                Some(index) => println!("muru: {name} instance {index} restarted"),
                None => println!("muru: {name} restarted{}", pid_note(&service)),
            }
        }

        answers.push(answer);
    }

    if json {
        print_answer(&serde_json::json!({ "services": answers }))?;
    }

    Ok(())
}

/// `service signal`.
pub async fn signal(
    name: String,
    signal: String,
    index: Option<u32>,
    socket: SocketArgs,
    json: bool,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let answer = act(
        &mut client,
        pb::ActOnServiceRequest {
            name: name.clone(),
            index,
            action: "signal".to_owned(),
            signal: signal.clone(),
            ..pb::ActOnServiceRequest::default()
        },
    )
    .await?;

    if json {
        return print_answer(&answer);
    }

    if answer.instances.is_empty() {
        anyhow::bail!("{name} has no running instance to signal");
    }

    let sent = match signal.as_str() {
        "reload" => "its reload signal".to_owned(),
        named => named.to_owned(),
    };

    let whole = answer
        .service
        .as_ref()
        .is_some_and(|service| service.instances == 1);

    for instance in &answer.instances {
        match whole {
            true => println!("muru: sent {sent} to {name} (pid {})", instance.pid),
            false => println!(
                "muru: sent {sent} to {name} instance {} (pid {})",
                instance.index, instance.pid
            ),
        }
    }

    Ok(())
}

/// `service logs`: the last lines of a service's log, and with
/// `--follow` those written from then on.
pub async fn logs(
    name: String,
    follow: bool,
    lines: u32,
    socket: SocketArgs,
    json: bool,
) -> anyhow::Result<()> {
    let mut client = client::connect(&socket.endpoint()?).await?;
    let mut stream = client
        .tail_logs(pb::TailLogsRequest {
            service: name.clone(),
            follow,
            lines,
            ..pb::TailLogsRequest::default()
        })
        .await
        .map_err(explain)?
        .into_inner();

    while let Some(line) = stream.next().await {
        let line = line.map_err(explain)?.line;

        if json {
            println!("{}", serde_json::json!({ "service": name, "line": line }));
        } else {
            println!("{line}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_reads_as_the_guide_shows_it() {
        let now = now_unix_ms();
        let service = pb::ServiceStatus {
            name: "thumbnailer".to_owned(),
            state: "backoff".to_owned(),
            since_unix_ms: 1_794_000_000_000,
            autostart: true,
            instances: 2,
            running: 1,
            command: vec!["thumbnailer".to_owned(), "--listen".to_owned()],
            cwd: "/var/lib/muru/services/thumbnailer".to_owned(),
            restart: "always, 1s to 60s, stable after 60s".to_owned(),
            readiness: "after 1s".to_owned(),
            stop_signal: "INT".to_owned(),
            stop_timeout_ms: 10_000,
            log: "/var/log/muru/services/thumbnailer.log".to_owned(),
            instance_list: vec![
                pb::ServiceInstance {
                    index: 0,
                    pid: 61002,
                    state: "running".to_owned(),
                    started_unix_ms: now - 3_600_000,
                    ..Default::default()
                },
                pb::ServiceInstance {
                    index: 1,
                    state: "backoff".to_owned(),
                    next_start_unix_ms: now + 21_500,
                    quick_exits: 7,
                    exits: vec![pb::ServiceExit {
                        unix_ms: 1_794_000_000_000,
                        message: "exited with exit status: 1 after 400ms".to_owned(),
                    }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let lines = status_lines(&service);

        assert_eq!(lines[0], "service     thumbnailer");
        assert!(
            lines[1].starts_with("state       backoff since 2026-11-"),
            "{lines:?}"
        );
        assert!(lines.contains(&"stop        INT, killed after 10s".to_owned()));
        assert!(lines.contains(&"instance 0  running, pid 61002, up 1h 0m".to_owned()));
        assert!(
            lines.iter().any(|line| line.starts_with(
                "instance 1  backoff, starting again in 22s after 7 quick exits in a row"
            )),
            "{lines:?}"
        );
        assert_eq!(lines[lines.len() - 2], "last exits of instance 1");
        assert!(lines[lines.len() - 1].ends_with("  exited with exit status: 1 after 400ms"));

        // A service that was never started, and one stopped by hand.

        let idle = pb::ServiceStatus {
            state: "stopped".to_owned(),
            ..Default::default()
        };
        assert!(
            status_lines(&idle)
                .contains(&"state      stopped: not started automatically".to_owned())
        );
        let held = pb::ServiceStatus {
            state: "stopped".to_owned(),
            held: true,
            autostart: true,
            ..Default::default()
        };
        assert!(
            status_lines(&held).contains(
                &"state      stopped by hand; muructl service start starts it".to_owned()
            )
        );
    }
}
