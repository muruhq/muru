//! The state store: SQLite in WAL mode in the state directory, holding
//! desired state, release records and events (design specification section
//! 10.2). Every access goes through one connection behind a mutex; the
//! store is small and the reconciler is the only writer.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use muru_config::{ClientClass, Host as VirtualHost, Overlay, RateLimit, Route, Upstream};

use crate::model::{
    App, CommandRun, Event, OverlayState, Release, ReleaseSpec, ReleaseState, Rollout, RunRecord,
    now_unix_ms,
};

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite said no.
    #[error("state store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A record could not be decoded.
    #[error("state store: {0}")]
    Decode(String),
    /// The object does not exist.
    #[error("{0} does not exist")]
    NotFound(String),
}

/// The schema, applied on open; additive changes get a new version.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS schema (version INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS apps (
    name TEXT PRIMARY KEY,
    current_release TEXT,
    previous_release TEXT,
    created_unix_ms INTEGER NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS releases (
    app TEXT NOT NULL REFERENCES apps(name),
    id TEXT NOT NULL,
    spec TEXT NOT NULL,
    state TEXT NOT NULL,
    message TEXT NOT NULL DEFAULT '',
    created_unix_ms INTEGER NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (app, id)
);
CREATE TABLE IF NOT EXISTS release_numbers (last INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS hosts (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS routes (
    host TEXT NOT NULL,
    path TEXT NOT NULL,
    spec TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (host, path)
);
CREATE TABLE IF NOT EXISTS rollouts (
    app TEXT NOT NULL,
    id TEXT NOT NULL,
    spec TEXT NOT NULL,
    state TEXT NOT NULL,
    created INTEGER NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (app, id)
);
CREATE TABLE IF NOT EXISTS scale_pins (
    app TEXT NOT NULL,
    release TEXT NOT NULL,
    grp TEXT NOT NULL,
    processes INTEGER NOT NULL,
    PRIMARY KEY (app, release, grp)
);
CREATE TABLE IF NOT EXISTS events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    time_unix_ms INTEGER NOT NULL,
    type TEXT NOT NULL,
    subject TEXT NOT NULL,
    level TEXT NOT NULL,
    message TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS event_sinks (
    name TEXT PRIMARY KEY,
    delivered INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS maintenance (
    app TEXT PRIMARY KEY,
    since_unix_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS host_maintenance (
    host TEXT PRIMARY KEY,
    since_unix_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS upstreams (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS overlays (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS overlay_activations (
    name TEXT NOT NULL,
    at_unix_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS conditions (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS overlay_state (
    name TEXT PRIMARY KEY,
    active INTEGER NOT NULL,
    source TEXT NOT NULL,
    since_unix_ms INTEGER NOT NULL,
    until_unix_ms INTEGER
);
CREATE TABLE IF NOT EXISTS client_classes (
    name TEXT PRIMARY KEY,
    position INTEGER NOT NULL,
    spec TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS rate_limits (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1
);
DROP TABLE IF EXISTS command_runs;
CREATE TABLE IF NOT EXISTS runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    app TEXT NOT NULL,
    name TEXT NOT NULL,
    release TEXT NOT NULL,
    trigger TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    detached INTEGER NOT NULL,
    exclusive INTEGER NOT NULL,
    inputs TEXT NOT NULL,
    timeout_ms INTEGER NOT NULL,
    started_unix_ms INTEGER NOT NULL,
    ended_unix_ms INTEGER NOT NULL DEFAULT 0,
    exit_code INTEGER NOT NULL DEFAULT 0,
    ok INTEGER NOT NULL DEFAULT 0,
    message TEXT NOT NULL DEFAULT '',
    pid INTEGER NOT NULL DEFAULT 0,
    start_time INTEGER NOT NULL DEFAULT 0,
    relay_pid INTEGER NOT NULL DEFAULT 0,
    relay_start_time INTEGER NOT NULL DEFAULT 0,
    stopping TEXT NOT NULL DEFAULT '',
    secrets TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS runs_by_command ON runs (app, name, ended_unix_ms);
CREATE TABLE IF NOT EXISTS gates (
    app TEXT NOT NULL,
    release TEXT NOT NULL,
    name TEXT NOT NULL,
    stage TEXT NOT NULL,
    run INTEGER NOT NULL,
    passed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (app, release, name, stage)
);
CREATE TABLE IF NOT EXISTS import_reports (
    app TEXT NOT NULL,
    release TEXT NOT NULL,
    report TEXT NOT NULL,
    PRIMARY KEY (app, release)
);
CREATE TABLE IF NOT EXISTS ownership (
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    deployment TEXT NOT NULL,
    PRIMARY KEY (kind, key)
);
CREATE TABLE IF NOT EXISTS log_periods (
    path TEXT PRIMARY KEY,
    period TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS log_files_seen (
    path TEXT PRIMARY KEY,
    device INTEGER NOT NULL,
    inode INTEGER NOT NULL
);
";

/// How many events are kept.
const EVENT_RETENTION: u64 = 10_000;

/// The schema this build writes.
pub const SCHEMA_VERSION: i64 = 23;

/// The store.
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    /// Open or create the store at `path`.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(SCHEMA)?;
        add_identity_columns(&connection)?;
        number_releases(&connection)?;
        give_preview_secrets(&connection)?;

        // Version 1 had applications, releases and events; version 2
        // added hosts and routes and version 3 the event sink cursors,
        // which the CREATE statements above supply to an older store;
        // version 4 added the application's user, group and umask, which
        // are added to an older table here; version 5 added the
        // applications in maintenance mode (section 4.12), version 6 the
        // HTTP upstreams, version 7 overlays and their state (section
        // 9.10), version 8 the client classes and rate limits of applied
        // files (sections 4.14 and 4.15), version 9 the application's
        // priority (section 7.4), added to an older table here, version
        // 10 the last run of each command (section 9.12), version 11 the
        // application's own access log choice (section 11.2), added to
        // an older table here, version 12 the virtual hosts in
        // maintenance mode (section 4.12), version 13 the period each log
        // file was last seen in (section 11.3), and version 14 whether
        // applied files declare a release (section 9.1), added to an older
        // table here, version 15 the file last seen at each log's path
        // (section 11.3), version 16 the runs of commands kept in place of
        // each command's last run (issue 688), version 17 the gates each
        // release has passed (issue 266), version 18 the named conditions
        // of applied files (issue 123), version 19 the activations of
        // overlays by their conditions (issue 689), version 20 the import
        // report of each release's load (issue 741), and version 21 where
        // the application's data directory and its parts are (issue 849),
        // added to an older table here, and version 22 the number of each
        // release (issue 948), given here to the releases of an older
        // store, and version 23 the preview secret of each release
        // (issue 928), given here to the releases that have none.

        let version: Option<i64> = connection
            .query_row("SELECT version FROM schema LIMIT 1", [], |row| row.get(0))
            .optional()?;

        match version {
            None => {
                connection.execute("INSERT INTO schema (version) VALUES (?1)", [SCHEMA_VERSION])?;
            }
            Some(version) if version < SCHEMA_VERSION => {
                connection.execute("UPDATE schema SET version = ?1", [SCHEMA_VERSION])?;
            }
            Some(_) => {}
        }

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Open an existing store only to read it, as a command does with no
    /// daemon running: the schema is left as it is, so a store is only
    /// ever migrated by the daemon that will use it, and any write fails.
    ///
    /// A store with no write-ahead log is opened as immutable, since
    /// SQLite otherwise leaves `-wal` and `-shm` files beside it. One with
    /// a log, as a daemon that crashed leaves, is read through the log, so
    /// that nothing it holds is missed.
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        let mut log = path.as_os_str().to_owned();
        log.push("-wal");

        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;

        let connection = if Path::new(&log).exists() {
            Connection::open_with_flags(path, flags)?
        } else {
            let escaped = path
                .to_string_lossy()
                .replace('%', "%25")
                .replace('?', "%3f")
                .replace('#', "%23");

            Connection::open_with_flags(format!("file:{escaped}?immutable=1"), flags)?
        };

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// What SQLite's own check of the database finds (issue 950): no
    /// lines for a sound one, else what it says is wrong.
    pub fn integrity(&self) -> Result<Vec<String>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare("PRAGMA integrity_check")?;
            let lines = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(lines.into_iter().filter(|line| line != "ok").collect())
        })
    }

    /// The rows that name a row their foreign key does not find, each as
    /// its table and row id.
    pub fn foreign_key_faults(&self) -> Result<Vec<String>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
            let rows = statement.query_map([], |row| {
                let table: String = row.get(0)?;
                let rowid: Option<i64> = row.get(1)?;
                let parent: String = row.get(2)?;
                Ok(match rowid {
                    Some(rowid) => format!("{table} row {rowid} names no row of {parent}"),
                    None => format!("a row of {table} names no row of {parent}"),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// The schema version the store records, or none when it records
    /// none, as a file that is not a store does not.
    pub fn schema_version(&self) -> Result<Option<i64>, StoreError> {
        self.with(|connection| {
            connection
                .query_row("SELECT version FROM schema LIMIT 1", [], |row| row.get(0))
                .optional()
                .map_err(StoreError::from)
        })
    }

    /// Each table with how many rows it holds, by name.
    pub fn table_counts(&self) -> Result<Vec<(String, u64)>, StoreError> {
        self.with(|connection| {
            let names: Vec<String> = {
                let mut statement = connection.prepare(
                    "SELECT name FROM sqlite_schema WHERE type = 'table' \
                     AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )?;
                let rows = statement.query_map([], |row| row.get(0))?;
                rows.collect::<Result<_, _>>()?
            };
            let mut counts = Vec::new();

            for name in names {
                let quoted = name.replace('"', "\"\"");
                let count: i64 = connection.query_row(
                    &format!("SELECT COUNT(*) FROM \"{quoted}\""),
                    [],
                    |row| row.get(0),
                )?;
                counts.push((name, count as u64));
            }

            Ok(counts)
        })
    }

    /// The schema as the SQL that would make it, one statement to a
    /// paragraph, tables before indexes.
    pub fn schema_sql(&self) -> Result<String, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
                 ORDER BY type = 'table' DESC, name",
            )?;
            let statements = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(statements
                .iter()
                .map(|sql| format!("{sql};\n"))
                .collect::<Vec<_>>()
                .join("\n"))
        })
    }

    /// Write a copy of the store to `path`, which must not exist: one
    /// consistent database with no log beside it, taken in one read
    /// while a daemon goes on using the store.
    pub fn copy_to(&self, path: &Path) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
            Ok(())
        })?;

        // Section 4.22: a copy is made to be looked at elsewhere, so it
        // leaves out the releases' preview secrets, and is rebuilt so that
        // no page of the file still holds one. A daemon started on a copy
        // gives its releases new ones.

        let copy = Connection::open(path)?;
        let has_secrets = copy
            .prepare("PRAGMA table_info(releases)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(Result::ok)
            .any(|column| column == "preview_secret");

        if has_secrets {
            copy.pragma_update(None, "journal_mode", "DELETE")?;
            copy.pragma_update(None, "secure_delete", "ON")?;
            copy.execute("UPDATE releases SET preview_secret = NULL", [])?;
            copy.execute("VACUUM", [])?;
        }

        Ok(())
    }

    /// What the store holds that breaks a rule of Muru's own, each rule
    /// by its name with the rows that break it; a rule nothing breaks
    /// has none. The rules are those the daemon keeps whenever it
    /// writes, so a break is a fault and never a state a command left.
    pub fn rule_breaks(&self) -> Result<Vec<(&'static str, Vec<String>)>, StoreError> {
        const RULES: [(&str, &str); 6] = [
            (
                "each release belongs to an application",
                "SELECT 'release ' || app || '@' || id || ' has no application' FROM releases \
                 WHERE app NOT IN (SELECT name FROM apps)",
            ),
            (
                "each application's serving and previous release exist",
                "SELECT 'application ' || name || ' serves ' || current_release || \
                 ', which is no release' FROM apps WHERE current_release IS NOT NULL \
                 AND NOT EXISTS (SELECT 1 FROM releases WHERE app = name AND id = current_release) \
                 UNION ALL \
                 SELECT 'application ' || name || ' would roll back to ' || previous_release || \
                 ', which is no release' FROM apps WHERE previous_release IS NOT NULL \
                 AND NOT EXISTS (SELECT 1 FROM releases WHERE app = name AND id = previous_release)",
            ),
            (
                "scale pins, gates and import reports belong to a release",
                "SELECT 'a scale pin names ' || app || '@' || release FROM scale_pins s \
                 WHERE NOT EXISTS (SELECT 1 FROM releases r WHERE r.app = s.app AND r.id = s.release) \
                 UNION ALL \
                 SELECT 'a passed gate names ' || app || '@' || release FROM gates g \
                 WHERE NOT EXISTS (SELECT 1 FROM releases r WHERE r.app = g.app AND r.id = g.release) \
                 UNION ALL \
                 SELECT 'an import report names ' || app || '@' || release FROM import_reports i \
                 WHERE NOT EXISTS (SELECT 1 FROM releases r WHERE r.app = i.app AND r.id = i.release)",
            ),
            (
                "rollouts, runs and maintenance belong to an application",
                "SELECT 'rollout ' || id || ' names application ' || app FROM rollouts \
                 WHERE app NOT IN (SELECT name FROM apps) \
                 UNION ALL \
                 SELECT 'run ' || id || ' names application ' || app FROM runs \
                 WHERE app NOT IN (SELECT name FROM apps) \
                 UNION ALL \
                 SELECT 'maintenance names application ' || app FROM maintenance \
                 WHERE app NOT IN (SELECT name FROM apps)",
            ),
            (
                "each release has a number of its own, none past the count",
                "SELECT 'release ' || app || '@' || id || ' has no number' FROM releases \
                 WHERE number IS NULL \
                 UNION ALL \
                 SELECT 'the number ' || number || ' names ' || COUNT(*) || ' releases' \
                 FROM releases WHERE number IS NOT NULL GROUP BY number HAVING COUNT(*) > 1 \
                 UNION ALL \
                 SELECT 'release ' || app || '@' || id || ' has the number ' || number || \
                 ', past the count ' || (SELECT COALESCE(MAX(last), 0) FROM release_numbers) \
                 FROM releases \
                 WHERE number > (SELECT COALESCE(MAX(last), 0) FROM release_numbers)",
            ),
            (
                "event sequence numbers have no gaps",
                "SELECT 'the events run from ' || MIN(sequence) || ' to ' || MAX(sequence) || \
                 ' and there are ' || COUNT(*) FROM events \
                 HAVING COUNT(*) > 0 AND MAX(sequence) - MIN(sequence) + 1 <> COUNT(*)",
            ),
        ];

        self.with(|connection| {
            let mut found = Vec::new();

            for (rule, query) in RULES {
                let mut statement = connection.prepare(query)?;
                let breaks = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                found.push((rule, breaks));
            }

            Ok(found)
        })
    }

    /// An in-memory store, for tests and for development runs.
    pub fn in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(SCHEMA)?;
        add_identity_columns(&connection)?;
        number_releases(&connection)?;
        give_preview_secrets(&connection)?;
        connection.execute("INSERT INTO schema (version) VALUES (?1)", [SCHEMA_VERSION])?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn with<T>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let connection = self.connection.lock().expect("store poisoned");
        work(&connection)
    }

    /// Make `work`'s writes in one transaction: all of them, or, when it
    /// fails, none (section 10.10). The store is locked throughout, so
    /// `work` must not read or write through the store itself, events
    /// included.
    pub fn transaction<T, E: From<StoreError>>(
        &self,
        work: impl FnOnce(&Writes) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut connection = self.connection.lock().expect("store poisoned");
        let transaction = connection.transaction().map_err(StoreError::from)?;
        let done = work(&Writes {
            connection: &transaction,
        })?;
        transaction.commit().map_err(StoreError::from)?;
        Ok(done)
    }

    /// Every application, by name.
    pub fn apps(&self) -> Result<Vec<App>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT name, current_release, previous_release, created_unix_ms, resource_version, \
                 user, grp, umask, priority, access_log, data_dir, media_dir, uploads_dir, settings, \
                 serving_origin FROM apps ORDER BY name",
            )?;
            let rows = statement.query_map([], app_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// One application.
    pub fn app(&self, name: &str) -> Result<App, StoreError> {
        self.with(|connection| {
            connection
                .query_row(
                    "SELECT name, current_release, previous_release, created_unix_ms, resource_version, \
                     user, grp, umask, priority, access_log, data_dir, media_dir, uploads_dir, settings, \
                     serving_origin FROM apps WHERE name = ?1",
                    [name],
                    app_from_row,
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("application {name}")))
        })
    }

    /// Set the user, group and umask an application's processes run with
    /// (section 10.9).
    pub fn set_app_identity(
        &self,
        name: &str,
        user: Option<&str>,
        group: Option<&str>,
        umask: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            let changed = connection.execute(
                "UPDATE apps SET user = ?2, grp = ?3, umask = ?4, \
                 resource_version = resource_version + 1 WHERE name = ?1",
                params![name, user, group, umask],
            )?;

            if changed == 0 {
                return Err(StoreError::NotFound(format!("application {name}")));
            }

            Ok(())
        })
    }

    /// Set the priority by which the node's budget admits an
    /// application's scale-ups (section 7.4).
    pub fn set_app_priority(&self, name: &str, priority: i32) -> Result<(), StoreError> {
        self.with(|connection| {
            let changed = connection.execute(
                "UPDATE apps SET priority = ?2, resource_version = resource_version + 1 \
                 WHERE name = ?1",
                params![name, priority],
            )?;

            if changed == 0 {
                return Err(StoreError::NotFound(format!("application {name}")));
            }

            Ok(())
        })
    }

    /// Set where the application's data directory and its parts are
    /// (issue 849); none for each is the default.
    pub fn set_app_data(
        &self,
        name: &str,
        data_dir: Option<&str>,
        media_dir: Option<&str>,
        uploads_dir: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            let changed = connection.execute(
                "UPDATE apps SET data_dir = ?2, media_dir = ?3, uploads_dir = ?4, \
                 resource_version = resource_version + 1 WHERE name = ?1",
                params![name, data_dir, media_dir, uploads_dir],
            )?;

            if changed == 0 {
                return Err(StoreError::NotFound(format!("application {name}")));
            }

            Ok(())
        })
    }

    /// Set the application's own access log choice (section 11.2); an
    /// empty one leaves the `[logs]` table's.
    pub fn set_app_access_log(
        &self,
        name: &str,
        access_log: &crate::model::AccessLogSpec,
    ) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.set_app_access_log(name, access_log))
    }

    /// Set the application's own release settings (issue 899); empty
    /// ones remove them.
    pub fn set_app_settings(
        &self,
        name: &str,
        settings: &crate::app_settings::AppSettings,
    ) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.set_app_settings(name, settings))
    }

    /// Whether applied files last wrote the application's settings, and
    /// with `forget` no longer: a command is about to write them.
    pub fn settings_applied(&self, name: &str, forget: bool) -> Result<bool, StoreError> {
        self.with(|connection| {
            let applied: i64 = connection.query_row(
                "SELECT COUNT(*) FROM ownership WHERE kind = 'settings' AND key = ?1",
                [name],
                |row| row.get(0),
            )?;

            if forget {
                connection.execute(
                    "DELETE FROM ownership WHERE kind = 'settings' AND key = ?1",
                    [name],
                )?;
            }

            Ok(applied > 0)
        })
    }

    /// Change the application's own release settings in one step under
    /// the store's lock, creating the application when it does not exist:
    /// `change` is given the settings as they are, and what it leaves is
    /// written and returned. A change that fails writes nothing.
    pub fn update_app_settings(
        &self,
        name: &str,
        change: impl FnOnce(&mut crate::app_settings::AppSettings) -> Result<(), String>,
    ) -> Result<Result<crate::app_settings::AppSettings, String>, StoreError> {
        self.with(|connection| {
            connection.execute(
                "INSERT OR IGNORE INTO apps (name, created_unix_ms) VALUES (?1, ?2)",
                params![name, now_unix_ms() as i64],
            )?;

            let encoded: Option<String> = connection.query_row(
                "SELECT settings FROM apps WHERE name = ?1",
                [name],
                |row| row.get(0),
            )?;

            let mut settings: crate::app_settings::AppSettings = match encoded {
                Some(encoded) => decode(&encoded)?,
                None => crate::app_settings::AppSettings::default(),
            };

            if let Err(error) = change(&mut settings) {
                return Ok(Err(error));
            }

            Writes { connection }.set_app_settings(name, &settings)?;
            Ok(Ok(settings))
        })
    }

    /// Create an application if it does not exist, and return it.
    pub fn ensure_app(&self, name: &str) -> Result<App, StoreError> {
        self.with(|connection| {
            connection.execute(
                "INSERT OR IGNORE INTO apps (name, created_unix_ms) VALUES (?1, ?2)",
                params![name, now_unix_ms() as i64],
            )?;
            Ok(())
        })?;
        self.app(name)
    }

    /// Record which release serves the application and which a rollback restores.
    pub fn set_app_releases(
        &self,
        name: &str,
        current: Option<&str>,
        previous: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.set_app_releases(name, current, previous))
    }

    /// Record that applied files or a rollback set the serving release
    /// just written.
    pub fn set_serving_origin(
        &self,
        name: &str,
        how: crate::model::ServingSetBy,
    ) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.set_serving_origin(name, how))
    }

    /// Releases of one application, or of every application, newest first.
    pub fn releases(&self, app: Option<&str>) -> Result<Vec<Release>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT app, id, spec, state, message, created_unix_ms, resource_version, number \
                 FROM releases WHERE (?1 IS NULL OR app = ?1) ORDER BY created_unix_ms DESC, id DESC",
            )?;
            let rows = statement.query_map([app], release_from_row)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
        })
    }

    /// One release.
    pub fn release(&self, app: &str, id: &str) -> Result<Release, StoreError> {
        self.with(|connection| {
            connection
                .query_row(
                    "SELECT app, id, spec, state, message, created_unix_ms, resource_version, number \
                     FROM releases WHERE app = ?1 AND id = ?2",
                    [app, id],
                    release_from_row,
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("release {app}@{id}")))
        })
    }

    /// Record a new release in the pending state.
    pub fn create_release(
        &self,
        app: &str,
        id: &str,
        spec: &ReleaseSpec,
    ) -> Result<Release, StoreError> {
        let encoded =
            toml::to_string(spec).map_err(|error| StoreError::Decode(error.to_string()))?;
        let secret = new_preview_secret()?;
        self.with(|connection| {
            // The release's number (section 13.8, issue 948): the next of
            // a count that only goes up, so a number never names two
            // releases of a store, whatever was removed in between. Its
            // preview secret (section 4.22) is made with it and kept for
            // as long as the record is.

            connection.execute("UPDATE release_numbers SET last = last + 1", [])?;
            connection.execute(
                "INSERT INTO releases \
                 (app, id, spec, state, created_unix_ms, number, preview_secret) \
                 VALUES (?1, ?2, ?3, ?4, ?5, (SELECT last FROM release_numbers), ?6)",
                params![
                    app,
                    id,
                    encoded,
                    ReleaseState::Pending.as_str(),
                    now_unix_ms() as i64,
                    secret
                ],
            )?;
            Ok(())
        })?;
        self.release(app, id)
    }

    /// A release's preview secret (section 4.22). It is kept apart from
    /// the release's record, which commands print and bundles carry, so
    /// that nothing shows it but a preview that was asked for.
    pub fn preview_secret(&self, app: &str, id: &str) -> Result<String, StoreError> {
        self.with(|connection| {
            connection
                .query_row(
                    "SELECT preview_secret FROM releases WHERE app = ?1 AND id = ?2",
                    [app, id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
                .ok_or_else(|| StoreError::NotFound(format!("release {app}@{id}")))
        })
    }

    /// Every release's preview secret, by application and release, for
    /// the snapshot.
    pub fn preview_secrets(&self) -> Result<HashMap<(String, String), String>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT app, id, preview_secret FROM releases WHERE preview_secret IS NOT NULL",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(((row.get(0)?, row.get(1)?), row.get::<_, String>(2)?))
            })?;
            rows.collect::<Result<_, _>>().map_err(StoreError::from)
        })
    }

    /// Give a release a new preview secret, so that every link, cookie
    /// and header made with the old one stops previewing it.
    pub fn renew_preview_secret(&self, app: &str, id: &str) -> Result<String, StoreError> {
        let secret = new_preview_secret()?;
        self.with(|connection| {
            let changed = connection.execute(
                "UPDATE releases SET preview_secret = ?1 WHERE app = ?2 AND id = ?3",
                params![secret, app, id],
            )?;

            if changed == 0 {
                return Err(StoreError::NotFound(format!("release {app}@{id}")));
            }

            Ok(())
        })?;
        Ok(secret)
    }

    /// The next generated release id for an application: r1, r2, ...
    pub fn next_release_id(&self, app: &str) -> Result<String, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare("SELECT id FROM releases WHERE app = ?1")?;
            let highest = statement
                .query_map([app], |row| row.get::<_, String>(0))?
                .filter_map(Result::ok)
                .filter_map(|id| {
                    id.strip_prefix('r')
                        .and_then(|rest| rest.parse::<u64>().ok())
                })
                .max()
                .unwrap_or(0);
            Ok(format!("r{}", highest + 1))
        })
    }

    /// Remove a release record and its scale pins. The caller makes sure
    /// it is not running.
    pub fn delete_release(&self, app: &str, id: &str) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "DELETE FROM scale_pins WHERE app = ?1 AND release = ?2",
                params![app, id],
            )?;
            connection.execute(
                "DELETE FROM gates WHERE app = ?1 AND release = ?2",
                params![app, id],
            )?;
            connection.execute(
                "DELETE FROM import_reports WHERE app = ?1 AND release = ?2",
                params![app, id],
            )?;
            connection.execute(
                "DELETE FROM releases WHERE app = ?1 AND id = ?2",
                params![app, id],
            )?;
            Ok(())
        })
    }

    /// Keep a release's import report (section 6.18), the JSON its host
    /// sent, in place of the one before.
    pub fn put_import_report(&self, app: &str, id: &str, report: &str) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "INSERT INTO import_reports (app, release, report) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (app, release) DO UPDATE SET report = excluded.report",
                params![app, id, report],
            )?;
            Ok(())
        })
    }

    /// A release's import report (section 6.18), if its host sent one.
    pub fn import_report(&self, app: &str, id: &str) -> Result<Option<String>, StoreError> {
        self.with(|connection| {
            connection
                .query_row(
                    "SELECT report FROM import_reports WHERE app = ?1 AND release = ?2",
                    params![app, id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(StoreError::from)
        })
    }

    /// The releases of an application that applied files declare (section
    /// 9.1), which `release prune` leaves unless forced.
    pub fn declared_releases(&self, app: &str) -> Result<BTreeSet<String>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT id FROM releases WHERE app = ?1 AND declared = 1")?;
            let rows = statement.query_map([app], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<BTreeSet<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// The named deployment recorded for each object (ADR 0080), by kind
    /// and name. An object with no record belongs to `default`, and a
    /// record may outlive its object, which the caller passes over.
    pub fn ownership(&self) -> Result<crate::deployments::Owners, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT kind, key, deployment FROM ownership")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut owners = crate::deployments::Owners::new();

            for row in rows {
                let (kind, key, deployment) = row?;

                if let Some(kind) = crate::deployments::Kind::parse(&kind) {
                    owners.insert((kind, key), deployment);
                }
            }

            Ok(owners)
        })
    }

    /// Record who owns the objects an apply declared: a named deployment
    /// for each of its own, and no record for one that is `default`'s.
    pub fn set_owners(&self, owners: &crate::deployments::Owners) -> Result<(), StoreError> {
        self.with(|connection| {
            let transaction = connection.unchecked_transaction()?;

            for ((kind, key), deployment) in owners {
                if deployment == crate::deployments::DEFAULT {
                    transaction.execute(
                        "DELETE FROM ownership WHERE kind = ?1 AND key = ?2",
                        params![kind.as_str(), key],
                    )?;
                } else {
                    transaction.execute(
                        "INSERT INTO ownership (kind, key, deployment) VALUES (?1, ?2, ?3) \
                         ON CONFLICT(kind, key) DO UPDATE SET deployment = excluded.deployment",
                        params![kind.as_str(), key, deployment],
                    )?;
                }
            }

            transaction.commit()?;
            Ok(())
        })
    }

    /// Record the releases of an application that the files of an apply
    /// declare, in place of those an earlier apply declared.
    pub fn set_declared_releases(&self, app: &str, ids: &[String]) -> Result<(), StoreError> {
        self.with(|connection| {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute("UPDATE releases SET declared = 0 WHERE app = ?1", [app])?;

            for id in ids {
                transaction.execute(
                    "UPDATE releases SET declared = 1 WHERE app = ?1 AND id = ?2",
                    params![app, id],
                )?;
            }

            transaction.commit()?;
            Ok(())
        })
    }

    /// Remove an application and its release records. The caller makes
    /// sure none of them is running.
    pub fn delete_app(&self, name: &str) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute("DELETE FROM releases WHERE app = ?1", [name])?;
            connection.execute("DELETE FROM rollouts WHERE app = ?1", [name])?;
            connection.execute("DELETE FROM scale_pins WHERE app = ?1", [name])?;
            connection.execute("DELETE FROM maintenance WHERE app = ?1", [name])?;
            connection.execute("DELETE FROM runs WHERE app = ?1", [name])?;
            connection.execute("DELETE FROM gates WHERE app = ?1", [name])?;
            connection.execute("DELETE FROM apps WHERE name = ?1", [name])?;
            Ok(())
        })
    }

    /// Section 4.12: the applications in maintenance mode, each with when
    /// it was turned on.
    pub fn maintenance(&self) -> Result<Vec<(String, u64)>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT app, since_unix_ms FROM maintenance ORDER BY app")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// Put an application into maintenance mode, or take it out.
    pub fn set_maintenance(&self, app: &str, since_unix_ms: Option<u64>) -> Result<(), StoreError> {
        self.with(|connection| {
            match since_unix_ms {
                Some(since) => {
                    connection.execute(
                        "INSERT INTO maintenance (app, since_unix_ms) VALUES (?1, ?2) \
                         ON CONFLICT(app) DO NOTHING",
                        params![app, since as i64],
                    )?;
                }
                None => {
                    connection.execute("DELETE FROM maintenance WHERE app = ?1", [app])?;
                }
            }

            Ok(())
        })
    }

    /// Section 4.12: the virtual hosts in maintenance mode, each with
    /// when it was turned on.
    pub fn host_maintenance(&self) -> Result<Vec<(String, u64)>, StoreError> {
        self.with(|connection| {
            let mut statement = connection
                .prepare("SELECT host, since_unix_ms FROM host_maintenance ORDER BY host")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// Put a virtual host into maintenance mode, or take it out.
    pub fn set_host_maintenance(
        &self,
        host: &str,
        since_unix_ms: Option<u64>,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            match since_unix_ms {
                Some(since) => {
                    connection.execute(
                        "INSERT INTO host_maintenance (host, since_unix_ms) VALUES (?1, ?2) \
                         ON CONFLICT(host) DO NOTHING",
                        params![host, since as i64],
                    )?;
                }
                None => {
                    connection.execute("DELETE FROM host_maintenance WHERE host = ?1", [host])?;
                }
            }

            Ok(())
        })
    }

    /// Every virtual host, by name, with its definition.
    pub fn hosts(&self) -> Result<Vec<(String, VirtualHost)>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare("SELECT name, spec FROM hosts ORDER BY name")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut hosts = Vec::new();

            for row in rows {
                let (name, encoded) = row?;
                hosts.push((name, decode(&encoded)?));
            }

            Ok(hosts)
        })
    }

    /// Create or replace a virtual host.
    pub fn put_host(&self, name: &str, host: &VirtualHost) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.put_host(name, host))
    }

    /// Remove a virtual host and its routes.
    pub fn delete_host(&self, name: &str) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.delete_host(name))
    }

    /// Every HTTP upstream, by name (section 4.13).
    pub fn upstreams(&self) -> Result<Vec<(String, Upstream)>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT name, spec FROM upstreams ORDER BY name")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut upstreams = Vec::new();

            for row in rows {
                let (name, encoded) = row?;
                upstreams.push((name, decode(&encoded)?));
            }

            Ok(upstreams)
        })
    }

    /// Create or replace an HTTP upstream.
    pub fn put_upstream(&self, name: &str, upstream: &Upstream) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.put_upstream(name, upstream))
    }

    /// Remove an HTTP upstream.
    pub fn delete_upstream(&self, name: &str) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.delete_upstream(name))
    }

    /// The client classes of applied files with their positions, in the
    /// order their rules are tried (section 4.14).
    pub fn client_classes(&self) -> Result<Vec<(i64, ClientClass)>, StoreError> {
        self.with(|connection| {
            let mut statement = connection
                .prepare("SELECT position, spec FROM client_classes ORDER BY position, name")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut classes = Vec::new();

            for row in rows {
                let (position, encoded) = row?;
                classes.push((position, decode(&encoded)?));
            }

            Ok(classes)
        })
    }

    /// Create or replace a client class, its rule tried at `position`
    /// among the applied ones.
    pub fn put_client_class(&self, class: &ClientClass, position: i64) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.put_client_class(class, position))
    }

    /// Remove a client class.
    pub fn delete_client_class(&self, name: &str) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.delete_client_class(name))
    }

    /// The rate limits of applied files, by name (section 4.15).
    pub fn rate_limits(&self) -> Result<Vec<(String, RateLimit)>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT name, spec FROM rate_limits ORDER BY name")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut limits = Vec::new();

            for row in rows {
                let (name, encoded) = row?;
                limits.push((name, decode(&encoded)?));
            }

            Ok(limits)
        })
    }

    /// Create or replace a rate limit.
    pub fn put_rate_limit(&self, name: &str, limit: &RateLimit) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.put_rate_limit(name, limit))
    }

    /// Remove a rate limit.
    pub fn delete_rate_limit(&self, name: &str) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.delete_rate_limit(name))
    }

    /// Record that an overlay's condition activated it, forgetting those
    /// more than an hour old (issue 689).
    pub fn record_overlay_activation(&self, name: &str, at_unix_ms: u64) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "DELETE FROM overlay_activations WHERE at_unix_ms < ?1",
                [at_unix_ms.saturating_sub(3_600_000) as i64],
            )?;
            connection.execute(
                "INSERT INTO overlay_activations (name, at_unix_ms) VALUES (?1, ?2)",
                params![name, at_unix_ms as i64],
            )?;
            Ok(())
        })
    }

    /// How many times an overlay's condition activated it since
    /// `since_unix_ms`.
    pub fn overlay_activations_since(
        &self,
        name: &str,
        since_unix_ms: u64,
    ) -> Result<u32, StoreError> {
        self.with(|connection| {
            connection
                .query_row(
                    "SELECT count(*) FROM overlay_activations WHERE name = ?1 AND at_unix_ms >= ?2",
                    params![name, since_unix_ms as i64],
                    |row| row.get(0),
                )
                .map_err(StoreError::from)
        })
    }

    /// Every condition applied files declare, by name (section 9.10).
    pub fn conditions(&self) -> Result<Vec<(String, muru_config::Condition)>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT name, spec FROM conditions ORDER BY name")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut conditions = Vec::new();

            for row in rows {
                let (name, encoded) = row?;
                conditions.push((name, decode(&encoded)?));
            }

            Ok(conditions)
        })
    }

    /// Every overlay declared, by name (section 9.10).
    pub fn overlays(&self) -> Result<Vec<(String, Overlay)>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT name, spec FROM overlays ORDER BY name")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut overlays = Vec::new();

            for row in rows {
                let (name, encoded) = row?;
                overlays.push((name, decode(&encoded)?));
            }

            Ok(overlays)
        })
    }

    /// Create or replace an overlay's declaration.
    pub fn put_overlay(&self, name: &str, overlay: &Overlay) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.put_overlay(name, overlay))
    }

    /// Remove an overlay's declaration and its state.
    pub fn delete_overlay(&self, name: &str) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.delete_overlay(name))
    }

    /// Every overlay's state: active, or ended for a while.
    pub fn overlay_states(&self) -> Result<Vec<OverlayState>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT name, active, source, since_unix_ms, until_unix_ms \
                 FROM overlay_state ORDER BY name",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(OverlayState {
                    name: row.get(0)?,
                    active: row.get::<_, i64>(1)? != 0,
                    source: row.get(2)?,
                    since_unix_ms: row.get::<_, i64>(3)? as u64,
                    until_unix_ms: row.get::<_, Option<i64>>(4)?.map(|until| until as u64),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// Record an overlay's state, or clear it with `None`.
    pub fn set_overlay_state(
        &self,
        name: &str,
        state: Option<&OverlayState>,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            match state {
                Some(state) => {
                    connection.execute(
                        "INSERT INTO overlay_state (name, active, source, since_unix_ms, until_unix_ms) \
                         VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(name) DO UPDATE SET \
                         active = excluded.active, source = excluded.source, \
                         since_unix_ms = excluded.since_unix_ms, until_unix_ms = excluded.until_unix_ms",
                        params![
                            name,
                            i64::from(state.active),
                            state.source,
                            state.since_unix_ms as i64,
                            state.until_unix_ms.map(|until| until as i64)
                        ],
                    )?;
                }
                None => {
                    connection.execute("DELETE FROM overlay_state WHERE name = ?1", [name])?;
                }
            }

            Ok(())
        })
    }

    /// Every route, by host then path.
    pub fn routes(&self) -> Result<Vec<Route>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT spec FROM routes ORDER BY host, path")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            let mut routes = Vec::new();

            for row in rows {
                routes.push(decode(&row?)?);
            }

            Ok(routes)
        })
    }

    /// Create or replace a route, identified by its host and key: the
    /// path, and the client classes it is for (section 4.14).
    pub fn put_route(&self, route: &Route) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.put_route(route))
    }

    /// The rollouts of one application, or of all, oldest first (section
    /// 9.3).
    pub fn rollouts(&self, app: Option<&str>) -> Result<Vec<Rollout>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT spec FROM rollouts WHERE (?1 = '' OR app = ?1) ORDER BY app, created",
            )?;
            let rows = statement.query_map([app.unwrap_or("")], |row| row.get::<_, String>(0))?;
            let mut rollouts = Vec::new();

            for row in rows {
                rollouts.push(decode(&row?)?);
            }

            Ok(rollouts)
        })
    }

    /// One rollout by id, or the application's latest when `id` is empty.
    pub fn rollout(&self, app: &str, id: &str) -> Result<Rollout, StoreError> {
        self.with(|connection| {
            let encoded: Option<String> = connection
                .query_row(
                    "SELECT spec FROM rollouts WHERE app = ?1 AND (?2 = '' OR id = ?2) \
                     ORDER BY created DESC LIMIT 1",
                    params![app, id],
                    |row| row.get(0),
                )
                .optional()?;

            match encoded {
                Some(encoded) => decode(&encoded),
                None => Err(StoreError::NotFound(format!("rollout {app}/{id}"))),
            }
        })
    }

    /// The application's rollout that still shapes traffic, if any.
    pub fn active_rollout(&self, app: &str) -> Result<Option<Rollout>, StoreError> {
        Ok(self
            .rollouts(Some(app))?
            .into_iter()
            .find(|rollout| rollout.state.is_active()))
    }

    /// Create or replace a rollout record.
    pub fn put_rollout(&self, rollout: &Rollout) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(rollout).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.with(|connection| {
            connection.execute(
                "INSERT INTO rollouts (app, id, spec, state, created) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(app, id) DO UPDATE SET spec = excluded.spec, \
                 state = excluded.state, resource_version = resource_version + 1",
                params![
                    rollout.app,
                    rollout.id,
                    encoded,
                    rollout.state.as_str(),
                    rollout.created_unix_ms as i64
                ],
            )?;
            Ok(())
        })
    }

    /// Section 7.4: the worker counts an operator pinned on a release's
    /// groups, by group name.
    pub fn scale_pins(&self, app: &str, release: &str) -> Result<Vec<(String, u32)>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT grp, processes FROM scale_pins WHERE app = ?1 AND release = ?2 ORDER BY grp",
            )?;
            let rows = statement.query_map(params![app, release], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32))
            })?;
            let mut pins = Vec::new();

            for row in rows {
                pins.push(row?);
            }

            Ok(pins)
        })
    }

    /// Pin a group's worker count, replacing any earlier pin.
    pub fn put_scale_pin(
        &self,
        app: &str,
        release: &str,
        group: &str,
        processes: u32,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "INSERT INTO scale_pins (app, release, grp, processes) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(app, release, grp) DO UPDATE SET processes = excluded.processes",
                params![app, release, group, processes as i64],
            )?;
            Ok(())
        })
    }

    /// Remove a group's pin, or every pin of the release when no group is
    /// named.
    pub fn remove_scale_pins(
        &self,
        app: &str,
        release: &str,
        group: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "DELETE FROM scale_pins WHERE app = ?1 AND release = ?2 AND (?3 = '' OR grp = ?3)",
                params![app, release, group.unwrap_or("")],
            )?;
            Ok(())
        })
    }

    /// The next rollout id for an application: 1, 2, and so on.
    pub fn next_rollout_id(&self, app: &str) -> Result<String, StoreError> {
        self.with(|connection| {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM rollouts WHERE app = ?1",
                [app],
                |row| row.get(0),
            )?;
            Ok((count + 1).to_string())
        })
    }

    /// Remove a route, by its host and key.
    pub fn delete_route(&self, host: &str, path: &str) -> Result<(), StoreError> {
        self.with(|connection| Writes { connection }.delete_route(host, path))
    }

    /// The sequence number of the last event: a version of the whole
    /// store, since every change to it is an event.
    pub fn state_version(&self) -> Result<u64, StoreError> {
        self.with(|connection| {
            let last: Option<i64> =
                connection.query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))?;
            Ok(last.unwrap_or(0) as u64)
        })
    }

    /// Move a release to a state, with a message saying why or what next.
    pub fn set_release_state(
        &self,
        app: &str,
        id: &str,
        state: ReleaseState,
        message: &str,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "UPDATE releases SET state = ?3, message = ?4, resource_version = resource_version + 1 \
                 WHERE app = ?1 AND id = ?2",
                params![app, id, state.as_str(), message],
            )?;
            Ok(())
        })
    }

    /// Append an event and return it with its sequence number. Events past
    /// the retention count are dropped, oldest first.
    pub fn append_event(
        &self,
        kind: &str,
        subject: &str,
        level: &'static str,
        message: &str,
    ) -> Result<Event, StoreError> {
        self.append_event_at(kind, subject, level, message, None)
    }

    /// Append an event that happened at `happened_unix_ms`, or now when
    /// none is given, and return it with its sequence number, which is
    /// the next whatever its time: an event recorded late sits in the
    /// order it was recorded in with the time it happened (section 11.5).
    /// A time ahead of the clock is taken as now.
    pub fn append_event_at(
        &self,
        kind: &str,
        subject: &str,
        level: &'static str,
        message: &str,
        happened_unix_ms: Option<u64>,
    ) -> Result<Event, StoreError> {
        let now = now_unix_ms();
        let time_unix_ms = happened_unix_ms.map_or(now, |happened| happened.min(now));
        self.with(|connection| {
            connection.execute(
                "INSERT INTO events (time_unix_ms, type, subject, level, message) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![time_unix_ms as i64, kind, subject, level, message],
            )?;
            let sequence = connection.last_insert_rowid() as u64;
            connection.execute(
                "DELETE FROM events WHERE sequence <= ?1",
                [sequence.saturating_sub(EVENT_RETENTION) as i64],
            )?;

            Ok(Event {
                sequence,
                time_unix_ms,
                kind: kind.to_owned(),
                subject: subject.to_owned(),
                level,
                message: message.to_owned(),
            })
        })
    }

    /// The sequence of the latest event, or 0 before any.
    pub fn last_event_sequence(&self) -> Result<u64, StoreError> {
        self.with(|connection| {
            let latest: Option<i64> =
                connection.query_row("SELECT MAX(sequence) FROM events", [], |row| row.get(0))?;
            Ok(latest.unwrap_or(0) as u64)
        })
    }

    /// The sequence of the last event a sink delivered, or none for a
    /// sink the store has not seen.
    pub fn sink_cursor(&self, name: &str) -> Result<Option<u64>, StoreError> {
        self.with(|connection| {
            let delivered: Option<i64> = connection
                .query_row(
                    "SELECT delivered FROM event_sinks WHERE name = ?1",
                    [name],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(delivered.map(|delivered| delivered as u64))
        })
    }

    /// Record the last event a sink delivered.
    pub fn set_sink_cursor(&self, name: &str, delivered: u64) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "INSERT INTO event_sinks (name, delivered) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET delivered = excluded.delivered",
                params![name, delivered as i64],
            )?;
            Ok(())
        })
    }

    /// Record a run as it starts and give it its id. A run of an
    /// exclusive command is refused while another run of that command
    /// holds the application's claim, and the answer is then the id of
    /// that run instead: the check and the record are made together, so
    /// two runs can never both hold the claim (section 9.12).
    pub fn begin_run(&self, run: &RunRecord) -> Result<Result<u64, u64>, StoreError> {
        self.with(|connection| {
            if run.exclusive {
                let holder: Option<i64> = connection
                    .query_row(
                        "SELECT id FROM runs WHERE app = ?1 AND name = ?2 AND exclusive = 1 \
                         AND ended_unix_ms = 0 ORDER BY id LIMIT 1",
                        params![run.app, run.name],
                        |row| row.get(0),
                    )
                    .optional()?;

                if let Some(holder) = holder {
                    return Ok(Err(holder as u64));
                }
            }

            connection.execute(
                "INSERT INTO runs (app, name, release, trigger, requested_by, detached, \
                 exclusive, inputs, timeout_ms, started_unix_ms, secrets) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    run.app,
                    run.name,
                    run.release,
                    run.trigger,
                    run.requested_by,
                    run.detached,
                    run.exclusive,
                    run.inputs,
                    run.timeout_ms as i64,
                    run.started_unix_ms as i64,
                    run.secrets,
                ],
            )?;
            Ok(Ok(connection.last_insert_rowid() as u64))
        })
    }

    /// Record the process a run runs as, and the relay of a detached one.
    pub fn set_run_process(
        &self,
        id: u64,
        pid: u32,
        start_time: u64,
        relay_pid: u32,
        relay_start_time: u64,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "UPDATE runs SET pid = ?2, start_time = ?3, relay_pid = ?4, \
                 relay_start_time = ?5 WHERE id = ?1",
                params![
                    id as i64,
                    pid,
                    start_time as i64,
                    relay_pid,
                    relay_start_time as i64
                ],
            )?;
            Ok(())
        })
    }

    /// Record why a run is being stopped.
    pub fn set_run_stopping(&self, id: u64, why: &str) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "UPDATE runs SET stopping = ?2 WHERE id = ?1 AND stopping = ''",
                params![id as i64, why],
            )?;
            Ok(())
        })
    }

    /// Record how a run ended, which gives up its claim.
    pub fn end_run(
        &self,
        id: u64,
        ended_unix_ms: u64,
        exit_code: i32,
        ok: bool,
        message: &str,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "UPDATE runs SET ended_unix_ms = ?2, exit_code = ?3, ok = ?4, message = ?5 \
                 WHERE id = ?1",
                params![
                    id as i64,
                    ended_unix_ms.max(1) as i64,
                    exit_code,
                    ok,
                    message
                ],
            )?;
            Ok(())
        })
    }

    /// One run, by its id.
    pub fn run(&self, id: u64) -> Result<RunRecord, StoreError> {
        self.with(|connection| {
            connection
                .query_row(
                    &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
                    [id as i64],
                    run_from_row,
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("run {id}")))
        })
    }

    /// An application's runs, or one command's: those under way first,
    /// then the newest first.
    pub fn runs(&self, app: &str, name: Option<&str>) -> Result<Vec<RunRecord>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM runs WHERE app = ?1 AND (?2 IS NULL OR name = ?2) \
                 ORDER BY ended_unix_ms != 0, id DESC"
            ))?;
            let rows = statement.query_map(params![app, name], run_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// The id of every run kept (issue 692), for telling which runs'
    /// files are orphaned.
    pub fn run_ids(&self) -> Result<HashSet<u64>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare("SELECT id FROM runs")?;
            let rows = statement.query_map([], |row| row.get::<_, i64>(0))?;
            rows.map(|id| id.map(|id| id as u64))
                .collect::<Result<HashSet<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// Every run under way, the oldest first: what a daemon that starts
    /// takes over.
    pub fn runs_under_way(&self) -> Result<Vec<RunRecord>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM runs WHERE ended_unix_ms = 0 ORDER BY id"
            ))?;
            let rows = statement.query_map([], run_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// Forget the ended runs of a command beyond the newest `keep`,
    /// giving those forgotten, whose output the caller removes.
    pub fn prune_runs(
        &self,
        app: &str,
        name: &str,
        keep: usize,
    ) -> Result<Vec<RunRecord>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM runs WHERE app = ?1 AND name = ?2 \
                 AND ended_unix_ms != 0 ORDER BY id DESC LIMIT -1 OFFSET ?3"
            ))?;
            let old = statement
                .query_map(params![app, name, keep as i64], run_from_row)?
                .collect::<Result<Vec<_>, _>>()?;

            for run in &old {
                connection.execute("DELETE FROM runs WHERE id = ?1", [run.id as i64])?;
            }

            Ok(old)
        })
    }

    /// Record that a release passed a gate at `stage`, by the run `run`
    /// (issue 266).
    pub fn pass_gate(
        &self,
        app: &str,
        release: &str,
        name: &str,
        stage: &str,
        run: u64,
    ) -> Result<(), StoreError> {
        self.with(|connection| {
            connection.execute(
                "INSERT OR REPLACE INTO gates (app, release, name, stage, run, passed_unix_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![app, release, name, stage, run as i64, now_unix_ms() as i64],
            )?;
            Ok(())
        })
    }

    /// The gates a release has passed: each command's name, stage, run and
    /// when, in the order they passed.
    pub fn gates_passed(
        &self,
        app: &str,
        release: &str,
    ) -> Result<Vec<(String, String, u64, u64)>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT name, stage, run, passed_unix_ms FROM gates \
                 WHERE app = ?1 AND release = ?2 ORDER BY passed_unix_ms, name",
            )?;
            let rows = statement.query_map(params![app, release], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, i64>(3)? as u64,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// The last run of each of an application's commands that has ended,
    /// by name.
    pub fn command_runs(&self, app: &str) -> Result<Vec<(String, CommandRun)>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT name, release, started_unix_ms, ended_unix_ms, exit_code, ok, message \
                 FROM runs WHERE id IN (SELECT max(id) FROM runs WHERE app = ?1 \
                 AND ended_unix_ms != 0 GROUP BY name) ORDER BY name",
            )?;
            let rows = statement.query_map(params![app], |row| {
                let started = row.get::<_, i64>(2)? as u64;
                let ended = row.get::<_, i64>(3)? as u64;

                Ok((
                    row.get::<_, String>(0)?,
                    CommandRun {
                        release: row.get(1)?,
                        started_unix_ms: started,
                        duration_ms: ended.saturating_sub(started),
                        exit_code: row.get(4)?,
                        ok: row.get(5)?,
                        message: row.get(6)?,
                    },
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// The period each log file was last seen in (section 11.3).
    pub fn log_periods(&self) -> Result<Vec<(String, String)>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare("SELECT path, period FROM log_periods")?;
            let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// Record the period of each of these log files, in one write.
    pub fn set_log_periods(&self, periods: &[(String, String)]) -> Result<(), StoreError> {
        self.with(|connection| {
            let transaction = connection.unchecked_transaction()?;

            for (path, period) in periods {
                transaction.execute(
                    "INSERT INTO log_periods (path, period) VALUES (?1, ?2) \
                     ON CONFLICT(path) DO UPDATE SET period = excluded.period",
                    params![path, period],
                )?;
            }

            transaction.commit()?;
            Ok(())
        })
    }

    /// Forget the periods of log files that no longer exist.
    pub fn forget_log_periods(&self, paths: &[String]) -> Result<(), StoreError> {
        self.with(|connection| {
            for path in paths {
                connection.execute("DELETE FROM log_periods WHERE path = ?1", params![path])?;
            }

            Ok(())
        })
    }

    /// The file last seen at each log's path, as its device and inode
    /// (section 11.3: reopen_on_rename).
    pub fn log_files_seen(&self) -> Result<Vec<(String, u64, u64)>, StoreError> {
        self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT path, device, inode FROM log_files_seen")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    /// Replace the files last seen at the logs' paths, in one write.
    pub fn set_log_files_seen(&self, seen: &[(String, u64, u64)]) -> Result<(), StoreError> {
        self.with(|connection| {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute("DELETE FROM log_files_seen", [])?;

            for (path, device, inode) in seen {
                transaction.execute(
                    "INSERT INTO log_files_seen (path, device, inode) VALUES (?1, ?2, ?3)",
                    params![path, *device as i64, *inode as i64],
                )?;
            }

            transaction.commit()?;
            Ok(())
        })
    }

    /// Events after a sequence number, oldest first, up to `limit`.
    pub fn events_after(&self, after: u64, limit: usize) -> Result<Vec<Event>, StoreError> {
        self.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT sequence, time_unix_ms, type, subject, level, message FROM events \
                 WHERE sequence > ?1 ORDER BY sequence LIMIT ?2",
            )?;
            let rows = statement.query_map(params![after as i64, limit as i64], |row| {
                Ok(Event {
                    sequence: row.get::<_, i64>(0)? as u64,
                    time_unix_ms: row.get::<_, i64>(1)? as u64,
                    kind: row.get(2)?,
                    subject: row.get(3)?,
                    level: level_from(row.get::<_, String>(4)?),
                    message: row.get(5)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }
}

fn level_from(text: String) -> &'static str {
    match text.as_str() {
        "notice" => "notice",
        "warning" => "warning",
        "error" => "error",
        _ => "info",
    }
}

fn app_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<App> {
    Ok(App {
        name: row.get(0)?,
        current: row.get(1)?,
        previous: row.get(2)?,
        serving_origin: row
            .get::<_, Option<String>>(14)?
            .and_then(|text| crate::model::ServingOrigin::decode(&text)),
        created_unix_ms: row.get::<_, i64>(3)? as u64,
        resource_version: row.get::<_, i64>(4)? as u64,
        user: row.get(5)?,
        group: row.get(6)?,
        umask: row.get(7)?,
        priority: row.get::<_, Option<i64>>(8)?.unwrap_or(0) as i32,
        access_log: match row.get::<_, Option<String>>(9)? {
            Some(encoded) => decode(&encoded).map_err(|error: StoreError| undecodable(9, error))?,
            None => crate::model::AccessLogSpec::default(),
        },
        data_dir: row.get(10)?,
        media_dir: row.get(11)?,
        uploads_dir: row.get(12)?,
        settings: match row.get::<_, Option<String>>(13)? {
            Some(encoded) => {
                decode(&encoded).map_err(|error: StoreError| undecodable(13, error))?
            }
            None => crate::app_settings::AppSettings::default(),
        },
    })
}

/// Add the identity columns of version 4, the priority of version 9, the
/// access log choice of version 11 and the data directory of version 21
/// to an applications table that predates them.
fn add_identity_columns(connection: &Connection) -> Result<(), StoreError> {
    add_columns(
        connection,
        "apps",
        &[
            ("user", "TEXT"),
            ("grp", "TEXT"),
            ("umask", "TEXT"),
            ("priority", "INTEGER"),
            ("access_log", "TEXT"),
            ("data_dir", "TEXT"),
            ("media_dir", "TEXT"),
            ("uploads_dir", "TEXT"),
            ("settings", "TEXT"),
            ("serving_origin", "TEXT"),
        ],
    )?;
    add_columns(
        connection,
        "releases",
        &[
            ("declared", "INTEGER NOT NULL DEFAULT 0"),
            ("number", "INTEGER"),
            ("preview_secret", "TEXT"),
        ],
    )
}

/// Give a preview secret to every release that has none (version 23,
/// issue 928): the releases of an older store, and those of a copy made
/// by `muructl doctor store --copy`, which leaves the secrets out.
fn give_preview_secrets(connection: &Connection) -> Result<(), StoreError> {
    let without: Vec<i64> = {
        let mut statement =
            connection.prepare("SELECT rowid FROM releases WHERE preview_secret IS NULL")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<Result<_, _>>()?
    };

    for row in without {
        connection.execute(
            "UPDATE releases SET preview_secret = ?1 WHERE rowid = ?2",
            params![new_preview_secret()?, row],
        )?;
    }

    Ok(())
}

/// A new preview secret, or why the store cannot record a release
/// without one.
fn new_preview_secret() -> Result<String, StoreError> {
    crate::detours::secret().map_err(StoreError::Decode)
}

/// Give every release a number (version 22, issue 948): the count the
/// numbers come from is started where the highest number is, and the
/// releases of an older store, which have none, are numbered oldest
/// first. A number names the release's host directory (section 13.8),
/// which is why it is short and never an application's name or an ID.
fn number_releases(connection: &Connection) -> Result<(), StoreError> {
    let counted: i64 =
        connection.query_row("SELECT COUNT(*) FROM release_numbers", [], |row| row.get(0))?;

    if counted == 0 {
        connection.execute(
            "INSERT INTO release_numbers (last) SELECT COALESCE(MAX(number), 0) FROM releases",
            [],
        )?;
    }

    let unnumbered: Vec<i64> = {
        let mut statement = connection.prepare(
            "SELECT rowid FROM releases WHERE number IS NULL ORDER BY created_unix_ms, rowid",
        )?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<Result<_, _>>()?
    };

    for row in unnumbered {
        connection.execute("UPDATE release_numbers SET last = last + 1", [])?;
        connection.execute(
            "UPDATE releases SET number = (SELECT last FROM release_numbers) WHERE rowid = ?1",
            [row],
        )?;
    }

    Ok(())
}

/// The columns a run is read from, in `run_from_row`'s order.
const RUN_COLUMNS: &str = "id, app, name, release, trigger, requested_by, detached, exclusive, \
     inputs, timeout_ms, started_unix_ms, ended_unix_ms, exit_code, ok, message, pid, start_time, \
     relay_pid, relay_start_time, stopping, secrets";

/// A run from a row of `RUN_COLUMNS`.
fn run_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
    Ok(RunRecord {
        id: row.get::<_, i64>(0)? as u64,
        app: row.get(1)?,
        name: row.get(2)?,
        release: row.get(3)?,
        trigger: row.get(4)?,
        requested_by: row.get(5)?,
        detached: row.get(6)?,
        exclusive: row.get(7)?,
        inputs: row.get(8)?,
        timeout_ms: row.get::<_, i64>(9)? as u64,
        started_unix_ms: row.get::<_, i64>(10)? as u64,
        ended_unix_ms: row.get::<_, i64>(11)? as u64,
        exit_code: row.get(12)?,
        ok: row.get(13)?,
        message: row.get(14)?,
        pid: row.get(15)?,
        start_time: row.get::<_, i64>(16)? as u64,
        relay_pid: row.get(17)?,
        relay_start_time: row.get::<_, i64>(18)? as u64,
        stopping: row.get(19)?,
        secrets: row.get(20)?,
    })
}

/// Add to `table` each of `columns` an older store lacks.
fn add_columns(
    connection: &Connection,
    table: &str,
    columns: &[(&str, &str)],
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let existing: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;

    for (column, kind) in columns {
        if !existing.iter().any(|name| name == column) {
            connection.execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"),
                [],
            )?;
        }
    }

    Ok(())
}

/// A record that cannot be decoded is reported as a conversion failure
/// of its column, which is how rusqlite carries such errors.
fn undecodable(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

/// Writes made together in one transaction (section 10.10): those of an
/// apply's commit phase.
pub struct Writes<'a> {
    connection: &'a Connection,
}

impl Writes<'_> {
    /// Set the application's own release settings (issue 899); empty
    /// ones remove them.
    pub fn set_app_settings(
        &self,
        name: &str,
        settings: &crate::app_settings::AppSettings,
    ) -> Result<(), StoreError> {
        let encoded = if settings.is_empty() {
            None
        } else {
            Some(toml::to_string(settings).map_err(|error| StoreError::Decode(error.to_string()))?)
        };

        let changed = self.connection.execute(
            "UPDATE apps SET settings = ?2, resource_version = resource_version + 1 \
                 WHERE name = ?1",
            params![name, encoded],
        )?;

        if changed == 0 {
            return Err(StoreError::NotFound(format!("application {name}")));
        }

        Ok(())
    }

    /// Set the application's own access log choice (section 11.2); an
    /// empty one leaves the `[logs]` table's.
    pub fn set_app_access_log(
        &self,
        name: &str,
        access_log: &crate::model::AccessLogSpec,
    ) -> Result<(), StoreError> {
        let encoded = if access_log.is_empty() {
            None
        } else {
            Some(
                toml::to_string(access_log)
                    .map_err(|error| StoreError::Decode(error.to_string()))?,
            )
        };

        let changed = self.connection.execute(
            "UPDATE apps SET access_log = ?2, resource_version = resource_version + 1 \
                 WHERE name = ?1",
            params![name, encoded],
        )?;

        if changed == 0 {
            return Err(StoreError::NotFound(format!("application {name}")));
        }

        Ok(())
    }

    /// Record which release serves the application and which a rollback restores.
    pub fn set_app_releases(
        &self,
        name: &str,
        current: Option<&str>,
        previous: Option<&str>,
    ) -> Result<(), StoreError> {
        // How the serving release was set is said after, by an apply or
        // a rollback. Anything else leaves none, which reads as a
        // command having set it.

        self.connection.execute(
            "UPDATE apps SET current_release = ?2, previous_release = ?3, serving_origin = NULL, \
                 resource_version = resource_version + 1 WHERE name = ?1",
            params![name, current, previous],
        )?;
        Ok(())
    }

    /// Remember that applied files wrote the application's settings:
    /// muructl app set then says that their next apply takes its change
    /// away (ADR 0080).
    pub fn mark_settings_applied(&self, name: &str) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO ownership (kind, key, deployment) VALUES ('settings', ?1, 'apply') \
             ON CONFLICT(kind, key) DO NOTHING",
            params![name],
        )?;
        Ok(())
    }

    /// Record that applied files or a rollback set the serving release
    /// just written (ADR 0082).
    pub fn set_serving_origin(
        &self,
        name: &str,
        how: crate::model::ServingSetBy,
    ) -> Result<(), StoreError> {
        let origin = crate::model::ServingOrigin {
            how,
            at_unix_ms: now_unix_ms(),
        };
        self.connection.execute(
            "UPDATE apps SET serving_origin = ?2 WHERE name = ?1",
            params![name, origin.encode()],
        )?;
        Ok(())
    }

    /// Create or replace a virtual host.
    pub fn put_host(&self, name: &str, host: &VirtualHost) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(host).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.connection.execute(
            "INSERT INTO hosts (name, spec) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, \
                 resource_version = resource_version + 1",
            params![name, encoded],
        )?;
        Ok(())
    }

    /// Remove a virtual host and its routes.
    pub fn delete_host(&self, name: &str) -> Result<(), StoreError> {
        self.connection
            .execute("DELETE FROM routes WHERE host = ?1", [name])?;
        self.connection
            .execute("DELETE FROM host_maintenance WHERE host = ?1", [name])?;
        self.connection
            .execute("DELETE FROM hosts WHERE name = ?1", [name])?;
        Ok(())
    }

    /// Create or replace an HTTP upstream.
    pub fn put_upstream(&self, name: &str, upstream: &Upstream) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(upstream).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.connection.execute(
            "INSERT INTO upstreams (name, spec) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, \
                 resource_version = resource_version + 1",
            params![name, encoded],
        )?;
        Ok(())
    }

    /// Remove an HTTP upstream.
    pub fn delete_upstream(&self, name: &str) -> Result<(), StoreError> {
        self.connection
            .execute("DELETE FROM upstreams WHERE name = ?1", [name])?;
        Ok(())
    }

    /// Create or replace a client class, its rule tried at `position`
    /// among the applied ones.
    pub fn put_client_class(&self, class: &ClientClass, position: i64) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(class).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.connection.execute(
            "INSERT INTO client_classes (name, position, spec) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(name) DO UPDATE SET position = excluded.position, \
                 spec = excluded.spec, resource_version = resource_version + 1",
            params![class.name, position, encoded],
        )?;
        Ok(())
    }

    /// Remove a client class.
    pub fn delete_client_class(&self, name: &str) -> Result<(), StoreError> {
        self.connection
            .execute("DELETE FROM client_classes WHERE name = ?1", [name])?;
        Ok(())
    }

    /// Create or replace a rate limit.
    pub fn put_rate_limit(&self, name: &str, limit: &RateLimit) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(limit).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.connection.execute(
            "INSERT INTO rate_limits (name, spec) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, \
                 resource_version = resource_version + 1",
            params![name, encoded],
        )?;
        Ok(())
    }

    /// Remove a rate limit.
    pub fn delete_rate_limit(&self, name: &str) -> Result<(), StoreError> {
        self.connection
            .execute("DELETE FROM rate_limits WHERE name = ?1", [name])?;
        Ok(())
    }

    /// Create or replace an overlay's declaration.
    pub fn put_overlay(&self, name: &str, overlay: &Overlay) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(overlay).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.connection.execute(
            "INSERT INTO overlays (name, spec) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, \
                 resource_version = resource_version + 1",
            params![name, encoded],
        )?;
        Ok(())
    }

    /// Create or replace a condition's declaration.
    pub fn put_condition(
        &self,
        name: &str,
        condition: &muru_config::Condition,
    ) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(condition).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.connection.execute(
            "INSERT INTO conditions (name, spec) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, \
                 resource_version = resource_version + 1",
            params![name, encoded],
        )?;
        Ok(())
    }

    /// Remove a condition's declaration.
    pub fn delete_condition(&self, name: &str) -> Result<(), StoreError> {
        self.connection
            .execute("DELETE FROM conditions WHERE name = ?1", [name])?;
        Ok(())
    }

    /// Remove an overlay's declaration and its state.
    pub fn delete_overlay(&self, name: &str) -> Result<(), StoreError> {
        self.connection
            .execute("DELETE FROM overlays WHERE name = ?1", [name])?;
        self.connection
            .execute("DELETE FROM overlay_state WHERE name = ?1", [name])?;
        self.connection
            .execute("DELETE FROM overlay_activations WHERE name = ?1", [name])?;
        Ok(())
    }

    /// Forget what holds an overlay back while it is not active: a skip,
    /// an advisory window or an end by hand (issue 755).
    pub fn clear_overlay_hold(&self, name: &str) -> Result<(), StoreError> {
        self.connection.execute(
            "DELETE FROM overlay_state WHERE name = ?1 AND active = 0",
            [name],
        )?;
        Ok(())
    }

    /// Create or replace a route, identified by its host and key: the
    /// path, and the client classes it is for (section 4.14).
    pub fn put_route(&self, route: &Route) -> Result<(), StoreError> {
        let encoded =
            toml::to_string(route).map_err(|error| StoreError::Decode(error.to_string()))?;
        self.connection.execute(
            "INSERT INTO routes (host, path, spec) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(host, path) DO UPDATE SET spec = excluded.spec, \
                 resource_version = resource_version + 1",
            params![route.host, route.key(), encoded],
        )?;
        Ok(())
    }

    /// Remove a route, by its host and key.
    pub fn delete_route(&self, host: &str, path: &str) -> Result<(), StoreError> {
        self.connection.execute(
            "DELETE FROM routes WHERE host = ?1 AND path = ?2",
            params![host, path],
        )?;
        Ok(())
    }
}

