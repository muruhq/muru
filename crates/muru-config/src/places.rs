//! The places Muru provides an application (design specification
//! sections 4.9 and 13.8, issues 849 and 850, ADRs 0072 and 0073): its
//! data directory, shared by every release, with the media and uploads
//! parts Muru makes in it, and each release's own directory of built
//! static files; and the `media:` and `static:` forms that name them in
//! `sendfile_roots`, a static entry's `dir` and `--static`, beside
//! `source:` for the release's own source (issue 900).
//!
//! What may be served is checked here too: only media is ever served,
//! so the data directory, the uploads part and any directory containing
//! either can never be a file hint root or a static directory.

use std::path::{Component, Path, PathBuf};

/// The variable naming the application's data directory.
pub const DATA_VARIABLE: &str = "MURU_DATA_DIR";
/// The variable naming the media part, the one part ever served.
pub const MEDIA_VARIABLE: &str = "MURU_MEDIA_DIR";
/// The variable naming the uploads part, kept but never served.
pub const UPLOADS_VARIABLE: &str = "MURU_UPLOADS_DIR";
/// The variable naming the release's directory of built static files.
pub const STATIC_VARIABLE: &str = "MURU_STATIC_DIR";

/// Every variable of this module, which Muru alone sets: a value from
/// the release's own environment or an inherited one is replaced, or
/// removed where Muru has none to give.
pub const VARIABLES: [&str; 4] = [
    DATA_VARIABLE,
    MEDIA_VARIABLE,
    UPLOADS_VARIABLE,
    STATIC_VARIABLE,
];

/// The media part's name in the data directory by default.
pub const MEDIA_PART: &str = "media";
/// The uploads part's name in the data directory by default.
pub const UPLOADS_PART: &str = "uploads";
/// The directory under the source `muructl serve` gives as the static
/// directory by default.
pub const SERVE_STATIC_DIR: &str = "staticfiles";

/// Where a refusal of a directory sends the reader.
pub const FILE_HINTS_GUIDE: &str =
    "https://docs.muruhq.dev/en/latest/howto/file-hints.html#what-a-hint-may-name";

/// An application's data directory and its two parts, each absolute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirs {
    /// `MURU_DATA_DIR`: never served.
    pub data: PathBuf,
    /// `MURU_MEDIA_DIR`: a file hint root by default.
    pub media: PathBuf,
    /// `MURU_UPLOADS_DIR`: never served.
    pub uploads: PathBuf,
}

impl DataDirs {
    /// The parts of `data`: a relative `media` or `uploads` is taken from
    /// the data directory, and each defaults to its name there.
    pub fn new(data: PathBuf, media: Option<&str>, uploads: Option<&str>) -> Self {
        let part = |given: Option<&str>, default: &str| {
            let given = Path::new(given.filter(|text| !text.is_empty()).unwrap_or(default));

            if given.is_absolute() {
                given.to_path_buf()
            } else {
                data.join(given)
            }
        };

        Self {
            media: part(media, MEDIA_PART),
            uploads: part(uploads, UPLOADS_PART),
            data,
        }
    }

    /// The three variables, in the order the table of the guide gives
    /// them.
    pub fn variables(&self) -> Vec<(String, String)> {
        vec![
            (DATA_VARIABLE.to_owned(), self.data.display().to_string()),
            (MEDIA_VARIABLE.to_owned(), self.media.display().to_string()),
            (
                UPLOADS_VARIABLE.to_owned(),
                self.uploads.display().to_string(),
            ),
        ]
    }

