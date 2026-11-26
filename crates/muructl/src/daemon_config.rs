//! `muructl daemon config` (design specification section 10.3, ADR 0079):
//! the daemon's own configuration file, found, shown, checked and changed
//! without the person having to know where it is or whether a reload is
//! enough. Every change is checked with the code the daemon reads the
//! file with before it is written, written whole, and then given to a
//! running daemon by a reload.

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Args, Subcommand};
use muru_controller::client;
use muru_controller::settings::{NodeSettings, Overrides};
use muru_proto::admin::v1 as pb;
use serde_json::Value;

use crate::admin::{DaemonFileArgs, SocketArgs};

/// The subcommands of `muructl daemon config`.
#[derive(Subcommand)]
pub enum DaemonConfigCommand {
    /// Print the path of the daemon's configuration file, and whether
    /// it is there: a running daemon's own, else the one a daemon
    /// started with the same options would read.
    Path(Where),

    /// Print the file as it is written; with --full, every setting as
    /// the daemon reads it, with where each came from.
    Show {
        /// Write every setting of the plain tables: what the file sets,
        /// marked so, and as comments the default of every other key.
        /// The result is itself a file the daemon takes.
        #[arg(long)]
        full: bool,

        #[command(flatten)]
        place: Where,
    },

    /// Check a file as the daemon checks it when it starts or reloads,
    /// and change nothing: the daemon's own file, or the one named,
    /// whose relative paths are taken from its own directory.
    Check {
        /// The file to check (default: the daemon's own).
        file: Option<PathBuf>,

        #[command(flatten)]
        place: Where,
    },

    /// Open the file in $VISUAL, else $EDITOR, else vi, check what was
    /// saved, write it and have a running daemon reload. A file with a
    /// mistake is never written.
    Edit {
        #[command(flatten)]
        how: How,

        #[command(flatten)]
        place: Where,
    },

    /// Set one key by its dotted path, such as limits.max_body_size
    /// 50MB, keeping the file's comments and layout; then check, write
    /// and reload as edit does.
    Set {
        /// The key, as TABLE.KEY.
        key: String,

        /// The value: a TOML value such as true, 4 or ["a", "b"], and
        /// anything else as a string.
        value: String,

        #[command(flatten)]
        how: How,

        #[command(flatten)]
        place: Where,
    },

    /// Take one key out of the file, by its dotted path.
    Unset {
        /// The key, as TABLE.KEY.
        key: String,

        #[command(flatten)]
        how: How,

        #[command(flatten)]
        place: Where,
    },

    /// Install a whole file as the daemon's, from a path or from
    /// standard input with -: checked as it would be in the daemon's
    /// place, written, then reloaded.
    Replace {
        /// The file to install, or - for standard input.
        file: PathBuf,

        #[command(flatten)]
        how: How,

        #[command(flatten)]
        place: Where,
    },
}

/// Which daemon is meant.
#[derive(Args)]
pub struct Where {
    #[command(flatten)]
    socket: SocketArgs,

    /// The daemon's own files, as it was started with them.
    #[command(flatten)]
    files: DaemonFileArgs,
}

/// How a change is carried out.
#[derive(Args)]
pub struct How {
    /// Check the result and write nothing.
    #[arg(long)]
    check: bool,

    /// Write the file and leave the daemon as it is; the next reload or
    /// start reads it.
    #[arg(long)]
    no_reload: bool,
}

/// The daemon's file, and the daemon when one runs.
struct Target {
    path: PathBuf,
    secrets: PathBuf,
    running: Option<(client::Endpoint, pb::Status)>,
}

impl Target {
    /// The file's text, empty when it is not there.
    fn text(&self) -> anyhow::Result<String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    /// Check `text` as the daemon would were it the file.
    fn check(&self, text: &str) -> anyhow::Result<()> {
        checked(text, &self.path, &self.secrets)
    }

    /// The flags the daemon was started with that stand over the file.
    fn start_flags(&self) -> &[String] {
        self.running
            .as_ref()
            .map_or(&[], |(_, status)| status.start_flags.as_slice())
    }
}

/// Check `text` as the daemon checks its file, as though it were at
/// `path`. A refusal names the file, which some of the daemon's own
/// messages leave to whoever started it to know.
fn checked(text: &str, path: &Path, secrets: &Path) -> anyhow::Result<()> {
    NodeSettings::check(text, path, secrets, &Overrides::default())
        .map(drop)
        .map_err(|error| {
            let at = path.display().to_string();

            if error.starts_with(&at) {
                anyhow::anyhow!("{error}")
            } else {
                anyhow::anyhow!("{at}: {error}")
            }
        })
}

