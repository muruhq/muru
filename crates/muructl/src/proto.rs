//! `muructl proto` (issue 858, with section 20.11 of the design
//! specification): the `.proto` files of the admin API this muructl was
//! built from, written out for a client that is not part of Muru, so its
//! stubs come from exactly the release it uses, offline and with no tag
//! to work out. Shaped like `muructl mcp skill`: each file written records
//! the muructl it came from and a hash of its body, so `status` can tell a
//! file of another release from one with local changes.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Subcommand;
use serde_json::{Value, json};

use crate::admin::{JsonArgs, print_answer};

/// The proto subcommands.
#[derive(Subcommand)]
pub enum ProtoCommand {
    /// Write the admin API's .proto files beneath DIR, laid out by package
    /// so that `protoc -I DIR` resolves their imports. Files already there
    /// are replaced.
    Export {
        /// The directory the files are written beneath.
        dir: PathBuf,

        /// Also write a compiled FileDescriptorSet of the files, with
        /// their imports, to FILE, for grpcurl -protoset, buf or a
        /// gRPC-JSON transcoder.
        #[arg(long, value_name = "FILE")]
        descriptor_set: Option<PathBuf>,
    },

    /// Say whether the .proto files beneath DIR match this muructl's, are
    /// of another release, or have local changes; exits 1 unless they
    /// match.
    Status {
        /// The directory the files were exported beneath.
        dir: PathBuf,

        #[command(flatten)]
        output: JsonArgs,
    },

    /// Write one .proto file to standard output, as export writes it.
    Print {
        /// The file, by its path beneath the export directory.
        #[arg(default_value = "muru/admin/v1/admin.proto")]
        path: String,
    },

    /// Name the .proto files, with the package and services of each.
    List {
        #[command(flatten)]
        output: JsonArgs,
    },
}

/// Run a `proto` subcommand.
pub fn run(command: ProtoCommand) -> anyhow::Result<()> {
    match command {
        ProtoCommand::Export {
            dir,
            descriptor_set,
        } => export(&dir, descriptor_set.as_deref()),
        ProtoCommand::Status { dir, output } => {
            let report = status(&dir);

            if output.json {
                print_answer(&report)?;
            } else {
                println!("muru: {}", describe(&report));
            }

            if report["state"] != "current" {
                std::process::exit(1);
            }

            Ok(())
        }
        ProtoCommand::Print { path } => {
            let Some((_, body)) = files().find(|(name, _)| *name == path) else {
                anyhow::bail!(
                    "muructl has no {path}; the files are {}",
                    names().join(", ")
                );
            };

            print!("{}", text(body));
            Ok(())
        }
        ProtoCommand::List { output } => {
            let report = list();

            if output.json {
                return print_answer(&report);
            }

            for file in report["files"].as_array().into_iter().flatten() {
                let services: Vec<&str> = file["services"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();

                println!(
                    "{}  package {}, services {}",
                    file["path"].as_str().unwrap_or_default(),
                    file["package"].as_str().unwrap_or_default(),
                    services.join(", ")
                );
            }

            Ok(())
        }
    }
}

/// The files given out, by path.
fn files() -> impl Iterator<Item = (&'static str, &'static str)> {
    muru_proto::public::FILES.iter().copied()
}

/// The paths of the files given out.
fn names() -> Vec<&'static str> {
    files().map(|(name, _)| name).collect()
}