    /// Each part with its name, data first.
    pub fn parts(&self) -> [(&'static str, &Path); 3] {
        [
            ("data", &self.data),
            ("media", &self.media),
            ("uploads", &self.uploads),
        ]
    }

    /// Check the layout: the media part, the one served, may contain
    /// neither the data directory nor the uploads part, and no two parts
    /// are one directory. Checked on the paths as given and as they
    /// resolve through symbolic links.
    pub fn check(&self) -> Result<(), String> {
        for dirs in [self.clone(), self.resolved()] {
            if dirs.media == dirs.data || dirs.media == dirs.uploads || dirs.data == dirs.uploads {
                return Err(format!(
                    "the data directory {}, its media part {} and its uploads part {} must be \
                     three directories",
                    self.data.display(),
                    self.media.display(),
                    self.uploads.display()
                ));
            }

            if within(&dirs.data, &dirs.media) {
                return Err(format!(
                    "the media part {} contains the data directory {}, which is never served \
                     (see {FILE_HINTS_GUIDE})",
                    self.media.display(),
                    self.data.display()
                ));
            }

            if within(&dirs.uploads, &dirs.media) {
                return Err(format!(
                    "the media part {} contains the uploads part {}, which is never served \
                     (see {FILE_HINTS_GUIDE})",
                    self.media.display(),
                    self.uploads.display()
                ));
            }
        }

        Ok(())
    }

    /// The parts as they resolve through symbolic links, as far as they
    /// exist.
    pub fn resolved(&self) -> Self {
        Self {
            data: resolve(&self.data),
            media: resolve(&self.media),
            uploads: resolve(&self.uploads),
        }
    }
}

/// What a directory in configuration names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place<'a> {
    /// A directory by its path: absolute, or relative to the source.
    Path(&'a str),
    /// `media:` or `media:SUBDIR`: the media part, or a directory in it.
    Media(&'a str),
    /// `static:` or `static:SUBDIR`: the release's directory of built
    /// static files, or a directory in it.
    Static(&'a str),
    /// `source:` or `source:SUBDIR`: the release's source directory, or a
    /// directory in it (issue 900), which a relative path names too.
    Source(&'a str),
}

/// The places a release is given, for resolving a [`Place`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Provided<'a> {
    /// The application's data directory, when it has one.
    pub data: Option<&'a DataDirs>,
    /// The release's directory of built static files, when it has one.
    pub static_dir: Option<&'a Path>,
}

impl<'a> Place<'a> {
    /// Read a directory as configuration writes it. `data:` and
    /// `uploads:` are refused, saying why; any other text is a path, so
    /// a directory whose name starts with `media:` is written
    /// `./media:x`. A relative path stays a path, though it names a
    /// directory in the source as `source:` does: see [`Place::in_source`].
    pub fn parse(text: &'a str) -> Result<Self, String> {
        let prefixed = |prefix: &str| text.strip_prefix(prefix);

        if prefixed("data:").is_some() {
            return Err(format!(
                "{text:?}: the data directory is never served; keep files to serve in media: \
                 (see {FILE_HINTS_GUIDE})"
            ));
        }

        if prefixed("uploads:").is_some() {
            return Err(format!(
                "{text:?}: the uploads part holds files kept but never served; save files to \
                 serve in media: (see {FILE_HINTS_GUIDE})"
            ));
        }

        let (place, rest) = if let Some(rest) = prefixed("media:") {
            (Self::Media(rest), rest)
        } else if let Some(rest) = prefixed("static:") {
            (Self::Static(rest), rest)
        } else if let Some(rest) = prefixed("source:") {
            (Self::Source(rest), rest)
        } else {
            return Ok(Self::Path(text));
        };

        let climbs = Path::new(rest)
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir));

        if climbs {
            return Err(format!(
                "{text:?}: what follows the prefix is a directory inside it, with no .. and \
                 not absolute"
            ));
        }

        Ok(place)
    }

    /// Whether the place is one Muru provides rather than a path.
    pub fn is_provided(self) -> bool {
        matches!(self, Self::Media(_) | Self::Static(_))
    }

    /// The directory in the source a place names, by its path relative to
    /// the source: `source:SUBDIR`'s, or a relative path's, which means
    /// the same (issue 900). None for an absolute path or a place Muru
    /// provides.
    pub fn in_source(self) -> Option<&'a str> {
        match self {
            Self::Source(rest) => Some(rest),
            Self::Path(text) if !Path::new(text).is_absolute() => Some(text),
            _ => None,
        }
    }

    /// The directory, absolute: a relative path is taken from `source`;
    /// a provided place from what `provided` holds, or why it cannot be.
    pub fn resolve(self, source: &Path, provided: &Provided<'_>) -> Result<PathBuf, String> {
        let under = |base: &Path, rest: &str| {
            if rest.is_empty() {
                base.to_path_buf()
            } else {
                base.join(rest)
            }
        };

        match self {
            Self::Path(text) if Path::new(text).is_absolute() => Ok(PathBuf::from(text)),
            Self::Path(text) => Ok(source.join(text)),
            Self::Source(rest) => Ok(under(source, rest)),
            Self::Media(rest) => provided
                .data
                .map(|dirs| under(&dirs.media, rest))
                .ok_or_else(|| {
                    "media: names the application's media part, and it has no data directory \
                     here; under muructl serve, name one with --data-dir"
                        .to_owned()
                }),
            Self::Static(rest) => {
                provided
                    .static_dir
                    .map(|dir| under(dir, rest))
                    .ok_or_else(|| {
                        "static: names the release's directory of built static files, which it \
                     does not have here"
                            .to_owned()
                    })
            }
        }
    }
}

