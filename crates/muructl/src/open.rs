//! `muructl open` (design specification sections 10.7 and 13.1, issue
//! 815): open the current project's application, an application on a
//! daemon, or an instance's dashboard in the browser on this machine.
//!
//! A development run's or a serve's URL comes from its registry record,
//! with no admin call; an application on a daemon is asked of the daemon
//! with ApplicationUrls, which works it out from the routing. The names a
//! daemon's virtual hosts answer to may be another machine's, so for an
//! instance on this machine each candidate's name is checked to reach it.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs as _};
use std::path::Path;

use clap::Args;
use muru_controller::client;
use muru_controller::registry::{self, Kind, Record};
use muru_proto::admin::v1 as pb;
use serde::Serialize;

/// What `muructl open` takes.
#[derive(Args)]
pub struct OpenArgs {
    /// The application on a daemon (default: the project's, or the
    /// development run of this directory), or a path under the
    /// application's URL when it starts with /.
    #[arg(value_name = "APP")]
    first: Option<String>,

    /// A path under the application's URL, such as /admin/.
    #[arg(value_name = "PATH")]
    path: Option<String>,

    /// Open the instance's dashboard, signed in where the dashboard needs
    /// a session.
    #[arg(long)]
    dashboard: bool,

    /// With --dashboard, a development run's page for one request.
    #[arg(long, value_name = "ID", requires = "dashboard")]
    request: Option<String>,

    /// Print the URL and open nothing.
    #[arg(long)]
    print: bool,

    /// Print every URL the application can be reached at, best first, and
    /// open nothing.
    #[arg(long, conflicts_with_all = ["print", "dashboard"])]
    list: bool,

    /// With --print or --list, print the URLs as JSON, with how each was
    /// chosen.
    #[arg(long)]
    json: bool,

    /// Only https URLs.
    #[arg(long, conflicts_with = "http")]
    https: bool,

    /// Only http URLs.
    #[arg(long)]
    http: bool,

    /// On a daemon, only the route of this name.
    #[arg(long, value_name = "NAME")]
    route: Option<String>,

    /// On a daemon, only the virtual host of this name.
    #[arg(long = "host", value_name = "NAME")]
    host: Option<String>,

    /// Only the listener at this address.
    #[arg(long, value_name = "ADDRESS")]
    listener: Option<String>,

    /// Keep names that resolve to another machine.
    #[arg(long)]
    any_name: bool,

    #[command(flatten)]
    socket: crate::admin::SocketArgs,
}

impl OpenArgs {
    /// The application named, when the first argument is one rather than
    /// a path.
    pub fn app(&self) -> Option<&str> {
        self.first
            .as_deref()
            .filter(|first| !first.starts_with('/'))
    }

    /// The path under the application's URL.
    fn under(&self) -> Option<&str> {
        self.path
            .as_deref()
            .or_else(|| self.first.as_deref().filter(|first| first.starts_with('/')))
    }
}

/// One URL the application can be reached at, and how it was chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Choice {
    /// The URL.
    pub url: String,
    /// `http` or `https`.
    pub scheme: String,
    /// The name in it.
    pub name: String,
    /// The listener's address, as bound.
    pub bound: String,
    /// The route's name, on a daemon.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub route: String,
    /// The virtual host, on a daemon.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// How it was chosen.
    pub why: String,
}

/// A run's or a serve's URLs from its record: https first when it has a
/// TLS listener, each under its `--host` names and then `localhost`, or
/// the address, for a loopback or wildcard bind.
pub fn record_choices(record: &Record) -> Vec<Choice> {
    let mut listeners: Vec<&registry::Listener> = record.listeners.iter().collect();
    listeners.sort_by_key(|listener| !listener.tls);
    let mut choices = Vec::new();

    for listener in listeners {
        let Some(bound) = listener
            .bound
            .as_deref()
            .and_then(|bound| bound.parse::<SocketAddr>().ok())
        else {
            continue;
        };

        let scheme = if listener.tls { "https" } else { "http" };
        let mut names: Vec<(String, String)> = record
            .names
            .iter()
            .map(|name| (name.clone(), format!("the run's --host name {name}")))
            .collect();
        names.push((
            muru_controller::urls::address_name(bound),
            format!("the {} listener {bound}", listener.name),
        ));

        for (name, why) in names {
            choices.push(Choice {
                url: muru_controller::urls::url(scheme, &name, bound.port(), "/"),
                scheme: scheme.to_owned(),
                name,
                bound: bound.to_string(),
                route: String::new(),
                host: String::new(),
                why,
            });
        }
    }

    choices
}