/// Find the daemon's file: a running daemon says which it reads, and
/// with none running it is the one the options resolve to.
async fn target(place: &Where) -> anyhow::Result<Target> {
    let socket = place.socket.socket()?;
    let locations = crate::admin::daemon_locations(&place.socket, &place.files);

    if socket == locations.admin_socket() && crate::admin::no_daemon(&place.socket, &socket) {
        return Ok(Target {
            path: locations.config.path.clone(),
            secrets: locations.state.path.join("secrets"),
            running: None,
        });
    }

    let endpoint = client::Endpoint::unix(&socket);
    let mut client = client::connect(&endpoint).await?;
    let status = client
        .get_status(pb::GetStatusRequest {})
        .await
        .map_err(|status| anyhow::anyhow!("{}", status.message()))?
        .into_inner();

    if status.config.is_empty() {
        anyhow::bail!(
            "what answers at {} is not a daemon, and has no daemon configuration file",
            socket.display()
        );
    }

    Ok(Target {
        path: PathBuf::from(&status.config),
        secrets: Path::new(&status.state_dir).join("secrets"),
        running: Some((endpoint, status)),
    })
}

/// Run one of the subcommands.
pub async fn run(command: DaemonConfigCommand) -> anyhow::Result<()> {
    match command {
        DaemonConfigCommand::Path(place) => {
            let target = target(&place).await?;

            let present = if target.path.exists() {
                ""
            } else {
                " (not present)"
            };
            println!("{}{present}", target.path.display());
            Ok(())
        }
        DaemonConfigCommand::Show { full, place } => show(&target(&place).await?, full),
        DaemonConfigCommand::Check { file, place } => check(&target(&place).await?, file),
        DaemonConfigCommand::Edit { how, place } => edit(&target(&place).await?, &how).await,
        DaemonConfigCommand::Set {
            key,
            value,
            how,
            place,
        } => {
            let target = target(&place).await?;
            let text = set(&target.text()?, &key, Some(&value))?;
            install(&target, &text, &how).await
        }
        DaemonConfigCommand::Unset { key, how, place } => {
            let target = target(&place).await?;
            let text = set(&target.text()?, &key, None)?;
            install(&target, &text, &how).await
        }
        DaemonConfigCommand::Replace { file, how, place } => {
            let target = target(&place).await?;

            let text = if file == Path::new("-") {
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .context("reading standard input")?;
                text
            } else {
                std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?
            };

            install(&target, &text, &how).await
        }
    }
}

/// Print the file, or with `full` every setting with where it came from.
fn show(target: &Target, full: bool) -> anyhow::Result<()> {
    let text = target.text()?;

    if full {
        print!("{}", full_form(&text, &target.path, target.start_flags())?);
        return Ok(());
    }

    if !target.path.exists() {
        eprintln!(
            "muru: {} is not present; the daemon runs on its defaults (--full shows them)",
            target.path.display()
        );
    }

    print!("{text}");
    Ok(())
}

/// Check the daemon's file, or one named, where it lies.
fn check(target: &Target, file: Option<PathBuf>) -> anyhow::Result<()> {
    let Some(file) = file else {
        target.check(&target.text()?)?;
        println!("muru: {} is valid", target.path.display());
        return Ok(());
    };

    // A file named is checked where it is: would a daemon started with
    // it take it. A replacement is checked in the daemon's place by
    // `replace --check`.

    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let path = std::fs::canonicalize(&file).unwrap_or(file);
    checked(&text, &path, &target.secrets)?;
    println!("muru: {} is valid", path.display());
    Ok(())
}

