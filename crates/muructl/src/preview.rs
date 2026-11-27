//! `muructl preview` (design specification sections 4.22 and 13.1, ADR
//! 0084): a one-time entry link to a release's preview, opened in the
//! browser or printed, or the `Muru-Preview` header that previews it.
//! The release is a detour or any other release the daemon's edge routes
//! to; the daemon chooses it when the command names an application
//! alone.

use std::time::Duration;

use clap::Args;
use muru_proto::admin::v1 as pb;

use crate::admin::SocketArgs;

/// The admin API's client, as this module's functions take it.
type Client = pb::admin_client::AdminClient<tonic::transport::Channel>;

/// `muructl preview`.
#[derive(Args)]
pub struct PreviewArgs {
    /// What to preview: a release as APP@RELEASE, an application, which
    /// means its open detour or else its one held release, or a detour
    /// by its release, its label, the tag muructl instance list shows or
    /// a prefix of its ID (default: the project's application, else the
    /// one detour open).
    #[arg(value_name = "TARGET")]
    target: Option<String>,

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

    /// Print the Muru-Preview header that previews the release without a
    /// cookie, for curl or a test client.
    #[arg(long, conflicts_with_all = ["list", "print", "host", "name", "valid_for"])]
    header: bool,

    /// Build the link from this name, which must be one of the names or
    /// patterns of a host that routes the application, such as one name
    /// of a host whose names are all wildcards.
    #[arg(long, value_name = "NAME")]
    name: Option<String>,

    /// Keep names that resolve to another machine.
    #[arg(long)]
    any_name: bool,

    /// How long the link can be used for, once, for example 10m
    /// (default 1m, at most 1h).
    #[arg(long, value_name = "DURATION", value_parser = muru_config::parse_duration)]
    valid_for: Option<Duration>,

    /// Give the release a new preview secret first: every link, cookie
    /// and header given out before stops previewing it.
    #[arg(long)]
    new_secret: bool,

    #[command(flatten)]
    output: crate::admin::JsonArgs,

    /// Which daemon the release is on.
    #[command(flatten)]
    socket: SocketArgs,
}

/// What a preview is asked of: the application, and the release, or
/// none for the daemon to choose.
type Target = (String, String);

/// The release a command names. `APP@RELEASE` is taken as it is. Any
/// other word is a detour, as `muructl detour traffic` finds one, or
/// else an application, whose open detour or one held release the
/// daemon chooses. With no word it is the project's application, else
/// the daemon's one detour.
async fn target(client: &mut Client, wanted: Option<&str>) -> anyhow::Result<Target> {
    if let Some((app, release)) = wanted.and_then(|wanted| wanted.split_once('@')) {
        anyhow::ensure!(
            !app.is_empty() && !release.is_empty(),
            "a release is named as APP@RELEASE"
        );
        return Ok((app.to_owned(), release.to_owned()));
    }

    let detours = crate::detour::listed(client).await?;

    let Some(wanted) = wanted else {
        // The project's application, when the daemon has it: the daemon
        // chooses its detour or its held release. A directory that names
        // no application of the daemon's leaves the one detour open.

        let project = std::env::current_dir()
            .ok()
            .and_then(|directory| crate::open::project_app(&directory));

        if let Some(app) = project
            && (detours.iter().any(|detour| detour.app == app) || known(client, &app).await)
        {
            return Ok((app, String::new()));
        }

        return match detours.as_slice() {
            [only] => Ok((only.app.clone(), only.release.clone())),
            _ => anyhow::bail!(
                "name what to preview, as APP@RELEASE or an application: this directory is no \
                 project of an application of this daemon, and {}",
                match detours.len() {
                    0 => "no detour is open".to_owned(),
                    open => format!("{open} detours are open"),
                }
            ),
        };
    };

    // A detour first, by any of its names. An application with several
    // detours open is refused here, naming each.

    let refusal = match muru_controller::detours::choose(Some(wanted), &detours) {
        Ok(detour) => return Ok((detour.app.clone(), detour.release.clone())),
        Err(refusal) => refusal,
    };

    anyhow::ensure!(
        !detours.iter().any(|detour| detour.app == wanted),
        "{refusal}"
    );

    anyhow::ensure!(
        known(client, wanted).await,
        "{wanted} is no application of this daemon and no open detour; a release is named as \
         APP@RELEASE"
    );

    Ok((wanted.to_owned(), String::new()))
}

/// Whether the daemon has an application of the name.
async fn known(client: &mut Client, app: &str) -> bool {
    client
        .get_app(pb::GetAppRequest {
            name: app.to_owned(),
        })
        .await
        .is_ok()
}

/// What a preview is asked for, beside its release.
#[derive(Default)]
struct Asked<'a> {
    host: Option<&'a str>,
    all: bool,
    name: Option<&'a str>,
    valid_for: Option<Duration>,
    new_secret: bool,
}

/// Ask the daemon for entry links to a release's preview.
async fn mint(
    client: &mut Client,
    (app, release): &Target,
    asked: Asked<'_>,
) -> anyhow::Result<pb::ReleasePreview> {
    Ok(client
        .preview_release(pb::PreviewReleaseRequest {
            app: app.clone(),
            release: release.clone(),
            host: asked.host.unwrap_or_default().to_owned(),
            all: asked.all,
            name: asked.name.unwrap_or_default().to_owned(),
            valid_for_ms: asked
                .valid_for
                .map_or(0, |valid_for| valid_for.as_millis() as u64),
            new_secret: asked.new_secret,
        })
        .await
        .map_err(crate::admin::explain)?
        .into_inner())
}