/// A stored definition, as TOML, back as its type.
fn decode<T: serde::de::DeserializeOwned>(encoded: &str) -> Result<T, StoreError> {
    toml::from_str(encoded).map_err(|error| StoreError::Decode(error.to_string()))
}

fn release_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Release> {
    let encoded: String = row.get(2)?;
    let spec: ReleaseSpec = toml::from_str(&encoded).map_err(|error| undecodable(2, error))?;
    let state: String = row.get(3)?;
    let state = state
        .parse::<ReleaseState>()
        .map_err(|error| undecodable(3, std::io::Error::other(error)))?;

    Ok(Release {
        app: row.get(0)?,
        id: row.get(1)?,
        spec,
        state,
        message: row.get(4)?,
        created_unix_ms: row.get::<_, i64>(5)? as u64,
        resource_version: row.get::<_, i64>(6)? as u64,
        number: row.get::<_, Option<i64>>(7)?.unwrap_or(0) as u64,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::ScalingSpec;
    use muru_config::{Interface, Preload};

    /// A release's spec, for this crate's tests.
    pub(crate) fn spec() -> ReleaseSpec {
        ReleaseSpec {
            metadata: std::collections::BTreeMap::new(),
            limits: crate::model::ServedLimits::default(),
            source: PathBuf::from("/srv/shop"),
            entrypoint: "shop.wsgi:application".to_owned(),
            interface: Interface::Auto,
            python: PathBuf::from("/srv/shop/.venv/bin/python"),
            processes: 2,
            initial_processes: None,
            threads: 4,
            sync_threads: 8,
            concurrency: 1000,
            preload: Preload::Auto,
            hooks: None,
            preload_modules: Vec::new(),
            instrumentation: Vec::new(),
            lazy_imports: false,
            import_report: false,
            factory: None,
            reflection: false,
            handoff: None,
            grpc_web_origins: Vec::new(),
            groups: Vec::new(),
            mounts: Vec::new(),
            static_dirs: Vec::new(),
            logging: Default::default(),
            max_requests: None,
            max_lifetime_ms: None,
            max_surge: muru_config::DEFAULT_MAX_SURGE,
            max_unavailable: muru_config::DEFAULT_MAX_UNAVAILABLE,
            timeouts: Default::default(),
            memory: Default::default(),
            env: Vec::new(),
            scaling: ScalingSpec::default(),
            accel_redirect: false,
            cgi_local_redirect: false,
            sendfile_roots: Vec::new(),
            sendfile_header: None,
            error_pages: Default::default(),
            error_override: None,
            maintenance: None,
            http: None,
            layers: crate::model::Layers::default(),
            interpreter: None,
            build: crate::model::BuildSpec::default(),
            bytecode: crate::model::BytecodeSpec::default(),
            commands: Vec::new(),
        }
    }

    #[test]
    fn releases_are_created_and_advance() {
        let store = Store::in_memory().unwrap();
        store.ensure_app("shop").unwrap();
        assert_eq!(store.next_release_id("shop").unwrap(), "r1");

        let release = store.create_release("shop", "r1", &spec()).unwrap();
        assert_eq!(release.state, ReleaseState::Pending);
        assert_eq!(release.spec, spec());
        assert_eq!(store.next_release_id("shop").unwrap(), "r2");

        store
            .set_release_state("shop", "r1", ReleaseState::Ready, "")
            .unwrap();
        store.set_app_releases("shop", Some("r1"), None).unwrap();
        assert_eq!(
            store.release("shop", "r1").unwrap().state,
            ReleaseState::Ready
        );
        assert_eq!(store.app("shop").unwrap().current.as_deref(), Some("r1"));
        assert!(store.release("shop", "r9").is_err());
    }

    #[test]
    fn a_sound_store_breaks_no_rule_and_a_broken_one_says_which() {
        let store = Store::in_memory().unwrap();
        store.ensure_app("shop").unwrap();
        store.create_release("shop", "r1", &spec()).unwrap();
        store.set_app_releases("shop", Some("r1"), None).unwrap();
        store
            .append_event("release.ready", "shop@r1", "info", "ready")
            .unwrap();
        store
            .append_event("release.ready", "shop@r1", "info", "again")
            .unwrap();

        assert!(store.integrity().unwrap().is_empty());
        assert!(store.foreign_key_faults().unwrap().is_empty());
        assert_eq!(store.schema_version().unwrap(), Some(SCHEMA_VERSION));
        assert!(
            store
                .rule_breaks()
                .unwrap()
                .iter()
                .all(|(_, breaks)| breaks.is_empty())
        );

        let counts = store.table_counts().unwrap();
        assert!(counts.contains(&("releases".to_owned(), 1)), "{counts:?}");
        assert!(counts.contains(&("events".to_owned(), 2)), "{counts:?}");
        assert!(
            store
                .schema_sql()
                .unwrap()
                .contains("CREATE TABLE releases (")
        );

        // A serving release that is gone, a release with no application
        // and a gap in the events are each a break of their own rule.

        store
            .with(|connection| {
                connection.pragma_update(None, "foreign_keys", "OFF")?;
                connection.execute("UPDATE apps SET current_release = 'r9'", [])?;
                connection.execute(
                    "INSERT INTO releases (app, id, spec, state, created_unix_ms, number) \
                     VALUES ('gone', 'r1', '', 'stopped', 1, 1)",
                    [],
                )?;
                connection.execute(
                    "INSERT INTO events (sequence, time_unix_ms, type, subject, level, message) \
                     VALUES (9, 1, 'a', 'b', 'info', 'c')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        let breaks: Vec<String> = store
            .rule_breaks()
            .unwrap()
            .into_iter()
            .flat_map(|(_, breaks)| breaks)
            .collect();
        assert_eq!(
            breaks,
            [
                "release gone@r1 has no application",
                "application shop serves r9, which is no release",
                "the number 1 names 2 releases",
                "the events run from 1 to 9 and there are 3",
            ]
        );
        assert_eq!(
            store.foreign_key_faults().unwrap(),
            ["releases row 2 names no row of apps"]
        );
    }

    #[test]
    fn a_copy_of_a_store_is_a_whole_store_and_the_original_is_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("muru.sqlite");

        {
            let store = Store::open(&path).unwrap();
            store.ensure_app("shop").unwrap();
            store.create_release("shop", "r1", &spec()).unwrap();
        }

        let before = std::fs::read(&path).unwrap();
        let copy = directory.path().join("copy.sqlite");
        Store::open_read_only(&path)
            .unwrap()
            .copy_to(&copy)
            .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), before);
        let copied = Store::open_read_only(&copy).unwrap();
        assert_eq!(copied.release("shop", "r1").unwrap().number, 1);
        assert!(copied.integrity().unwrap().is_empty());

        // The copy holds no release's preview secret, in its rows or in
        // the bytes of its file, and a daemon that opens it gives each a
        // new one.

        let secret = Store::open_read_only(&path)
            .unwrap()
            .preview_secret("shop", "r1")
            .unwrap();
        assert!(copied.preview_secret("shop", "r1").is_err());
        drop(copied);
        let bytes = std::fs::read(&copy).unwrap();
        assert!(
            !bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes())
        );
        let opened = Store::open(&copy).unwrap();
        assert_ne!(opened.preview_secret("shop", "r1").unwrap(), secret);

        // A copy never replaces a file that is there.

        assert!(
            Store::open_read_only(&path)
                .unwrap()
                .copy_to(&copy)
                .is_err()
        );
    }

    #[test]
    fn a_release_has_a_number_that_is_never_given_twice() {
        let store = Store::in_memory().unwrap();
        store.ensure_app("shop").unwrap();
        store.ensure_app("blog").unwrap();

        let first = store.create_release("shop", "r1", &spec()).unwrap();
        let second = store.create_release("blog", "r1", &spec()).unwrap();
        assert_eq!((first.number, second.number), (1, 2));

        // The highest number is not given again once its release is gone.

        store.delete_release("blog", "r1").unwrap();
        let third = store.create_release("blog", "r2", &spec()).unwrap();
        assert_eq!(third.number, 3);
        assert_eq!(store.releases(None).unwrap().len(), 2);
    }

    #[test]
    fn a_release_keeps_its_preview_secret_until_it_is_given_a_new_one() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("muru.sqlite");

        let (first, second) = {
            let store = Store::open(&path).unwrap();
            store.ensure_app("shop").unwrap();
            store.create_release("shop", "r1", &spec()).unwrap();
            store.create_release("shop", "r2", &spec()).unwrap();
            (
                store.preview_secret("shop", "r1").unwrap(),
                store.preview_secret("shop", "r2").unwrap(),
            )
        };

        // 128 bits in hexadecimal, each release its own.

        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);

        // A store opened again holds the same secrets, and one from
        // before releases had them gives a secret to each that has none.

        let store = Store::open(&path).unwrap();
        assert_eq!(store.preview_secret("shop", "r1").unwrap(), first);
        store
            .with(|connection| {
                connection.execute(
                    "UPDATE releases SET preview_secret = NULL WHERE id = 'r2'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.preview_secret("shop", "r1").unwrap(), first);
        let given = store.preview_secret("shop", "r2").unwrap();
        assert!(given.len() == 32 && given != second);

        // A new secret replaces the old, for that release alone, and the
        // release's record carries no secret to print.

        let renewed = store.renew_preview_secret("shop", "r1").unwrap();
        assert_ne!(renewed, first);
        assert_eq!(store.preview_secret("shop", "r1").unwrap(), renewed);
        assert_eq!(store.preview_secret("shop", "r2").unwrap(), given);
        assert_eq!(store.preview_secrets().unwrap().len(), 2);
        assert!(store.renew_preview_secret("shop", "r9").is_err());
        assert!(store.preview_secret("shop", "r9").is_err());
        let record = format!("{:?}", store.release("shop", "r1").unwrap());
        assert!(!record.contains(&renewed));
    }

    #[test]
    fn the_releases_of_an_older_store_are_numbered_oldest_first() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("muru.sqlite");

        {
            let store = Store::open(&path).unwrap();
            store.ensure_app("shop").unwrap();
            store.create_release("shop", "r1", &spec()).unwrap();
            store.create_release("shop", "r2", &spec()).unwrap();

            store
                .with(|connection| {
                    connection.execute("UPDATE releases SET number = NULL", [])?;
                    connection.execute("DELETE FROM release_numbers", [])?;
                    connection.execute(
                        "UPDATE releases SET created_unix_ms = 5 WHERE id = 'r2'",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
        }

        let store = Store::open(&path).unwrap();
        assert_eq!(store.release("shop", "r2").unwrap().number, 1);
        assert_eq!(store.release("shop", "r1").unwrap().number, 2);
        assert_eq!(
            store.create_release("shop", "r3", &spec()).unwrap().number,
            3
        );
    }

    #[test]
    fn an_import_report_is_kept_beside_its_release_and_removed_with_it() {
        let store = Store::in_memory().unwrap();
        store.ensure_app("shop").unwrap();
        store.create_release("shop", "r1", &spec()).unwrap();
        assert_eq!(store.import_report("shop", "r1").unwrap(), None);

        store.put_import_report("shop", "r1", "{\"a\":1}").unwrap();
        store.put_import_report("shop", "r1", "{\"a\":2}").unwrap();
        assert_eq!(
            store.import_report("shop", "r1").unwrap().as_deref(),
            Some("{\"a\":2}")
        );

        store.delete_release("shop", "r1").unwrap();
        assert_eq!(store.import_report("shop", "r1").unwrap(), None);
    }

    #[test]
    fn a_store_opened_read_only_is_left_exactly_as_it_was() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("muru.sqlite");

        // A store at an older schema, with no daemon running.

        {
            let store = Store::open(&path).unwrap();
            store.ensure_app("shop").unwrap();
        }

        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch("DROP TABLE log_periods; UPDATE schema SET version = 12;")
                .unwrap();
        }

        let before = std::fs::read(&path).unwrap();
        let store = Store::open_read_only(&path).unwrap();
        assert_eq!(store.apps().unwrap().len(), 1);
        assert!(
            store
                .set_log_periods(&[("/a".to_owned(), "d1".to_owned())])
                .is_err()
        );
        drop(store);

        assert_eq!(std::fs::read(&path).unwrap(), before);

        let mut names: Vec<String> = std::fs::read_dir(directory.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["muru.sqlite"], "nothing is left beside it");

        // What only a write-ahead log holds is still read.

        let writer = Store::open(&path).unwrap();
        writer.ensure_app("later").unwrap();
        let reader = Store::open_read_only(&path).unwrap();
        assert_eq!(reader.apps().unwrap().len(), 2);
    }

    #[test]
    fn log_periods_are_kept_by_path_and_can_be_forgotten() {
        let store = Store::in_memory().unwrap();
        let record = |path: &str, period: &str| (path.to_owned(), period.to_owned());

        store
            .set_log_periods(&[record("/a.log", "d1"), record("/b.log", "d1")])
            .unwrap();
        store.set_log_periods(&[record("/a.log", "d2")]).unwrap();
        store.forget_log_periods(&["/b.log".to_owned()]).unwrap();
        assert_eq!(store.log_periods().unwrap(), vec![record("/a.log", "d2")]);
    }

    /// A run of `name` for `app` as it starts.
    fn starting(app: &str, name: &str, exclusive: bool) -> RunRecord {
        RunRecord {
            app: app.to_owned(),
            name: name.to_owned(),
            release: "r1".to_owned(),
            trigger: "manual".to_owned(),
            requested_by: "you".to_owned(),
            exclusive,
            started_unix_ms: 1_000,
            ..RunRecord::default()
        }
    }

    #[test]
    fn runs_are_kept_from_their_start_to_their_end_and_listed_under_way_first() {
        let store = Store::in_memory().unwrap();
        let first = store
            .begin_run(&starting("shop", "migrate", false))
            .unwrap()
            .unwrap();
        let second = store
            .begin_run(&starting("shop", "migrate", false))
            .unwrap()
            .unwrap();
        let other = store
            .begin_run(&starting("shop", "backup", false))
            .unwrap()
            .unwrap();
        store
            .begin_run(&starting("blog", "migrate", false))
            .unwrap()
            .unwrap();

        store.set_run_process(first, 4242, 77, 4241, 76).unwrap();
        store
            .end_run(first, 1_020, 1, false, "exited with status 1 after 20ms")
            .unwrap();
        store
            .end_run(other, 1_050, 0, true, "exited with status 0 after 50ms")
            .unwrap();

        let listed: Vec<(u64, bool)> = store
            .runs("shop", None)
            .unwrap()
            .iter()
            .map(|run| (run.id, run.under_way()))
            .collect();
        assert_eq!(listed, [(second, true), (other, false), (first, false)]);
        assert_eq!(store.runs("shop", Some("backup")).unwrap().len(), 1);

        let ended = store.run(first).unwrap();
        assert_eq!(
            (ended.pid, ended.start_time, ended.relay_pid),
            (4242, 77, 4241)
        );
        assert_eq!((ended.exit_code, ended.ok), (1, false));

        // The last run of each command is the newest that has ended.

        let last: Vec<(String, i32, u64)> = store
            .command_runs("shop")
            .unwrap()
            .into_iter()
            .map(|(name, run)| (name, run.exit_code, run.duration_ms))
            .collect();
        assert_eq!(
            last,
            [("backup".to_owned(), 0, 50), ("migrate".to_owned(), 1, 20)]
        );
        assert_eq!(store.runs_under_way().unwrap().len(), 2);

        store.ensure_app("shop").unwrap();
        store.delete_app("shop").unwrap();
        assert!(store.runs("shop", None).unwrap().is_empty());
        assert_eq!(store.runs("blog", None).unwrap().len(), 1);
    }

    #[test]
    fn an_exclusive_command_is_claimed_by_one_run_until_it_ends() {
        let store = Store::in_memory().unwrap();
        let holder = store
            .begin_run(&starting("shop", "backup", true))
            .unwrap()
            .unwrap();

        assert_eq!(
            store.begin_run(&starting("shop", "backup", true)).unwrap(),
            Err(holder)
        );

        // Another command, or another application's, is not held back.

        assert!(
            store
                .begin_run(&starting("shop", "migrate", true))
                .unwrap()
                .is_ok()
        );
        assert!(
            store
                .begin_run(&starting("blog", "backup", true))
                .unwrap()
                .is_ok()
        );

        store
            .end_run(holder, 2_000, 0, true, "exited with status 0 after 1s")
            .unwrap();
        assert!(
            store
                .begin_run(&starting("shop", "backup", true))
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn only_the_newest_ended_runs_of_a_command_are_kept() {
        let store = Store::in_memory().unwrap();
        let ids: Vec<u64> = (0..5)
            .map(|_| {
                store
                    .begin_run(&starting("shop", "migrate", false))
                    .unwrap()
                    .unwrap()
            })
            .collect();

        for id in &ids[..4] {
            store.end_run(*id, 2_000, 0, true, "done").unwrap();
        }

        // The run under way is never forgotten, whatever its age.

        let forgotten: Vec<u64> = store
            .prune_runs("shop", "migrate", 2)
            .unwrap()
            .iter()
            .map(|run| run.id)
            .collect();
        assert_eq!(forgotten, [ids[1], ids[0]]);

        let kept: Vec<u64> = store
            .runs("shop", None)
            .unwrap()
            .iter()
            .map(|run| run.id)
            .collect();
        assert_eq!(kept, [ids[4], ids[3], ids[2]]);
    }

    #[test]
    fn hosts_in_maintenance_are_kept_until_turned_off_or_removed() {
        let store = Store::in_memory().unwrap();
        store.put_host("shop", &VirtualHost::default()).unwrap();
        store.put_host("blog", &VirtualHost::default()).unwrap();
        assert!(store.host_maintenance().unwrap().is_empty());

        store.set_host_maintenance("shop", Some(1000)).unwrap();
        store.set_host_maintenance("shop", Some(2000)).unwrap();
        store.set_host_maintenance("blog", Some(3000)).unwrap();
        assert_eq!(
            store.host_maintenance().unwrap(),
            [("blog".to_owned(), 3000), ("shop".to_owned(), 1000)],
            "turning it on again keeps when it began"
        );

        store.set_host_maintenance("blog", None).unwrap();
        store.delete_host("shop").unwrap();
        assert!(store.host_maintenance().unwrap().is_empty());
    }

    #[test]
    fn sink_cursors_are_kept_by_name() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.sink_cursor("ops").unwrap(), None);
        assert_eq!(store.last_event_sequence().unwrap(), 0);
        let event = store
            .append_event("daemon.started", "daemon", "info", "up")
            .unwrap();
        assert_eq!(store.last_event_sequence().unwrap(), event.sequence);
        store.set_sink_cursor("ops", event.sequence).unwrap();
        store.set_sink_cursor("ops", event.sequence + 1).unwrap();
        assert_eq!(store.sink_cursor("ops").unwrap(), Some(event.sequence + 1));
    }

    #[test]
    fn an_event_recorded_late_keeps_the_time_it_happened_and_takes_the_next_sequence() {
        let store = Store::in_memory().unwrap();
        let first = store
            .append_event("daemon.started", "daemon", "info", "up")
            .unwrap();
        let held = store
            .append_event_at(
                "worker.recycled",
                "shop@r1",
                "info",
                "held",
                Some(1_700_000_000_000),
            )
            .unwrap();
        assert_eq!(held.sequence, first.sequence + 1);
        assert_eq!(held.time_unix_ms, 1_700_000_000_000);

        // A time ahead of the clock is taken as now.

        let ahead = store
            .append_event_at(
                "worker.recycled",
                "shop@r1",
                "info",
                "ahead",
                Some(u64::MAX / 2),
            )
            .unwrap();
        assert!(ahead.time_unix_ms >= first.time_unix_ms);
        assert!(ahead.time_unix_ms <= now_unix_ms());

        let times: Vec<u64> = store
            .events_after(first.sequence, 10)
            .unwrap()
            .iter()
            .map(|event| event.time_unix_ms)
            .collect();
        assert_eq!(times, [held.time_unix_ms, ahead.time_unix_ms]);
    }

    #[test]
    fn events_are_numbered_and_listed_after_a_sequence() {
        let store = Store::in_memory().unwrap();
        let first = store
            .append_event("release.ready", "shop@r1", "info", "ready")
            .unwrap();
        let second = store
            .append_event("release.failed", "shop@r2", "error", "no")
            .unwrap();

        assert_eq!(second.sequence, first.sequence + 1);
        let after = store.events_after(first.sequence, 10).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].kind, "release.failed");
        assert_eq!(after[0].level, "error");
    }

    #[test]
    fn client_classes_keep_their_order_and_limits_their_names() {
        let store = Store::in_memory().unwrap();
        let class = |name: &str| ClientClass {
            name: name.to_owned(),
            user_agent: vec![name.to_owned()],
            ..ClientClass::default()
        };
        store.put_client_class(&class("zeta"), 0).unwrap();
        store.put_client_class(&class("alpha"), 1).unwrap();
        store.put_client_class(&class("zeta"), 2).unwrap();

        let names: Vec<String> = store
            .client_classes()
            .unwrap()
            .into_iter()
            .map(|(_, class)| class.name)
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);

        store.delete_client_class("alpha").unwrap();
        assert_eq!(store.client_classes().unwrap().len(), 1);

        let limit = RateLimit {
            rate: "2/s".to_owned(),
            ..RateLimit::default()
        };
        store.put_rate_limit("crawlers", &limit).unwrap();
        assert_eq!(
            store.rate_limits().unwrap(),
            [("crawlers".to_owned(), limit)]
        );
        store.delete_rate_limit("crawlers").unwrap();
        assert!(store.rate_limits().unwrap().is_empty());
    }
}