/// The file's text with one key set, or taken out when `value` is none,
/// its comments and layout kept.
fn set(text: &str, key: &str, value: Option<&str>) -> anyhow::Result<String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error| anyhow::anyhow!("the file does not parse: {error}"))?;
    let parts: Vec<&str> = key.split('.').collect();

    if parts.len() < 2 || parts.iter().any(|part| part.is_empty()) {
        anyhow::bail!(
            "{key:?} is not a key of a table: name it as TABLE.KEY, such as limits.max_body_size"
        );
    }

    let (name, tables) = parts.split_last().expect("at least two parts");
    let mut table = document.as_table_mut();

    for part in tables {
        // A key to take out of a table that is not there is not there.

        if value.is_none() && !table.contains_key(part) {
            anyhow::bail!("{key} is not set in the file");
        }

        let entry = table
            .entry(part)
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));

        table = entry.as_table_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "{part} is not a table with single keys, so muructl daemon config set cannot \
                 change it; use muructl daemon config edit"
            )
        })?;
    }

    match value {
        Some(value) => {
            let parsed = value
                .parse::<toml_edit::Value>()
                .unwrap_or_else(|_| toml_edit::Value::from(value));

            // A key that is there keeps its place and the comments
            // around it; only its value changes.

            match table.get_mut(name).and_then(toml_edit::Item::as_value_mut) {
                Some(existing) => {
                    let decor = existing.decor().clone();
                    *existing = parsed;
                    *existing.decor_mut() = decor;
                }
                None => {
                    table.insert(name, toml_edit::Item::Value(parsed));
                }
            }
        }
        None => {
            if table.remove(name).is_none() {
                anyhow::bail!("{key} is not set in the file");
            }
        }
    }

    Ok(document.to_string())
}

/// What a new file opens with, for an editor to start from.
const TEMPLATE: &str = "\
# The daemon's own configuration: how it listens and runs. What it serves
# (applications, hosts, routes) is not here; give that to muructl apply.
# Every table is optional. muructl daemon config show --full lists every
# setting with its default.

# [[listener]]
# address = \"127.0.0.1:8080\"

# [limits]
# max_body_size = \"10MB\"
";

/// Edit the file in the person's editor until it is valid or given up.
async fn edit(target: &Target, how: &How) -> anyhow::Result<()> {
    let before = target.text()?;

    let start = if before.is_empty() && !target.path.exists() {
        TEMPLATE.to_owned()
    } else {
        before.clone()
    };

    refuse_unwritable(&target.path)?;

    let directory = tempfile::tempdir().context("making a temporary directory")?;
    let copy = directory.path().join("muru.toml");
    std::fs::write(&copy, &start).with_context(|| format!("writing {}", copy.display()))?;

    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "vi".to_owned());

    loop {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(&copy)
            .status()
            .with_context(|| format!("running {editor}"))?;

        if !status.success() {
            anyhow::bail!("{editor} did not finish ({status}); nothing was changed");
        }

        let after = std::fs::read_to_string(&copy)
            .with_context(|| format!("reading {}", copy.display()))?;

        if after == start {
            println!("muru: no changes");
            return Ok(());
        }

        match target.check(&after) {
            Ok(()) => {
                // Somebody else may have changed the file meanwhile.

                if target.text()? != before {
                    anyhow::bail!(
                        "{} changed while it was being edited; nothing was written, and the \
                         edit is in {}",
                        target.path.display(),
                        directory.keep().join("muru.toml").display()
                    );
                }

                return install(target, &after, how).await;
            }
            Err(error) => {
                eprintln!("muru: {error:#}");

                if !std::io::stdin().is_terminal() {
                    anyhow::bail!("the file was not written");
                }

                eprint!("Edit again? [Y/n] ");
                std::io::stderr().flush().ok();
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;

                if matches!(answer.trim(), "n" | "N" | "no") {
                    anyhow::bail!("the file was not written");
                }
            }
        }
    }
}

/// Check a new text for the file, write it and have the daemon take it.
async fn install(target: &Target, text: &str, how: &How) -> anyhow::Result<()> {
    target.check(text)?;

    if how.check {
        println!(
            "muru: valid as {}; nothing was written",
            target.path.display()
        );
        return Ok(());
    }

    let before = target.text()?;

    if before == text && target.path.exists() {
        println!("muru: no changes");
        return Ok(());
    }

    refuse_unwritable(&target.path)?;
    write_whole(&target.path, text)?;
    println!("muru: wrote {}", target.path.display());

    for warning in overridden(&before, text, target.start_flags()) {
        eprintln!("muru: warning: {warning}");
    }

    if how.no_reload {
        return Ok(());
    }

    let Some((endpoint, _)) = &target.running else {
        println!("muru: the daemon is not running; it reads the file when it starts");
        return Ok(());
    };

    let mut client = client::connect(endpoint).await?;
    let response = client
        .reload_config(pb::ReloadConfigRequest {})
        .await
        .map_err(|status| anyhow::anyhow!("{}", status.message()))?
        .into_inner();
    println!("muru: {}", response.message);
    Ok(())
}

/// The keys of the file a start flag stands over, with the flag.
const FLAGGED: [(&str, &[&str]); 4] = [
    ("--listen", &["listener"]),
    ("--metrics", &["metrics"]),
    ("--dashboard", &["dashboard"]),
    ("--user", &["daemon", "user"]),
];