/// The version the files are written for: muructl's own.
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The body's SHA-256, in hex.
fn hash(body: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, body.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The lines put in front of each file written, which `status` reads.
const HEADER_LINES: usize = 3;

/// A file as written: three comment lines recording the muructl and a
/// hash of the body, then the file as the binary was built from it.
fn text(body: &str) -> String {
    format!(
        "// muructl-version: \"{}\"\n// content-hash: \"sha256:{}\"\n// Written by `muructl proto export`; `muructl proto status` checks it.\n{body}",
        version(),
        hash(body)
    )
}

/// A written file split into the values its header records and its body.
fn split(text: &str) -> Option<(Option<&str>, Option<&str>, &str)> {
    let mut rest = text;
    let mut header = Vec::with_capacity(HEADER_LINES);

    for _ in 0..HEADER_LINES {
        let (line, after) = rest.split_once('\n')?;
        header.push(line.strip_prefix("// ")?);
        rest = after;
    }

    let value = |key: &str| {
        header
            .iter()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
            .map(|value| value.trim().trim_matches('"'))
    };

    Some((value("muructl-version"), value("content-hash"), rest))
}

/// Write the files beneath `dir`, and the descriptor set if asked for.
fn export(dir: &Path, descriptor_set: Option<&Path>) -> anyhow::Result<()> {
    let report = status(dir);

    for (name, body) in files() {
        let path = dir.join(name);
        let parent = path.parent().unwrap_or(dir);

        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        std::fs::write(&path, text(body)).with_context(|| format!("writing {}", path.display()))?;
    }

    let replaced = match report["state"].as_str() {
        Some("missing") => String::new(),
        Some("current") => ", which already matched".to_owned(),
        _ if report["edited"] == true => ", replacing files with local changes".to_owned(),
        _ => format!(
            ", replacing those of muructl {}",
            report["installed_version"].as_str().unwrap_or("unknown")
        ),
    };

    println!(
        "muru: wrote the .proto files of muructl {} beneath {}{replaced}",
        version(),
        dir.display()
    );

    if let Some(file) = descriptor_set {
        std::fs::write(file, muru_proto::public::DESCRIPTOR_SET)
            .with_context(|| format!("writing {}", file.display()))?;
        println!("muru: wrote the descriptor set to {}", file.display());
    }

    Ok(())
}

/// How one file beneath `dir` compares with this muructl's: `current`,
/// `missing`, `out_of_date` (written by another muructl, or another build
/// of this version whose file differs), `edited` (this muructl's file with
/// local changes) or `edited_out_of_date` (both). A file is edited when its
/// body no longer matches the hash its header records.
fn file_status(dir: &Path, name: &str, body: &str) -> Value {
    let path = dir.join(name);
    let written = std::fs::read_to_string(&path).ok();
    let parts = written.as_deref().map(split);
    let installed_version = parts
        .flatten()
        .and_then(|(version, _, _)| version)
        .map(str::to_owned);

    let state = match (&written, parts.flatten()) {
        (None, _) => "missing",
        (Some(found), _) if *found == text(body) => "current",
        (Some(_), None) => "edited_out_of_date",
        (Some(_), Some((version, recorded, found))) => {
            let edited = recorded != Some(format!("sha256:{}", hash(found)).as_str());
            let same = version == Some(self::version())
                && recorded == Some(format!("sha256:{}", hash(body)).as_str());

            match (edited, same) {
                (true, true) => "edited",
                (true, false) => "edited_out_of_date",
                (false, _) => "out_of_date",
            }
        }
    };

    json!({
        "path": path.display().to_string(),
        "name": name,
        "state": state,
        "installed_version": installed_version,
    })
}

/// How the files beneath `dir` compare with this muructl's: each file's
/// state, and one for them all, `current` only when every file is.
pub fn status(dir: &Path) -> Value {
    let files: Vec<Value> = files()
        .map(|(name, body)| file_status(dir, name, body))
        .collect();
    let states: Vec<&str> = files
        .iter()
        .filter_map(|file| file["state"].as_str())
        .collect();
    let edited = states.iter().any(|state| state.starts_with("edited"));
    let behind = states
        .iter()
        .any(|state| matches!(*state, "out_of_date" | "edited_out_of_date"));

    let state = if states.iter().all(|state| *state == "current") {
        "current"
    } else if states.iter().all(|state| *state == "missing") {
        "missing"
    } else {
        match (edited, behind) {
            (true, true) => "edited_out_of_date",
            (true, false) => "edited",
            (false, _) => "out_of_date",
        }
    };

    let installed_version = files
        .iter()
        .find_map(|file| file["installed_version"].as_str())
        .map(str::to_owned);

    json!({
        "dir": dir.display().to_string(),
        "state": state,
        "edited": edited,
        "version": version(),
        "installed_version": installed_version,
        "files": files,
    })
}

/// A status report as a sentence, naming what to run when the files are
/// missing, out of date or edited.
fn describe(report: &Value) -> String {
    let dir = report["dir"].as_str().unwrap_or_default();
    let installed = report["installed_version"].as_str().unwrap_or("unknown");

    match report["state"].as_str() {
        Some("current") => format!("the .proto files beneath {dir} match muructl {}", version()),
        Some("missing") => format!(
            "there are no .proto files of Muru's beneath {dir}; `muructl proto export {dir}` writes them"
        ),
        Some("edited") => format!(
            "the .proto files beneath {dir} have local changes to those of muructl {}; \
             `muructl proto export {dir}` replaces them",
            version()
        ),
        Some("edited_out_of_date") => format!(
            "the .proto files beneath {dir} are from muructl {installed} and have local \
             changes; this is muructl {}, and `muructl proto export {dir}` replaces them",
            version()
        ),
        _ if installed == version() => format!(
            "the .proto files beneath {dir} are missing some of this muructl's, or are from \
             another build of muructl {installed}; `muructl proto export {dir}` updates them"
        ),
        _ => format!(
            "the .proto files beneath {dir} are from muructl {installed}; this is muructl {}, \
             and `muructl proto export {dir}` updates them",
            version()
        ),
    }
}

/// The files with the package and the services each declares.
fn list() -> Value {
    let files: Vec<Value> = files()
        .map(|(name, body)| {
            let declared = |keyword: &str| -> Vec<String> {
                body.lines()
                    .filter_map(|line| line.trim().strip_prefix(keyword))
                    .filter_map(|rest| {
                        let name = rest.trim().trim_end_matches(['{', ';']).trim();
                        (!name.is_empty()).then(|| name.to_owned())
                    })
                    .collect()
            };

            json!({
                "path": name,
                "package": declared("package ").into_iter().next(),
                "services": declared("service "),
            })
        })
        .collect();

    json!({ "version": version(), "files": files })
}

#[cfg(test)]
mod tests {
    use super::{hash, list, status, text};

    const ADMIN: &str = "muru/admin/v1/admin.proto";

    fn admin() -> &'static str {
        super::files()
            .find(|(name, _)| *name == ADMIN)
            .expect("the admin protocol is given out")
            .1
    }

    #[test]
    fn only_the_admin_api_is_given_out() {
        let report = list();
        let files = report["files"].as_array().expect("files");

        assert_eq!(files.len(), 1, "{report}");
        assert_eq!(files[0]["path"], ADMIN);
        assert_eq!(files[0]["package"], "muru.admin.v1");
        assert_eq!(files[0]["services"], serde_json::json!(["Admin"]));
        assert!(!muru_proto::public::DESCRIPTOR_SET.is_empty());
    }

    #[test]
    fn status_tells_an_edited_file_from_one_of_another_release() {
        let dir = tempfile::tempdir().expect("a directory");
        assert_eq!(status(dir.path())["state"], "missing");

        let path = dir.path().join(ADMIN);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
        std::fs::write(&path, text(admin())).expect("the file");
        let report = status(dir.path());
        assert_eq!(report["state"], "current", "{report}");
        assert_eq!(report["installed_version"], env!("CARGO_PKG_VERSION"));

        // A line added to the body is a local change.

        std::fs::write(&path, format!("{}// ours\n", text(admin()))).expect("an edit");
        let report = status(dir.path());
        assert_eq!(report["state"], "edited", "{report}");
        assert_eq!(report["edited"], true);

        // Another muructl's file, untouched, is out of date; the same
        // with a change is both; a file with no header is both too.

        let older = text("syntax = \"proto3\";\n").replace(
            &format!("muructl-version: \"{}\"", env!("CARGO_PKG_VERSION")),
            "muructl-version: \"1.0.0\"",
        );
        std::fs::write(&path, &older).expect("an older file");
        let report = status(dir.path());
        assert_eq!(report["state"], "out_of_date", "{report}");
        assert!(super::describe(&report).contains("are from muructl 1.0.0; this is"));

        std::fs::write(&path, format!("{older}// ours\n")).expect("an edit");
        assert_eq!(status(dir.path())["state"], "edited_out_of_date");

        std::fs::write(&path, admin()).expect("a copy with no header");
        assert_eq!(status(dir.path())["state"], "edited_out_of_date");
    }

    #[test]
    fn the_header_records_the_hash_of_the_file_as_built() {
        let written = text(admin());
        let (version, recorded, body) = super::split(&written).expect("a header");

        assert_eq!(version, Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(recorded, Some(format!("sha256:{}", hash(admin())).as_str()));
        assert_eq!(body, admin());
    }
}
