//! Where `muructl run`, `muructl serve` and a project's own commands put
//! an application's places (design specification section 13.8, issues
//! 849 and 850): its data directory, with the media and uploads parts,
//! and its directory of built static files.
//!
//! A development run keeps both in the user state location, under
//! `runs/NAME-HASH`, never in the project, which is usually a git
//! checkout. `muructl serve` has a data directory only when one is named,
//! normally a mounted volume, and its static directory is `staticfiles`
//! in the project, filled when the image is built. A command run on its
//! own uses the run's, unless serve's variables name others, so a
//! migration run beside `muructl run` migrates the database it serves.

use std::path::{Path, PathBuf};

use muru_config::places::{DataDirs, Place};
use muru_controller::model::Within;
use muru_controller::places::Places;

/// The hex SHA-256 of a project's resolved path, which keeps two checkouts
/// of one project apart.
pub fn project_hash(project: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;

    let resolved = project
        .canonicalize()
        .unwrap_or_else(|_| project.to_path_buf());

    ring::digest::digest(&ring::digest::SHA256, resolved.as_os_str().as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The directory a development run keeps a project's places in.
pub fn run_location(project: &Path, name: &str) -> PathBuf {
    muru_config::places::run_places(
        &muru_controller::locations::user_state(),
        name,
        &project_hash(project),
    )
}

/// A development run's places: the data directory `data_dir` names, a
/// relative one taken from the project, else `data` in the run's
/// location, and `static` there.
pub fn for_run(project: &Path, name: &str, data_dir: Option<&Path>) -> Places {
    let location = run_location(project, name);

    let data = match data_dir {
        Some(dir) if dir.is_absolute() => dir.to_path_buf(),
        Some(dir) => project.join(dir),
        None => location.join("data"),
    };

    Places {
        data: Some(DataDirs::new(data, None, None)),
        static_dir: Some(location.join("static")),
        owner: None,
    }
}

/// What `muructl serve` was told of the places: each setting as given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeDirs {
    /// `--data-dir`, `MURU_SERVE_DATA_DIR` or `data_dir`.
    pub data: Option<String>,
    /// `--media-dir`, a relative one taken from the data directory.
    pub media: Option<String>,
    /// `--uploads-dir`, a relative one taken from the data directory.
    pub uploads: Option<String>,
    /// `--static-dir`, a relative one taken from the project.
    pub static_dir: Option<String>,
}

/// `muructl serve`'s places: a data directory only when one is named,
/// its parts within it unless named, and the static directory named or
/// `staticfiles` in the project.
pub fn for_serve(project: &Path, dirs: &ServeDirs) -> anyhow::Result<Places> {
    let data = match &dirs.data {
        Some(data) => {
            let data = Path::new(data);

            let data = if data.is_absolute() {
                data.to_path_buf()
            } else {
                project.join(data)
            };

            Some(DataDirs::new(
                data,
                dirs.media.as_deref(),
                dirs.uploads.as_deref(),
            ))
        }
        None => {
            for (flag, given) in [
                ("--media-dir", &dirs.media),
                ("--uploads-dir", &dirs.uploads),
            ] {
                anyhow::ensure!(
                    given.is_none(),
                    "{flag} names a part of the data directory, and there is none; name it with \
                     --data-dir"
                );
            }

            None
        }
    };

    let static_dir = Path::new(
        dirs.static_dir
            .as_deref()
            .unwrap_or(muru_config::places::SERVE_STATIC_DIR),
    );

    Ok(Places {
        data,
        static_dir: Some(if static_dir.is_absolute() {
            static_dir.to_path_buf()
        } else {
            project.join(static_dir)
        }),
        owner: None,
    })
}

/// `muructl serve`'s data directory must be there and writable, since it
/// is a volume the container is given: Muru makes the parts in it, not
/// the directory itself.
pub fn check_volume(places: &Places) -> anyhow::Result<()> {
    let Some(dirs) = &places.data else {
        return Ok(());
    };

    let data = &dirs.data;
    anyhow::ensure!(
        data.is_dir(),
        "the data directory {} does not exist; mount a volume there (a Docker volume, or a \
         Kubernetes PersistentVolumeClaim), or leave --data-dir out",
        data.display()
    );

    let probe = data.join(format!(".muru-write-check-{}", std::process::id()));

    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(error) => anyhow::bail!(
            "the data directory {} cannot be written ({error}); mount a volume there that the \
             container's user can write, or leave --data-dir out",
            data.display()
        ),
    }
}