/// A warning for each key the change touched that a flag the daemon was
/// started with stands over, so the file's value is not what serves.
fn overridden(before: &str, after: &str, flags: &[String]) -> Vec<String> {
    let table = |text: &str| text.parse::<toml::Table>().unwrap_or_default();
    let (before, after) = (table(before), table(after));
    let at = |table: &toml::Table, path: &[&str]| {
        let mut value = toml::Value::Table(table.clone());

        for part in path {
            value = value.get(part)?.clone();
        }

        Some(value)
    };

    FLAGGED
        .iter()
        .filter(|(flag, _)| flags.iter().any(|given| given == flag))
        .filter(|(_, path)| at(&before, path) != at(&after, path))
        .map(|(flag, path)| {
            format!(
                "the daemon was started with {flag}, which stands over {} in the file until it \
                 is started without it",
                path.join(".")
            )
        })
        .collect()
}

/// Refuse a file this user cannot write, before anything is edited.
fn refuse_unwritable(path: &Path) -> anyhow::Result<()> {
    let directory = path.parent().unwrap_or(Path::new("."));

    let probe = match path.exists() {
        true => std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .map(drop),
        false => std::fs::create_dir_all(directory)
            .and_then(|()| tempfile::tempfile_in(directory).map(drop)),
    };

    match probe {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => anyhow::bail!(
            "{} cannot be written by this user; run the command with the rights to, as with \
             sudo",
            path.display()
        ),
        Err(error) => Err(error).with_context(|| format!("opening {}", path.display())),
    }
}

/// Write the file whole: a temporary file beside it, with its mode and
/// owner, renamed over it, so the daemon never reads half a file.
fn write_whole(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let directory = path.parent().unwrap_or(Path::new("."));
    let existing = std::fs::metadata(path).ok();
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .with_context(|| format!("writing beside {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    file.as_file().sync_all()?;

    let mode = existing
        .as_ref()
        .map_or(0o644, |metadata| metadata.mode() & 0o7777);
    std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(mode))?;

    // Root writing another user's file leaves it that user's.

    if let Some(metadata) = &existing {
        let _ = std::os::unix::fs::chown(file.path(), Some(metadata.uid()), Some(metadata.gid()));
    }

    file.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Every setting of the file's plain tables: what the file sets, marked
/// so, and every other key as a comment with its default. The tables
/// that hold a list or named entries are written as the file has them.
/// The result is a file the daemon takes, and means what the file means.
fn full_form(text: &str, path: &Path, flags: &[String]) -> anyhow::Result<String> {
    let file: toml::Table = text
        .parse()
        .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
    let schema = muru_config::schema::json_schema(muru_config::schema::File::Daemon);
    let definitions = schema.get("$defs").cloned().unwrap_or_default();
    let resolve = |schema: &Value| -> Value {
        let reference = schema
            .get("$ref")
            .or_else(|| {
                schema
                    .get("anyOf")?
                    .as_array()?
                    .iter()
                    .find_map(|one| one.get("$ref"))
            })
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix("#/$defs/"));

        match reference {
            Some(name) => definitions.get(name).cloned().unwrap_or_default(),
            None => schema.clone(),
        }
    };

    let mut out = format!(
        "# The daemon's configuration as it is read, from {}{}.\n",
        path.display(),
        if path.exists() { "" } else { " (not present)" }
    );

    for flag in flags {
        out.push_str(&format!(
            "# The daemon was started with {flag}, which stands over the file.\n"
        ));
    }

    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(out);
    };

    for (name, property) in properties {
        let table = resolve(property);
        let own = file.get(name);

        // A table of single keys is written key by key. Anything else,
        // a list of entries or entries by name, is the file's own.

        let Some(keys) = table.get("properties").and_then(Value::as_object) else {
            if let Some(own) = own {
                out.push_str(&format!("\n# {name}: the file\n"));
                out.push_str(&toml::to_string(&toml::Table::from_iter([(
                    name.clone(),
                    own.clone(),
                )]))?);
            } else {
                out.push_str(&format!("\n# {name}: none in the file\n"));
            }

            continue;
        };

        let own = own.and_then(toml::Value::as_table);
        let mut lines = Vec::new();
        let mut nested = toml::Table::new();

        for (key, key_schema) in keys {
            match own.and_then(|own| own.get(key)) {
                Some(value @ (toml::Value::Table(_) | toml::Value::Array(_))) if !inline(value) => {
                    nested.insert(key.clone(), value.clone());
                }
                Some(value) => lines.push(format!("{key} = {value}  # the file")),
                // A default is written as a comment: the file as it is
                // written means what it meant, and a table the file
                // does not have stays out of it.
                None => match key_schema.get("default").filter(|value| !value.is_null()) {
                    Some(default) => match toml::Value::try_from(default) {
                        Ok(value) if inline(&value) => {
                            lines.push(format!("# {key} = {value}  (default)"));
                        }
                        _ => lines.push(format!("# {key}: not set")),
                    },
                    None => match key_schema.get("x-default").and_then(Value::as_str) {
                        Some(described) => {
                            lines.push(format!("# {key}: {described} (default)"));
                        }
                        None => lines.push(format!("# {key}: not set")),
                    },
                },
            }
        }

        if own.is_some() {
            out.push_str(&format!("\n[{name}]\n"));
        } else {
            out.push_str(&format!("\n# [{name}]: not in the file\n"));
        }

        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }

        if !nested.is_empty() {
            out.push_str(&format!("\n# {name}'s own tables: the file\n"));
            out.push_str(&toml::to_string(&toml::Table::from_iter([(
                name.clone(),
                toml::Value::Table(nested),
            )]))?);
        }
    }

    Ok(out)
}