/// A daemon's answer as choices.
fn daemon_choices(answer: pb::ApplicationUrlsResponse) -> Vec<Choice> {
    answer
        .urls
        .into_iter()
        .map(|url| Choice {
            url: url.url,
            scheme: url.scheme,
            name: url.name,
            bound: url.bound,
            route: url.route,
            host: url.host,
            why: url.why,
        })
        .collect()
}

/// Why a name does not reach the listener on this machine, or none when
/// it does: a `.localhost` name, `localhost` and an address always do; any
/// other must resolve to an address the listener is bound on, or to a
/// loopback address for a wildcard bind.
pub fn elsewhere(
    name: &str,
    bound: SocketAddr,
    resolve: &dyn Fn(&str) -> Vec<IpAddr>,
) -> Option<String> {
    let bare = name.trim_start_matches('[').trim_end_matches(']');

    if name == "localhost" || name.ends_with(".localhost") || bare.parse::<IpAddr>().is_ok() {
        return None;
    }

    let found = resolve(name);

    if found.is_empty() {
        return Some(format!("{name} does not resolve"));
    }

    let reaches = found
        .iter()
        .any(|ip| *ip == bound.ip() || (bound.ip().is_unspecified() && ip.is_loopback()));

    (!reaches).then(|| {
        let shown: Vec<String> = found.iter().map(ToString::to_string).collect();
        format!("{name} resolves to {}, not this listener", shown.join(", "))
    })
}

/// The addresses a name resolves to.
fn resolve(name: &str) -> Vec<IpAddr> {
    (name, 0)
        .to_socket_addrs()
        .map(|found| found.map(|address| address.ip()).collect())
        .unwrap_or_default()
}

/// The choices kept for this machine, with the names left out and why;
/// `.localhost` names first.
pub fn reachable(
    choices: Vec<Choice>,
    any_name: bool,
    resolve: &dyn Fn(&str) -> Vec<IpAddr>,
) -> (Vec<Choice>, Vec<String>) {
    let mut kept = Vec::new();
    let mut left = Vec::new();

    for choice in choices {
        let why = match choice.bound.parse::<SocketAddr>() {
            Ok(bound) if !any_name => elsewhere(&choice.name, bound, resolve),
            _ => None,
        };

        match why {
            Some(why) => {
                if !left.contains(&why) {
                    left.push(why);
                }
            }
            None => kept.push(choice),
        }
    }

    kept.sort_by_key(|choice| !choice.name.ends_with(".localhost"));
    (kept, left)
}

