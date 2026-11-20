//! The catalogue of named instances (design specification section 13.8,
//! issue 810, ADR 0070): the instances a user knows by name, defined in
//! the configuration area, apart from the registry, which records only
//! what runs.
//!
//! A named instance is `CONFIG/instances/NAME/`, holding `instance.toml`,
//! which says what the definition is (`kind = "daemon"`, and a
//! description of its own), and for a daemon `muru.toml`, the daemon's
//! configuration as any daemon's is. A directory without `instance.toml`
//! is not a definition. The `kind` leaves room for an instance on another
//! machine to be defined in the same place and namespace.

use std::path::{Path, PathBuf};

use crate::locations::INSTANCES_DIR;

/// The file that makes a directory a definition.
pub const DEFINITION_FILE: &str = "instance.toml";

/// What `instance.toml` says.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionFile {
    /// What the definition is: `daemon`, a daemon on this machine whose
    /// locations follow from the name.
    pub kind: String,
    /// What the instance is for, as the user describes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One named instance in the catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// Its name.
    pub name: String,
    /// What `instance.toml` says.
    pub file: DefinitionFile,
    /// Its directory, `CONFIG/instances/NAME`.
    pub directory: PathBuf,
}

impl Definition {
    /// The daemon's configuration file, beside `instance.toml`.
    pub fn config(&self) -> PathBuf {
        self.directory.join("muru.toml")
    }
}

/// The kinds of definition this version knows.
pub const KINDS: [&str; 1] = ["daemon"];

/// The definitions under the configuration area `area`, with a warning
/// for each directory that is not one: no `instance.toml`, one that does
/// not parse, a kind this version does not know, or a name that is not
/// an instance name.
pub fn read(area: &Path) -> (Vec<Definition>, Vec<String>) {
    let root = area.join(INSTANCES_DIR);
    let mut definitions = Vec::new();
    let mut warnings = Vec::new();

    let Ok(entries) = std::fs::read_dir(&root) else {
        return (definitions, warnings);
    };

    for entry in entries.flatten() {
        let directory = entry.path();

        if !directory.is_dir() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        let path = directory.join(DEFINITION_FILE);

        let Ok(text) = std::fs::read_to_string(&path) else {
            warnings.push(format!(
                "{} has no {DEFINITION_FILE}, so it is not an instance definition",
                directory.display()
            ));
            continue;
        };

        if let Err(error) = crate::locations::check_name(&name) {
            warnings.push(format!("{}: {error}", directory.display()));
            continue;
        }

        match toml::from_str::<DefinitionFile>(&text) {
            Ok(file) if KINDS.contains(&file.kind.as_str()) => definitions.push(Definition {
                name,
                file,
                directory,
            }),
            Ok(file) => warnings.push(format!(
                "{}: kind {:?} is not a kind of instance this version knows ({})",
                path.display(),
                file.kind,
                KINDS.join(", ")
            )),
            Err(error) => warnings.push(format!("{}: {}", path.display(), error.message())),
        }
    }

    definitions.sort_by(|a, b| a.name.cmp(&b.name));
    (definitions, warnings)
}

/// The definition named `name` under `area`.
pub fn find(area: &Path, name: &str) -> Option<Definition> {
    read(area)
        .0
        .into_iter()
        .find(|definition| definition.name == name)
}

/// Write a new daemon definition: `instance.toml` and the daemon's
/// `muru.toml` with `config`.
pub fn create(
    area: &Path,
    name: &str,
    description: Option<&str>,
    config: &str,
) -> std::io::Result<Definition> {
    let directory = area.join(INSTANCES_DIR).join(name);
    std::fs::create_dir_all(directory.parent().unwrap_or(area))?;
    std::fs::create_dir(&directory)?;

    let file = DefinitionFile {
        kind: "daemon".to_owned(),
        description: description.map(str::to_owned),
    };
    let written = toml::to_string(&file).map_err(std::io::Error::other)?;
    std::fs::write(directory.join(DEFINITION_FILE), written)?;
    std::fs::write(directory.join("muru.toml"), config)?;

    Ok(Definition {
        name: name.to_owned(),
        file,
        directory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_definition_is_a_directory_with_its_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let area = directory.path();

        create(
            area,
            "dev",
            Some("for trying things"),
            "[[listener]]\naddress = '127.0.0.1:9000'\n",
        )
        .expect("created");
        std::fs::create_dir_all(area.join("instances/loose")).expect("made");
        std::fs::create_dir_all(area.join("instances/remote")).expect("made");
        std::fs::write(
            area.join("instances/remote/instance.toml"),
            "kind = 'remote'\n",
        )
        .expect("written");

        let (definitions, warnings) = read(area);
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "dev");
        assert_eq!(
            definitions[0].file.description.as_deref(),
            Some("for trying things")
        );
        assert!(definitions[0].config().is_file());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("has no instance.toml"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("kind \"remote\""))
        );

        assert!(find(area, "dev").is_some());
        assert!(create(area, "dev", None, "").is_err());
    }
}
