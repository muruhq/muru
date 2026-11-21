//! The places Muru provides an application (design specification
//! sections 4.9, 8.7 and 13.8, issue 849, ADR 0072): its data directory
//! with the media and uploads parts, shared by every release and kept
//! when releases go.
//!
//! Under the daemon the data directory is `STATE/data/APP` unless the
//! application's `[app.NAME]` table moves it. `muructl run` and
//! `muructl serve` work out their own
//! locations and use the rest of this module as the daemon does: making
//! what is missing, checking what a release serves, and describing the
//! data directory for `muructl data show` and `muructl data check`.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use muru_config::places::{DataDirs, Provided};
use muru_proto::admin::v1 as pb;

use crate::model::ReleaseSpec;

/// The mode a data directory and its parts are made with.
pub const DATA_MODE: u32 = 0o700;

/// The places a release is given, worked out for a start or for the
/// edge's snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Places {
    /// The application's data directory, when it has one.
    pub data: Option<DataDirs>,
    /// The user the media part must belong to, under a daemon whose
    /// applications run as users of their own, so a link in its place
    /// cannot lead the edge or a worker into another user's directory.
    pub owner: Option<u32>,
}

impl Places {
    /// The places as `muru_config` resolves a `media:` directory against
    /// them.
    pub fn provided(&self) -> Provided<'_> {
        Provided {
            data: self.data.as_ref(),
        }
    }

    /// The media part, the one part ever served.
    pub fn media(&self) -> Option<&Path> {
        self.data.as_ref().map(|dirs| dirs.media.as_path())
    }

    /// The variables naming them, which the release's processes are
    /// given.
    pub fn variables(&self) -> Vec<(String, String)> {
        self.data
            .as_ref()
            .map(DataDirs::variables)
            .unwrap_or_default()
    }
}

/// An application's data directory under the daemon by default.
pub fn default_data_dir(state: &Path, app: &str) -> PathBuf {
    state.join("data").join(app)
}

/// An application's data directory and its parts under the daemon: its
/// `[app.NAME]` table's, else `STATE/data/APP` with `media` and
/// `uploads` in it.
pub fn app_data(state: &Path, app: &crate::model::App) -> DataDirs {
    DataDirs::new(
        app.data_dir
            .as_deref()
            .map_or_else(|| default_data_dir(state, &app.name), PathBuf::from),
        app.media_dir.as_deref(),
        app.uploads_dir.as_deref(),
    )
}

/// Make each part of the data directory that is missing, data first,
/// with `mode` and given to `owner` (a uid and gid), and leave what is
/// there as it is, whoever owns it: ownership is the operator's once a
/// part exists. A part that is a symbolic link counts as there.
pub fn make_data(dirs: &DataDirs, owner: Option<(u32, u32)>, mode: u32) -> Result<(), String> {
    for (name, path) in dirs.parts() {
        if path.symlink_metadata().is_ok() {
            continue;
        }

        let failed = |error: std::io::Error| {
            format!(
                "making the {name} directory {} failed: {error}",
                path.display()
            )
        };

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(failed)?;
        }

        match std::fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(failed(error)),
        }

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(failed)?;

        if let Some((uid, gid)) = owner {
            std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(failed)?;
        }
    }

    Ok(())
}

/// Check what a release would serve against its application's data
/// directory: the layout itself, every file hint root of every mount and
/// every static entry's directory, so only the media part is ever served.
pub fn check_release(spec: &ReleaseSpec, places: &Places) -> Result<(), String> {
    let Some(dirs) = &places.data else {
        return Ok(());
    };

    dirs.check()?;

    let mounts = std::iter::once(("", &spec.sendfile_roots)).chain(
        spec.mounts
            .iter()
            .map(|mount| (mount.name.as_str(), &mount.sendfile_roots)),
    );

    for (mount, roots) in mounts {
        let label = if mount.is_empty() {
            "sendfile_roots".to_owned()
        } else {
            format!("mount {mount}: sendfile_roots")
        };

        for root in roots {
            if let Some(path) = muru_config::sendfile_root(root, &spec.source, places.media()) {
                muru_config::places::check_served(&label, &path, dirs)?;
            }
        }
    }

    for entry in &spec.static_dirs {
        if let Some(dir) = entry.directory(places) {
            muru_config::places::check_served(&format!("static {}", entry.path), &dir, dirs)?;
        }
    }

    Ok(())
}

/// What a release's creation does with its places under the daemon:
/// what it serves is checked against its application's data directory,
/// as the application's table has it now.
pub fn prepare_release(
    state: &Path,
    store: &crate::store::Store,
    app: &str,
    _id: &str,
    spec: &ReleaseSpec,
) -> Result<(), String> {
    let places = Places {
        data: store.app(app).ok().map(|record| app_data(state, &record)),
        owner: None,
    };

    check_release(spec, &places)
}