/// Make what is missing of the places: the data directory and its parts
/// with `mode`, and the static directory.
pub fn make(places: &Places, mode: u32) -> anyhow::Result<()> {
    if let Some(dirs) = &places.data {
        muru_controller::places::make_data(dirs, None, mode).map_err(anyhow::Error::msg)?;
    }

    if let Some(dir) = &places.static_dir {
        std::fs::create_dir_all(dir).map_err(|error| {
            anyhow::anyhow!(
                "making the static directory {} failed: {error}",
                dir.display()
            )
        })?;
    }

    Ok(())
}

/// The places' lines at start-up: each part, where a link leads, and
/// which is a file hint root.
pub fn lines(places: &Places) -> Vec<String> {
    let mut lines = Vec::new();
    let shown = |path: &Path| match muru_controller::places::link_target(path) {
        Some(resolved) => format!("{} -> {}", path.display(), resolved.display()),
        None => path.display().to_string(),
    };

    match &places.data {
        Some(dirs) => {
            lines.push(format!("data     {}", shown(&dirs.data)));
            lines.push(format!("media    {}, a file hint root", shown(&dirs.media)));
            lines.push(format!("uploads  {}", shown(&dirs.uploads)));
        }
        None => lines.push(
            "data     none; --data-dir or MURU_SERVE_DATA_DIR names one, normally a volume"
                .to_owned(),
        ),
    }

    if let Some(dir) = &places.static_dir {
        lines.push(format!("static   {}", shown(dir)));
    }

    lines
}

/// A static entry's directory, resolved against the places: a relative
/// path from the project, `media:` and `static:` from what they name,
/// with which of the two it was.
pub fn static_directory(
    dir: &str,
    project: &Path,
    places: &Places,
) -> anyhow::Result<(PathBuf, Option<Within>)> {
    let place = Place::parse(dir).map_err(anyhow::Error::msg)?;
    let resolved = place
        .resolve(project, &places.provided())
        .map_err(anyhow::Error::msg)?;

    let within = match place {
        Place::Path(_) => None,
        Place::Media(_) => Some(Within::Media),
        Place::Static(_) => Some(Within::Static),
    };

    Ok((resolved, within))
}

/// Whether a directory has nothing in it, or is not there.
pub fn is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).map_or(true, |mut entries| entries.next().is_none())
}

