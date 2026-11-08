//! The skill a project carries for coding agents (design specification
//! section 10.11, ADR 0039): what the MCP tools cannot teach, embedded in
//! muructl and versioned with it, written into the project's skills
//! directory so it travels with the code and needs no channel of its own.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Subcommand;
use serde_json::json;

use crate::admin::{JsonArgs, print_answer};

/// The skill's text after its front matter.
const BODY: &str = include_str!("skill.md");

/// What the agent reads to decide when the skill applies.
const DESCRIPTION: &str = "Develop, deploy, test and debug this project's Python web application under Muru: releases, traffic, rollouts, events, logs and worker stacks, through the muru MCP tools or muructl --json.";

/// Claude Code's skills directory in a project, where `install` writes and
/// `status` looks unless told otherwise.
pub const DEFAULT_DIR: &str = ".claude/skills";

/// The skill subcommands.
#[derive(Subcommand)]
pub enum SkillCommand {
    /// Print the skill to standard output.
    Print,

    /// Write the skill into the project's skills directory, or update it
    /// there, as muru/SKILL.md. A copy with local changes is replaced only
    /// with --force.
    Install {
        /// The skills directory, for a client other than Claude Code.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_DIR)]
        dir: PathBuf,

        /// Replace a copy that has local changes, which are lost.
        #[arg(long)]
        force: bool,
    },

    /// Say whether the project's copy of the skill matches this muructl's,
    /// is out of date, or has local changes; exits 1 unless it matches.
    Status {
        /// The skills directory, for a client other than Claude Code.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_DIR)]
        dir: PathBuf,

        #[command(flatten)]
        output: JsonArgs,
    },
}

/// Run a `skill` subcommand.
pub fn run(command: SkillCommand) -> anyhow::Result<()> {
    match command {
        SkillCommand::Print => {
            print!("{}", text());
            Ok(())
        }
        SkillCommand::Install { dir, force } => {
            let report = status(&dir);
            let path = file(&dir);
            let edited = report["edited"] == true;

            anyhow::ensure!(
                !edited || force,
                "{}. Keep the project's own notes in a skill of their own or in CLAUDE.md, \
                 since this skill is replaced on every upgrade",
                describe(&report)
            );

            let parent = path.parent().unwrap_or(&dir);
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            std::fs::write(&path, text()).with_context(|| format!("writing {}", path.display()))?;

            let replaced = match report["state"].as_str() {
                Some("missing") => String::new(),
                Some("current") => ", which already matched".to_owned(),
                _ if edited => format!(
                    ", replacing the one from muructl {} and its local changes",
                    report["installed_version"].as_str().unwrap_or("unknown")
                ),
                _ => format!(
                    ", replacing the one from muructl {}",
                    report["installed_version"].as_str().unwrap_or("unknown")
                ),
            };

            println!(
                "muru: wrote the skill for muructl {} to {}{replaced}",
                version(),
                path.display()
            );
            Ok(())
        }
        SkillCommand::Status { dir, output } => {
            let report = status(&dir);
            let current = report["state"] == "current";

            if output.json {
                print_answer(&report)?;
            } else {
                println!("muru: {}", describe(&report));
            }

            if !current {
                std::process::exit(1);
            }

            Ok(())
        }
    }
}

/// The skill file in a skills directory.
pub fn file(dir: &Path) -> PathBuf {
    dir.join("muru").join("SKILL.md")
}

/// The version the skill is written for: muructl's own.
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The skill as written: front matter recording the version and a hash of
/// the body, so a copy can be checked against the muructl in use.
pub fn text() -> String {
    format!(
        "---\nname: muru\ndescription: {DESCRIPTION}\nmetadata:\n  muructl-version: \"{}\"\n  content-hash: \"sha256:{}\"\n---\n\n{BODY}",
        version(),
        hash(BODY)
    )
}

/// The body's SHA-256, in hex.
fn hash(body: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, body.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A skill file split into its front matter and its body, the blank
/// line after the front matter left out of both.
fn split(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    let (front, after) = rest.split_once("\n---\n")?;
    Some((front, after.strip_prefix('\n').unwrap_or(after)))
}

/// The value of a key in a skill file's front matter.
fn front_matter<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    split(text)?
        .0
        .lines()
        .find_map(|line| line.trim().strip_prefix(key)?.strip_prefix(':'))
        .map(|value| value.trim().trim_matches('"'))
}