/// Where a part that is itself a symbolic link leads, resolved; none for
/// a part that is not a link, whatever the directories above it are.
pub fn link_target(path: &Path) -> Option<PathBuf> {
    path.symlink_metadata()
        .ok()
        .filter(std::fs::Metadata::is_symlink)
        .map(|_| muru_config::places::resolve(path))
}

/// The file system types on which SQLite's locks and its WAL index do
/// not hold, by mountinfo's name (with a FUSE file system's subtype).
const NETWORK_FILESYSTEMS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "9p",
    "ceph",
    "glusterfs",
    "lustre",
    "gpfs",
    "afs",
    "beegfs",
    "fuse.sshfs",
    "fuse.glusterfs",
    "fuse.s3fs",
    "fuse.rclone",
    "fuse.juicefs",
    "fuse.gcsfuse",
    "fuse.mountpoint-s3",
    "fuse.cephfs",
    "fuse.grpcfuse",
];

/// Whether a file system type, as mountinfo names it, is one shared over
/// a network.
pub fn is_network(kind: &str) -> bool {
    NETWORK_FILESYSTEMS.contains(&kind)
}

/// What `muructl data show` and `muructl data check` say of an
/// application's data directory: each part where it is and leads, its
/// owner, mode and file system, whether it is a file hint root, what it
/// holds when `sizes` asks, whether the uploads part shares the upload
/// spool's file system, and the warnings. `none` says why there is no
/// data directory, when `dirs` is none.
pub fn describe(
    app: &str,
    dirs: Option<&DataDirs>,
    none: &str,
    expected_uid: u32,
    spool: Option<&Path>,
    sizes: bool,
) -> pb::AppData {
    let mut answer = pb::AppData {
        app: app.to_owned(),
        expected_uid,
        expected_owner: crate::processes::user_name(expected_uid),
        spool: spool
            .map(|spool| spool.display().to_string())
            .unwrap_or_default(),
        ..Default::default()
    };

    let Some(dirs) = dirs else {
        none.clone_into(&mut answer.none);
        return answer;
    };

    for (name, path) in dirs.parts() {
        let metadata = std::fs::metadata(path).ok();
        let kind = muru_common::mounts::mount_of(path)
            .map(|mount| mount.kind)
            .unwrap_or_default();

        answer.parts.push(pb::DataPart {
            name: name.to_owned(),
            path: path.display().to_string(),
            resolved: link_target(path)
                .map(|resolved| resolved.display().to_string())
                .unwrap_or_default(),
            exists: metadata.as_ref().is_some_and(std::fs::Metadata::is_dir),
            owner_uid: metadata.as_ref().map_or(0, |metadata| metadata.uid()),
            owner: metadata
                .as_ref()
                .map(|metadata| crate::processes::user_name(metadata.uid()))
                .unwrap_or_default(),
            mode: metadata
                .as_ref()
                .map_or(0, |metadata| metadata.mode() & 0o7777),
            network: is_network(&kind),
            filesystem: kind,
            root: name == "media",
            bytes: sizes.then(|| {
                let mut measure = crate::disk::Measure::default();
                measure.add(path);
                measure.total().used
            }),
        });
    }

    // The uploads part shares the spool's file system when the two are
    // on one device, and upload.save() then links rather than copies.

    let device = |path: &Path| std::fs::metadata(path).ok().map(|metadata| metadata.dev());
    answer.spool_shared = match (spool.and_then(device), device(&dirs.uploads)) {
        (Some(spool), Some(uploads)) => spool == uploads,
        _ => false,
    };

    answer.warnings = warnings(dirs, &answer);
    answer
}

/// What `muructl data check` reports, from a description: a database
/// file in media, where a file hint could send it, a part on a network
/// file system, and a part that is not the application's user's.
fn warnings(dirs: &DataDirs, described: &pb::AppData) -> Vec<String> {
    let mut warnings = Vec::new();

    for part in &described.parts {
        if !part.exists {
            continue;
        }

        if part.network {
            warnings.push(format!(
                "the {} directory {} is on {}, a network file system, where SQLite's locks and \
                 its WAL index do not hold; keep a database on a local file system",
                part.name, part.path, part.filesystem
            ));
        }

        if part.owner_uid != described.expected_uid {
            let owner = if part.owner.is_empty() {
                format!("uid {}", part.owner_uid)
            } else {
                part.owner.clone()
            };

            let expected = if described.expected_owner.is_empty() {
                format!("uid {}", described.expected_uid)
            } else {
                described.expected_owner.clone()
            };

            warnings.push(format!(
                "the {} directory {} belongs to {owner}, and the application runs as {expected}",
                part.name, part.path
            ));
        }
    }

    for database in databases(&dirs.media) {
        warnings.push(format!(
            "{} is a database file in the media part, which file hints and static entries \
             serve; keep it in the data directory",
            database.display()
        ));
    }

    warnings
}