/// `text` as a query string value: letters, digits, `-`, `_`, `.` and `/`
/// as they are, every other byte as `%XX`.
fn query_value(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => {
                char::from(byte).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// `base` with `under` after it, one slash between.
fn joined(base: &str, under: Option<&str>) -> String {
    match under {
        Some(under) => format!(
            "{}/{}",
            base.trim_end_matches('/'),
            under.trim_start_matches('/')
        ),
        None => base.to_owned(),
    }
}

/// The application a project's directory deploys as: its `[project]`
/// name, else the top-level module of its `[tool.muru]` entrypoint.
fn project_app(directory: &Path) -> Option<String> {
    let text = std::fs::read_to_string(directory.join("pyproject.toml")).ok();
    let name = text.as_deref().and_then(|text| {
        let file: toml::Table = text.parse().ok()?;
        let name = file.get("project")?.get("name")?.as_str()?;
        Some(name.to_ascii_lowercase().replace(['_', '.'], "-"))
    });

    if let Some(name) = name.filter(|name| muru_config::valid_name(name)) {
        return Some(name);
    }

    let table = match text {
        Some(text) => muru_config::parse_pyproject(&text).ok().flatten(),
        None => std::fs::read_to_string(directory.join("muru.toml"))
            .ok()
            .and_then(|text| muru_config::parse_muru_toml(&text).ok()),
    }?;
    let entrypoint = table.entrypoint?;
    let top = entrypoint.split([':', '.']).next()?.replace('_', "-");
    muru_config::valid_name(&top).then_some(top)
}

/// The live registry record reached at `endpoint`, when one is.
fn record_at(endpoint: &client::Endpoint) -> Option<Record> {
    registry::readable_registries()
        .iter()
        .flat_map(|registry| registry::live(registry))
        .find(|record| record.admin == *endpoint)
}

/// What `--print`, `--list` and `--json` show.
#[derive(Serialize)]
struct Printed<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    app: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instance: Option<&'a str>,
    url: &'a str,
    urls: &'a [Choice],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    left_out: &'a [String],
}

/// `muructl open`.
pub async fn open(args: OpenArgs) -> anyhow::Result<()> {
    let endpoint = args.socket.endpoint()?;
    let record = record_at(&endpoint).filter(|record| record.kind != Kind::Daemon);

    // The dashboard: a run's is open to whoever may reach it, a daemon's
    // is signed in to with a single-use link.

    if args.dashboard {
        let url = match &record {
            Some(record) => {
                let dashboard = record.dashboard.clone().ok_or_else(|| {
                    anyhow::anyhow!("the {} serves no dashboard", describe(record))
                })?;
                joined(
                    &dashboard,
                    args.request
                        .as_deref()
                        .map(|id| format!("request/{id}"))
                        .as_deref(),
                )
            }
            None => {
                let mut client = client::connect(&endpoint).await?;
                let link = client
                    .dashboard_login(pb::DashboardLoginRequest::default())
                    .await
                    .map_err(|status| anyhow::anyhow!("{}", status.message()))?
                    .into_inner();

                match &args.request {
                    Some(id) => format!(
                        "{}&next={}",
                        link.url,
                        query_value(&format!("request/{id}"))
                    ),
                    None => link.url,
                }
            }
        };

        return finish(&args, &url, &[], &[], None);
    }

    // The application: a run's from its record, a daemon's from its
    // routing.

    let (choices, app) = match &record {
        Some(record) => {
            if let Some(app) = args.app()
                && record.app.as_deref() != Some(app)
            {
                anyhow::bail!(
                    "this reaches the {}; muructl open {app} opens an application on a daemon, \
                     so name it with --instance",
                    describe(record)
                );
            }

            (record_choices(record), record.app.clone())
        }
        None => {
            let app = match args.app() {
                Some(app) => app.to_owned(),
                None => project_app(&std::env::current_dir()?).ok_or_else(|| {
                    anyhow::anyhow!(
                        "this directory is not a project muructl can name; name the application: \
                         muructl open APP"
                    )
                })?,
            };

            let mut client = client::connect(&endpoint).await?;
            let answer = client
                .application_urls(pb::ApplicationUrlsRequest { app: app.clone() })
                .await
                .map_err(|status| anyhow::anyhow!("{}", status.message()))?
                .into_inner();

            (daemon_choices(answer), Some(app))
        }
    };

    // The flags choose among them; the scheme is https when there is one.

    let mut choices: Vec<Choice> = choices
        .into_iter()
        .filter(|choice| {
            args.route
                .as_deref()
                .is_none_or(|route| choice.route == route)
        })
        .filter(|choice| args.host.as_deref().is_none_or(|host| choice.host == host))
        .filter(|choice| {
            args.listener
                .as_deref()
                .is_none_or(|listener| choice.bound == listener)
        })
        .collect();

    let wanted = if args.http {
        Some("http")
    } else if args.https {
        Some("https")
    } else {
        None
    };

    if let Some(scheme) = wanted {
        anyhow::ensure!(
            choices.iter().any(|choice| choice.scheme == scheme),
            "the application has no {scheme} listener to open"
        );
        choices.retain(|choice| choice.scheme == scheme);
    }

    let fallback = choices.first().cloned();
    let (choices, left_out) = reachable(choices, args.any_name, &resolve);

    let Some(first) = choices.first() else {
        let Some(fallback) = fallback else {
            anyhow::bail!(
                "{} has no address to open: no route on a listener reaches it",
                app.as_deref().unwrap_or("the application")
            );
        };

        let address: SocketAddr = fallback.bound.parse()?;

        let ip = if address.ip().is_unspecified() {
            "127.0.0.1".to_owned()
        } else {
            address.ip().to_string()
        };

        anyhow::bail!(
            "none of the application's names reaches this machine: {}; open {}://{ip}:{}/, which \
             reaches the listener's default host, or pass --any-name to keep them",
            left_out.join("; "),
            fallback.scheme,
            address.port()
        );
    };

    let url = joined(&first.url, args.under());
    finish(&args, &url, &choices, &left_out, app.as_deref())
}

/// Print, list or open the URL chosen.
fn finish(
    args: &OpenArgs,
    url: &str,
    choices: &[Choice],
    left_out: &[String],
    app: Option<&str>,
) -> anyhow::Result<()> {
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&Printed {
                app,
                instance: crate::admin::reached_instance(),
                url,
                urls: choices,
                left_out,
            })?
        );
        return Ok(());
    }

    if args.list {
        let mut table = vec![vec!["URL".to_owned(), "CHOSEN BY".to_owned()]];
        table.extend(
            choices
                .iter()
                .map(|choice| vec![joined(&choice.url, args.under()), choice.why.clone()]),
        );
        crate::instances::print_table(&table);

        for left in left_out {
            println!("left out: {left}");
        }

        return Ok(());
    }

    if args.print {
        println!("{url}");
        return Ok(());
    }

    crate::admin::require_local("open", crate::admin::reached_instance())?;
    crate::browser::open(url);

    match choices.len() {
        0 | 1 => {}
        2 => println!("muru: 1 other address: muructl open --list"),
        count => println!("muru: {} other addresses: muructl open --list", count - 1),
    }

    Ok(())
}