/// Check a directory that would be served, by a static entry or as a
/// file hint root, against the application's data directory: the data
/// directory, anything in it but the media part, the uploads part and
/// any directory containing either is refused. Checked on the paths as
/// given and as they resolve through symbolic links.
pub fn check_served(what: &str, path: &Path, dirs: &DataDirs) -> Result<(), String> {
    let given = (normal(path), normal_dirs(dirs));
    let resolved = (resolve(path), dirs.resolved());

    for (path, dirs) in [&given, &resolved] {
        let refusal = if path == &dirs.data {
            Some("is the data directory")
        } else if path == &dirs.uploads {
            Some("is the uploads part")
        } else if within(&dirs.data, path) {
            Some("contains the data directory")
        } else if within(&dirs.uploads, path) {
            Some("contains the uploads part")
        } else if within(path, &dirs.uploads) {
            Some("is in the uploads part")
        } else if within(path, &dirs.data) && !within(path, &dirs.media) {
            Some("is in the data directory outside its media part")
        } else {
            None
        };

        if let Some(refusal) = refusal {
            return Err(format!(
                "{what}: {} {refusal}, which is never served; only the media part is (see \
                 {FILE_HINTS_GUIDE})",
                given.0.display()
            ));
        }
    }

    Ok(())
}

/// Whether `inner` is `outer` or lies beneath it, by components.
fn within(inner: &Path, outer: &Path) -> bool {
    inner.starts_with(outer)
}

fn normal_dirs(dirs: &DataDirs) -> DataDirs {
    DataDirs {
        data: normal(&dirs.data),
        media: normal(&dirs.media),
        uploads: normal(&dirs.uploads),
    }
}

/// A path with `.` removed and `..` taken back a component, without
/// looking at the file system.
fn normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }

    out
}

/// A path resolved through symbolic links as far as it exists, the rest
/// added as written.
pub fn resolve(path: &Path) -> PathBuf {
    let path = normal(path);
    let mut existing = path.as_path();
    let mut rest = Vec::new();

    loop {
        if let Ok(resolved) = std::fs::canonicalize(existing) {
            let mut resolved = resolved;

            for name in rest.iter().rev() {
                resolved.push(name);
            }

            return resolved;
        }

        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path,
        }
    }
}