/// Whether a value is written on its key's line: anything but a table
/// or a list that holds tables.
fn inline(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(_) => false,
        toml::Value::Array(items) => items.iter().all(inline),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_changes_one_key_and_keeps_the_rest_of_the_file() {
        let text = "# how it listens\n[[listener]]\naddress = \"127.0.0.1:8080\"\n\n\
                    [limits]\n# uploads\nmax_body_size = \"10MB\"\n";

        let changed = set(text, "limits.max_body_size", Some("50MB")).unwrap();
        assert!(
            changed.contains("# how it listens\n[[listener]]"),
            "{changed}"
        );
        assert!(
            changed.contains("# uploads\nmax_body_size = \"50MB\""),
            "{changed}"
        );

        // A value that is TOML is taken as it is, and a table that is
        // not there is made.

        let added = set(&changed, "daemon.detours", Some("true")).unwrap();
        assert!(added.contains("[daemon]\ndetours = true"), "{added}");

        let removed = set(&added, "daemon.detours", None).unwrap();
        assert!(!removed.contains("detours"), "{removed}");

        for (key, value, why) in [
            ("limits", Some("1"), "is not a key of a table"),
            (
                "listener.address",
                Some("x"),
                "is not a table with single keys",
            ),
            ("limits.queue_timeout", None, "is not set in the file"),
            ("proxy.trusted", None, "is not set in the file"),
        ] {
            let error = set(text, key, value).unwrap_err().to_string();
            assert!(error.contains(why), "{key}: {error}");
        }
    }

    #[test]
    fn the_template_and_the_full_form_are_files_the_daemon_takes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("muru.toml");
        let check = |text: &str| {
            NodeSettings::check(text, &path, directory.path(), &Overrides::default()).map(drop)
        };

        check(TEMPLATE).unwrap();

        let text = "[[listener]]\naddress = \"127.0.0.1:8080\"\n\n[limits]\nmax_body_size = \"50MB\"\n\n\
                    [condition.busy]\nwhen = \"queue_depth{app='shop'} > 5\"\n";
        let full = full_form(text, &path, &["--metrics".to_owned()]).unwrap();
        check(&full).unwrap_or_else(|error| panic!("{error}\n{full}"));
        assert!(
            full.contains("max_body_size = \"50MB\"  # the file"),
            "{full}"
        );
        assert!(full.contains("(default)"), "{full}");
        assert!(full.contains("# [metrics]: not in the file"), "{full}");
        assert!(full.contains("started with --metrics"), "{full}");
        assert!(full.contains("[[listener]]"), "{full}");

        // With no file it is the defaults, and still a file.

        check(&full_form("", &path, &[]).unwrap()).unwrap();
    }

    #[test]
    fn a_change_under_a_start_flag_is_warned_of() {
        let flags = ["--listen".to_owned(), "--user".to_owned()];
        let before = "[limits]\nmax_body_size = \"10MB\"\n";
        let after =
            "[[listener]]\naddress = \"0.0.0.0:80\"\n\n[limits]\nmax_body_size = \"50MB\"\n";

        let warned = overridden(before, after, &flags);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains("--listen") && warned[0].contains("listener"));
        assert!(overridden(before, after, &[]).is_empty());
    }
}