/// A run or serve, as a sentence names it.
fn describe(record: &Record) -> String {
    let app = record.app.as_deref().unwrap_or("an application");

    match record.kind {
        Kind::Serve => format!("muructl serve of {app}"),
        _ => format!("development run of {app}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(names: &[&str], listeners: &[(&str, bool)]) -> Record {
        let mut record = Record::new(
            registry::InstanceId::generate(),
            Kind::Run,
            client::Endpoint::unix("/tmp/admin.sock"),
            std::path::PathBuf::from("/tmp/run"),
        );
        record.names = names.iter().map(|name| (*name).to_owned()).collect();
        record.listeners = listeners
            .iter()
            .map(|(address, tls)| registry::Listener {
                name: if *tls { "https" } else { "http" }.to_owned(),
                configured: (*address).to_owned(),
                bound: Some((*address).to_owned()),
                tls: *tls,
            })
            .collect();
        record
    }

    #[test]
    fn a_runs_url_is_https_first_under_its_first_host_name() {
        let urls: Vec<String> = record_choices(&run(
            &["shop.localhost"],
            &[("127.0.0.1:8080", false), ("127.0.0.1:8443", true)],
        ))
        .into_iter()
        .map(|choice| choice.url)
        .collect();

        assert_eq!(
            urls,
            [
                "https://shop.localhost:8443/",
                "https://localhost:8443/",
                "http://shop.localhost:8080/",
                "http://localhost:8080/",
            ]
        );

        let ipv6 = record_choices(&run(&[], &[("[2001:db8::1]:8080", false)]));
        assert_eq!(ipv6[0].url, "http://[2001:db8::1]:8080/");
    }

    #[test]
    fn a_name_that_resolves_elsewhere_is_left_out_and_localhost_names_come_first() {
        let resolve = |name: &str| -> Vec<IpAddr> {
            match name {
                "shop.example.com" => vec!["93.184.215.14".parse().expect("ip")],
                "here.example.com" => vec!["127.0.0.1".parse().expect("ip")],
                _ => Vec::new(),
            }
        };
        let choice = |name: &str| Choice {
            url: format!("http://{name}/"),
            scheme: "http".to_owned(),
            name: name.to_owned(),
            bound: "0.0.0.0:80".to_owned(),
            route: String::new(),
            host: String::new(),
            why: String::new(),
        };

        let (kept, left) = reachable(
            vec![
                choice("shop.example.com"),
                choice("here.example.com"),
                choice("shop.localhost"),
                choice("gone.example.com"),
            ],
            false,
            &resolve,
        );
        let names: Vec<&str> = kept.iter().map(|choice| choice.name.as_str()).collect();
        assert_eq!(names, ["shop.localhost", "here.example.com"]);
        assert_eq!(
            left,
            [
                "shop.example.com resolves to 93.184.215.14, not this listener",
                "gone.example.com does not resolve"
            ]
        );

        let (kept, left) = reachable(vec![choice("shop.example.com")], true, &resolve);
        assert_eq!((kept.len(), left.len()), (1, 0));
    }

    #[test]
    fn a_path_goes_under_the_url_and_a_name_or_a_path_is_told_apart() {
        assert_eq!(
            joined("http://localhost:8080/", Some("/admin/")),
            "http://localhost:8080/admin/"
        );
        assert_eq!(
            joined("http://shop.localhost/shop/", Some("cart")),
            "http://shop.localhost/shop/cart"
        );
        assert_eq!(joined("http://localhost/", None), "http://localhost/");
    }
}