/// The database files beneath `dir`, without following symbolic links:
/// `*.db`, `*.sqlite`, `*.sqlite3`, and a `-wal` file.
fn databases(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];

    // The part itself may be a link, which is followed; what is beneath
    // it is not.

    let mut first = true;

    while let Some(directory) = pending.pop() {
        let entries = if first {
            first = false;
            std::fs::read_dir(&directory)
        } else {
            match directory.symlink_metadata() {
                Ok(metadata) if metadata.is_dir() => std::fs::read_dir(&directory),
                _ => continue,
            }
        };

        let Ok(entries) = entries else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };

            if kind.is_dir() {
                pending.push(path);
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let database = [".db", ".sqlite", ".sqlite3", "-wal"]
                .iter()
                .any(|suffix| name.ends_with(suffix));

            if kind.is_file() && database {
                found.push(path);
            }
        }
    }

    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muru-places-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_data_directory_is_made_with_its_parts_and_what_is_there_is_left() {
        let base = scratch("make");
        let dirs = DataDirs::new(base.join("data/shop"), None, None);
        make_data(&dirs, None, DATA_MODE).unwrap();

        for (_, path) in dirs.parts() {
            let metadata = std::fs::metadata(path).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.mode() & 0o777, 0o700, "{}", path.display());
        }

        // A part already there, a link to another disk among them, is
        // left as it is.

        std::fs::remove_dir(&dirs.media).unwrap();
        std::fs::create_dir(base.join("disk")).unwrap();
        std::fs::set_permissions(base.join("disk"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink(base.join("disk"), &dirs.media).unwrap();
        make_data(&dirs, None, DATA_MODE).unwrap();
        assert!(dirs.media.symlink_metadata().unwrap().is_symlink());
        assert_eq!(
            std::fs::metadata(base.join("disk")).unwrap().mode() & 0o777,
            0o755
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_variables_name_each_place_the_release_has() {
        let places = Places {
            data: Some(DataDirs::new(PathBuf::from("/srv/data/shop"), None, None)),
            owner: None,
        };
        let names: Vec<String> = places
            .variables()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            ["MURU_DATA_DIR", "MURU_MEDIA_DIR", "MURU_UPLOADS_DIR"]
        );
        assert!(Places::default().variables().is_empty());
    }

    #[test]
    fn a_description_warns_of_a_database_in_media_and_a_part_of_another_user() {
        let base = scratch("describe");
        let dirs = DataDirs::new(base.join("data"), None, None);
        make_data(&dirs, None, DATA_MODE).unwrap();
        std::fs::create_dir_all(dirs.media.join("exports")).unwrap();
        std::fs::write(dirs.media.join("exports/copy.sqlite3"), "x").unwrap();
        std::fs::write(dirs.media.join("report.pdf"), "x").unwrap();
        std::fs::write(dirs.data.join("app.db"), "x").unwrap();
        let owner = std::fs::metadata(&base).unwrap().uid();

        let described = describe("shop", Some(&dirs), "", owner, Some(&dirs.data), true);
        let names: Vec<&str> = described
            .parts
            .iter()
            .map(|part| part.name.as_str())
            .collect();
        assert_eq!(names, ["data", "media", "uploads"]);
        assert!(
            described
                .parts
                .iter()
                .all(|part| part.exists && part.mode == 0o700)
        );
        assert!(described.parts[1].root && !described.parts[0].root);
        assert!(described.parts[0].bytes.is_some());
        assert!(described.spool_shared);
        assert_eq!(described.warnings.len(), 1, "{:?}", described.warnings);
        assert!(described.warnings[0].contains("exports/copy.sqlite3"));

        let other = describe("shop", Some(&dirs), "", owner + 1, None, false);
        assert!(other.parts[0].bytes.is_none());
        assert_eq!(
            other
                .warnings
                .iter()
                .filter(|warning| warning.contains("and the application runs as"))
                .count(),
            3
        );

        let none = describe(
            "shop",
            None,
            "no data directory was named",
            owner,
            None,
            false,
        );
        assert!(none.parts.is_empty());
        assert_eq!(none.none, "no data directory was named");

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn network_file_systems_are_known_by_name() {
        assert!(is_network("nfs4"));
        assert!(is_network("fuse.sshfs"));
        assert!(!is_network("ext4"));
        assert!(!is_network("tmpfs"));
    }
}