/// The directory `muructl run` keeps an application's places in, under
/// the user state location: `runs/NAME-HASH`, the hash from the
/// project's resolved path, so two checkouts of one project do not
/// share a database. `hash` is the hex digest of that path.
pub fn run_places(user_state: &Path, name: &str, hash: &str) -> PathBuf {
    let short = &hash[..hash.len().min(12)];
    user_state.join("runs").join(format!("{name}-{short}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> DataDirs {
        DataDirs::new(PathBuf::from("/srv/state/data/shop"), None, None)
    }

    #[test]
    fn the_parts_default_to_their_names_and_relative_ones_are_taken_from_the_data_directory() {
        let dirs = dirs();
        assert_eq!(dirs.media, Path::new("/srv/state/data/shop/media"));
        assert_eq!(dirs.uploads, Path::new("/srv/state/data/shop/uploads"));

        let moved = DataDirs::new(PathBuf::from("/srv/shop"), Some("/mnt/media"), Some("in"));
        assert_eq!(moved.media, Path::new("/mnt/media"));
        assert_eq!(moved.uploads, Path::new("/srv/shop/in"));
        assert_eq!(
            moved.variables(),
            [
                ("MURU_DATA_DIR".to_owned(), "/srv/shop".to_owned()),
                ("MURU_MEDIA_DIR".to_owned(), "/mnt/media".to_owned()),
                ("MURU_UPLOADS_DIR".to_owned(), "/srv/shop/in".to_owned()),
            ]
        );
        assert!(moved.check().is_ok());
    }

    #[test]
    fn a_media_part_containing_the_data_directory_or_uploads_is_refused() {
        let around = DataDirs::new(PathBuf::from("/srv/shop/data"), Some("/srv/shop"), None);
        assert!(
            around
                .check()
                .unwrap_err()
                .contains("contains the data directory")
        );

        let uploads = DataDirs::new(PathBuf::from("/srv/shop"), None, Some("media/in"));
        assert!(
            uploads
                .check()
                .unwrap_err()
                .contains("contains the uploads part")
        );

        let same = DataDirs::new(PathBuf::from("/srv/shop"), Some("files"), Some("files"));
        assert!(same.check().unwrap_err().contains("three directories"));
    }

    #[test]
    fn places_are_read_from_their_prefixes_and_data_and_uploads_are_refused() {
        assert_eq!(Place::parse("media:"), Ok(Place::Media("")));
        assert_eq!(Place::parse("media:public"), Ok(Place::Media("public")));
        assert_eq!(Place::parse("static:"), Ok(Place::Static("")));
        assert_eq!(Place::parse("static:admin"), Ok(Place::Static("admin")));
        assert_eq!(Place::parse("./media:x"), Ok(Place::Path("./media:x")));
        assert_eq!(Place::parse("source:"), Ok(Place::Source("")));
        assert_eq!(Place::parse("source:assets"), Ok(Place::Source("assets")));
        assert!(Place::parse("source:../other").is_err());
        assert_eq!(
            Place::parse("source:assets").unwrap().in_source(),
            Some("assets")
        );
        assert_eq!(Place::parse("assets").unwrap().in_source(), Some("assets"));
        assert_eq!(Place::parse("/srv/assets").unwrap().in_source(), None);
        assert_eq!(Place::parse("media:").unwrap().in_source(), None);
        assert_eq!(
            Place::parse("source:assets")
                .unwrap()
                .resolve(Path::new("/srv/shop"), &Provided::default()),
            Ok(PathBuf::from("/srv/shop/assets"))
        );
        assert_eq!(Place::parse("shop/static"), Ok(Place::Path("shop/static")));
        assert!(Place::parse("data:").unwrap_err().contains("never served"));
        assert!(
            Place::parse("uploads:x")
                .unwrap_err()
                .contains("never served")
        );
        assert!(Place::parse("media:../x").unwrap_err().contains("no .."));
        assert!(Place::parse("media:/etc").unwrap_err().contains("no .."));
    }

    #[test]
    fn a_place_resolves_to_its_directory_or_says_why_it_has_none() {
        let dirs = dirs();
        let static_dir = PathBuf::from("/srv/state/static/shop/r1");
        let provided = Provided {
            data: Some(&dirs),
            static_dir: Some(&static_dir),
        };
        let source = Path::new("/src/shop");
        let resolve = |text: &str, provided: &Provided<'_>| {
            Place::parse(text).unwrap().resolve(source, provided)
        };

        assert_eq!(
            resolve("media:public", &provided).unwrap(),
            Path::new("/srv/state/data/shop/media/public")
        );
        assert_eq!(resolve("static:", &provided).unwrap(), static_dir);
        assert_eq!(
            resolve("assets", &provided).unwrap(),
            Path::new("/src/shop/assets")
        );
        assert_eq!(
            resolve("/srv/files", &provided).unwrap(),
            Path::new("/srv/files")
        );

        let none = Provided::default();
        assert!(resolve("media:", &none).unwrap_err().contains("--data-dir"));
        assert!(resolve("static:", &none).is_err());
    }

    #[test]
    fn only_media_is_served() {
        let dirs = dirs();
        let check = |path: &str| check_served("static /files/", Path::new(path), &dirs);

        assert!(check("/srv/state/data/shop/media").is_ok());
        assert!(check("/srv/state/data/shop/media/public").is_ok());
        assert!(check("/srv/other").is_ok());

        for (path, why) in [
            ("/srv/state/data/shop", "is the data directory"),
            ("/srv/state", "contains the data directory"),
            ("/srv/state/data/shop/uploads", "is the uploads part"),
            ("/srv/state/data/shop/uploads/x", "is in the uploads part"),
            ("/srv/state/data/shop/cache", "outside its media part"),
            ("/srv/state/data/shop/media/../db", "outside its media part"),
        ] {
            let refused = check(path).unwrap_err();
            assert!(refused.contains(why), "{path}: {refused}");
            assert!(refused.contains(FILE_HINTS_GUIDE), "{refused}");
        }

        let uploads_apart = DataDirs::new(PathBuf::from("/srv/shop"), None, Some("/srv/in"));
        assert!(
            check_served("root", Path::new("/srv"), &uploads_apart)
                .unwrap_err()
                .contains("contains the data directory")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_leads_into_the_data_directory_through_a_link_is_refused() {
        let base = std::env::temp_dir().join(format!("muru-places-{}", std::process::id()));
        let data = base.join("data");
        std::fs::create_dir_all(data.join("media")).unwrap();
        std::os::unix::fs::symlink(&data, base.join("link")).unwrap();
        let dirs = DataDirs::new(data.clone(), None, None);

        let refused = check_served("root", &base.join("link"), &dirs).unwrap_err();
        assert!(refused.contains("is the data directory"), "{refused}");
        assert!(check_served("root", &base.join("link/media"), &dirs).is_ok());

        // A media part moved behind a link to the uploads part is refused.

        std::fs::remove_dir(data.join("media")).unwrap();
        std::fs::create_dir_all(data.join("uploads")).unwrap();
        std::os::unix::fs::symlink(data.join("uploads"), data.join("media")).unwrap();
        assert!(dirs.check().is_err());

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_run_keeps_its_places_by_name_and_a_short_hash() {
        assert_eq!(
            run_places(
                Path::new("/home/me/.local/state/muru"),
                "shop",
                "3f9a1c2e7b40aa11"
            ),
            Path::new("/home/me/.local/state/muru/runs/shop-3f9a1c2e7b40")
        );
    }
}