/// How the project's copy compares with this muructl's skill: its path,
/// its state, the versions, and whether it has local changes. The state
/// is `current`, `missing`, `out_of_date` (written by another muructl),
/// `edited` (this muructl's skill with local changes) or
/// `edited_out_of_date` (both). A copy is edited when its body no longer
/// matches the `content-hash` its front matter records.
pub fn status(dir: &Path) -> serde_json::Value {
    let path = file(dir);
    let installed = std::fs::read_to_string(&path).ok();
    let installed_version = installed
        .as_deref()
        .and_then(|text| front_matter(text, "muructl-version"))
        .map(str::to_owned);

    let state = match &installed {
        None => "missing",
        Some(text) if *text == self::text() => "current",
        Some(text) => {
            let recorded = front_matter(text, "content-hash");
            let edited = split(text).is_none_or(|(_, body)| {
                recorded != Some(format!("sha256:{}", hash(body)).as_str())
            });
            let same_skill = installed_version.as_deref() == Some(version())
                && recorded == Some(format!("sha256:{}", hash(BODY)).as_str());

            match (edited, same_skill) {
                (true, true) => "edited",
                (true, false) => "edited_out_of_date",
                (false, _) => "out_of_date",
            }
        }
    };

    json!({
        "path": path.display().to_string(),
        "state": state,
        "edited": state.starts_with("edited"),
        "version": version(),
        "installed_version": installed_version,
    })
}

/// A status report as a sentence, naming what to run when the copy is
/// missing, out of date or edited.
pub fn describe(report: &serde_json::Value) -> String {
    let path = report["path"].as_str().unwrap_or_default();
    let installed = report["installed_version"].as_str().unwrap_or("unknown");

    match report["state"].as_str() {
        Some("current") => format!("the skill at {path} matches muructl {}", version()),
        Some("missing") => {
            format!("the project carries no skill at {path}; `muructl mcp skill install` writes it")
        }
        Some("edited") => format!(
            "the skill at {path} has local changes to the skill for muructl {}; \
             `muructl mcp skill install --force` replaces them",
            version()
        ),
        Some("edited_out_of_date") => format!(
            "the skill at {path} is from muructl {installed} and has local changes; this is \
             muructl {}, and `muructl mcp skill install --force` replaces it, changes and all",
            version()
        ),
        _ if installed == version() => format!(
            "the skill at {path} is from another build of muructl {installed}, whose skill \
             differs; `muructl mcp skill install` updates it"
        ),
        _ => format!(
            "the skill at {path} is from muructl {installed}; this is muructl {}, and \
             `muructl mcp skill install` updates it",
            version()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{status, text};

    #[test]
    fn status_tells_an_edited_copy_from_an_out_of_date_one() {
        let dir = tempfile::tempdir().expect("a directory");
        assert_eq!(status(dir.path())["state"], "missing");

        let path = super::file(dir.path());
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(&path, text()).expect("the skill");
        let report = status(dir.path());
        assert_eq!(report["state"], "current");
        assert_eq!(report["edited"], false);
        assert_eq!(report["installed_version"], env!("CARGO_PKG_VERSION"));

        // A line added to the body is a local change.

        std::fs::write(&path, format!("{}\nOur notes.\n", text())).expect("an edit");
        let report = status(dir.path());
        assert_eq!(report["state"], "edited");
        assert!(super::describe(&report).contains("--force"));

        // Another muructl's skill, untouched, is out of date; the same
        // with a change is both.

        let older = text().replace(
            &format!("muructl-version: \"{}\"", env!("CARGO_PKG_VERSION")),
            "muructl-version: \"1.0.0\"",
        );
        std::fs::write(&path, &older).expect("an older skill");
        let report = status(dir.path());
        assert_eq!(report["state"], "out_of_date");
        assert!(super::describe(&report).contains("is from muructl 1.0.0; this is"));

        std::fs::write(&path, older.replace("Developing with Muru", "Edited")).expect("an edit");
        assert_eq!(status(dir.path())["state"], "edited_out_of_date");
    }

    #[test]
    fn the_front_matter_names_the_skill_and_records_the_hash() {
        let text = text();
        assert!(text.starts_with("---\nname: muru\ndescription: "));
        assert!(text.contains("content-hash: \"sha256:"));
        assert!(text.contains("# Developing with Muru"));
    }
}