/// A command's places when it runs on its own, through no run or serve
/// (section 9.12): serve's variables, `MURU_SERVE_DATA_DIR` and the
/// rest, when they name a data directory or a static directory, else
/// where a development run of the project keeps them, with the data
/// directory its run table names, `data_dir` (issue 859).
pub fn for_command(
    project: &Path,
    name: &str,
    data_dir: Option<&Path>,
    read: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Places> {
    let serve = ServeDirs {
        data: read("MURU_SERVE_DATA_DIR"),
        media: read("MURU_SERVE_MEDIA_DIR"),
        uploads: read("MURU_SERVE_UPLOADS_DIR"),
        static_dir: read("MURU_SERVE_STATIC_DIR"),
    };
    let run = for_run(project, name, data_dir);
    let named = for_serve(project, &serve)?;

    Ok(Places {
        data: if serve.data.is_some() {
            named.data
        } else {
            run.data
        },
        static_dir: if serve.static_dir.is_some() {
            named.static_dir
        } else {
            run.static_dir
        },
        owner: None,
    })
}

/// The variables a process of the project is given for its places, and
/// those it must not inherit because it has no such place.
pub fn variables(places: &Places) -> (Vec<(String, String)>, Vec<String>) {
    let set = places.variables();
    let unset = muru_config::places::VARIABLES
        .iter()
        .filter(|name| !set.iter().any(|(key, _)| key == *name))
        .map(|name| (*name).to_owned())
        .collect();

    (set, unset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_keeps_its_places_by_the_projects_name_and_path() {
        let project = tempfile::tempdir().unwrap();
        let places = for_run(project.path(), "shop", None);
        let dirs = places.data.as_ref().unwrap();
        let location = run_location(project.path(), "shop");

        assert_eq!(dirs.data, location.join("data"));
        assert_eq!(dirs.media, location.join("data/media"));
        assert_eq!(
            places.static_dir.as_deref(),
            Some(&*location.join("static"))
        );
        assert!(
            !dirs.data.starts_with(project.path()),
            "never inside the project"
        );

        let name = location.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("shop-") && name.len() == "shop-".len() + 12,
            "{name}"
        );

        // Another checkout of the same project is kept apart.

        let other = tempfile::tempdir().unwrap();
        assert_ne!(run_location(other.path(), "shop"), location);

        // A data directory named is used, a relative one in the project.

        let moved = for_run(project.path(), "shop", Some(Path::new("var")));
        assert_eq!(moved.data.unwrap().data, project.path().join("var"),);
    }

    #[test]
    fn serve_has_a_data_directory_only_when_one_is_named() {
        let project = Path::new("/app");
        let none = for_serve(project, &ServeDirs::default()).unwrap();
        assert!(none.data.is_none());
        assert_eq!(
            none.static_dir.as_deref(),
            Some(Path::new("/app/staticfiles"))
        );
        assert_eq!(
            variables(&none).1,
            ["MURU_DATA_DIR", "MURU_MEDIA_DIR", "MURU_UPLOADS_DIR"]
        );

        let named = for_serve(
            project,
            &ServeDirs {
                data: Some("/data".to_owned()),
                media: Some("/shared/media".to_owned()),
                uploads: None,
                static_dir: Some("collected".to_owned()),
            },
        )
        .unwrap();
        let dirs = named.data.unwrap();
        assert_eq!(dirs.media, Path::new("/shared/media"));
        assert_eq!(dirs.uploads, Path::new("/data/uploads"));
        assert_eq!(
            named.static_dir.as_deref(),
            Some(Path::new("/app/collected"))
        );

        let refused = for_serve(
            project,
            &ServeDirs {
                media: Some("/shared/media".to_owned()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(refused.to_string().contains("--data-dir"), "{refused}");
    }

    #[test]
    fn a_volume_that_is_not_there_is_refused_naming_the_mount() {
        let missing = for_serve(
            Path::new("/app"),
            &ServeDirs {
                data: Some("/nowhere/muru-data".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let refused = check_volume(&missing).unwrap_err().to_string();
        assert!(refused.contains("mount a volume there"), "{refused}");

        let there = tempfile::tempdir().unwrap();
        let places = for_serve(
            Path::new("/app"),
            &ServeDirs {
                data: Some(there.path().display().to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(check_volume(&places).is_ok());
    }

    #[test]
    fn a_command_on_its_own_uses_the_runs_places_unless_serves_variables_name_others() {
        let project = tempfile::tempdir().unwrap();
        let run = for_run(project.path(), "shop", None);
        assert_eq!(
            for_command(project.path(), "shop", None, |_| None).unwrap(),
            run
        );

        // Issue 859: the run table's data directory, as the run takes it.

        let moved = for_command(project.path(), "shop", Some(Path::new("data")), |_| None).unwrap();
        assert_eq!(
            moved,
            for_run(project.path(), "shop", Some(Path::new("data")))
        );
        assert_eq!(moved.data.unwrap().data, project.path().join("data"));

        let named = for_command(project.path(), "shop", Some(Path::new("data")), |name| {
            (name == "MURU_SERVE_DATA_DIR").then(|| "/data".to_owned())
        })
        .unwrap();
        assert_eq!(named.data.unwrap().data, Path::new("/data"));
        assert_eq!(named.static_dir, run.static_dir);
    }
}