/// The entry links whose names reach this machine, as `muructl open`
/// checks its URLs, with why each name left out was (issue 908); every
/// link with `any_name`.
fn reachable_links(
    preview: &pb::ReleasePreview,
    any_name: bool,
) -> (Vec<pb::PreviewLink>, Vec<String>) {
    let choices = preview
        .links
        .iter()
        .map(|link| crate::open::Choice {
            url: link.url.clone(),
            scheme: String::new(),
            name: link.name.clone(),
            bound: link.bound.clone(),
            route: String::new(),
            host: String::new(),
            why: link.address.clone(),
        })
        .collect();
    let (kept, left_out) = crate::open::reachable_here(choices, any_name);
    let links = kept
        .into_iter()
        .filter_map(|choice| {
            preview
                .links
                .iter()
                .find(|link| link.url == choice.url)
                .cloned()
        })
        .collect();

    (links, left_out)
}

/// The link to open: the first whose name reaches this machine, or why
/// there is none.
fn chosen_link(
    preview: &pb::ReleasePreview,
    any_name: bool,
) -> anyhow::Result<(pb::PreviewLink, usize)> {
    if preview.links.is_empty() {
        anyhow::bail!(
            "{}; muructl preview --header gives the header that previews it",
            preview.unreachable
        );
    }

    let (links, left_out) = reachable_links(preview, any_name);

    match links.first() {
        Some(link) => Ok((link.clone(), links.len() - 1)),
        None => anyhow::bail!(
            "none of the application's names reaches this machine: {}; pass --name with one \
             that does, or --any-name to keep them",
            left_out.join("; ")
        ),
    }
}

/// Open an entry link in the browser, saying where it leads and how
/// many other addresses there are.
fn open_link(link: &pb::PreviewLink, others: usize, release: &str) {
    let others = match others {
        0 => String::new(),
        1 => " (1 other address: muructl preview --list)".to_owned(),
        count => format!(" ({count} other addresses: muructl preview --list)"),
    };

    muru_common::muru_println!("opened {} in preview of {release}{others}", link.address);
    let url = link.url.clone();
    std::thread::spawn(move || crate::browser::open(&url));
}

/// Run `muructl preview`.
pub async fn preview(args: PreviewArgs) -> anyhow::Result<()> {
    let mut client = muru_controller::client::connect(&args.socket.endpoint()?).await?;
    let target = target(&mut client, args.target.as_deref()).await?;

    // Every address's link is asked for, so the first whose name reaches
    // this machine can be chosen; a host asked for gives its one.

    let preview = mint(
        &mut client,
        &target,
        Asked {
            host: args.host.as_deref(),
            all: args.host.is_none(),
            name: args.name.as_deref(),
            valid_for: args.valid_for,
            new_secret: args.new_secret,
        },
    )
    .await?;
    let name = format!("{}@{}", preview.app, preview.release);

    // An agent or a script is given the header and the link chosen, or
    // every link that reaches this machine with --list, or why there is
    // none, and opens nothing.

    if args.output.json {
        let mut answer = preview.clone();

        if !answer.links.is_empty() {
            let (links, left_out) = reachable_links(&preview, args.any_name);
            answer.others = u32::try_from(links.len().saturating_sub(1)).unwrap_or(u32::MAX);
            answer.links = if args.list {
                links
            } else {
                links.into_iter().take(1).collect()
            };

            if answer.links.is_empty() {
                answer.unreachable = format!(
                    "none of the application's names reaches this machine: {}",
                    left_out.join("; ")
                );
            }
        }

        return crate::admin::print_answer(&answer);
    }

    if args.new_secret {
        muru_common::muru_eprintln!(
            "{name} has a new preview secret: links, cookies and headers given out before no \
             longer preview it"
        );
    }

    if args.header {
        println!("Muru-Preview: {}", preview.header);
        return Ok(());
    }

    if args.list {
        anyhow::ensure!(
            !preview.links.is_empty(),
            "{}; muructl preview --header gives the header that previews it",
            preview.unreachable
        );
        let (links, left_out) = reachable_links(&preview, args.any_name);

        for link in &links {
            println!("{}  {}", link.url, link.address);
        }

        for left in left_out {
            println!("left out: {left}");
        }

        return Ok(());
    }

    let (link, others) = chosen_link(&preview, args.any_name)?;

    if args.print {
        println!("{}", link.url);
        return Ok(());
    }

    open_link(&link, others, &name);

    // The browser is given a moment to start before muructl exits.

    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(())
}

/// `--open-preview`: an entry link for a release just started, a detour
/// or one deployed with `--hold`, opened in the browser.
pub async fn open_for(
    daemon: &muru_controller::client::Endpoint,
    app: &str,
    release: &str,
    name_asked: Option<&str>,
    any_name: bool,
) -> anyhow::Result<()> {
    let mut client = muru_controller::client::connect(daemon).await?;
    let preview = mint(
        &mut client,
        &(app.to_owned(), release.to_owned()),
        Asked {
            all: true,
            name: name_asked,
            ..Asked::default()
        },
    )
    .await?;
    let (link, others) = chosen_link(&preview, any_name)?;
    open_link(&link, others, &format!("{app}@{release}"));
    Ok(())
}
