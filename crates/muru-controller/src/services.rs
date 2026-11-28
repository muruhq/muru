//! Services (design specification section 9.13): named commands the
//! daemon keeps running beside the applications, which take no requests
//! from the edge. Their processes are run by the supervisor managed HTTP
//! servers share (`supervisor.rs`), under the one restart policy (ADR
//! 0090), each instance with a log relay of its own as its parent.

use muru_config::Service as Table;

/// How many instances a table declares.
pub fn instances_of(table: &Table) -> u32 {
    table.instances.unwrap_or(1).max(1)
}

/// Whether a table's service starts with the daemon.
pub fn autostart_of(table: &Table) -> bool {
    table.autostart.unwrap_or(true)
}

/// A count of instances as a phrase: `1 instance`, `3 instances`.
pub fn counted(count: u32) -> String {
    format!("{count} instance{}", if count == 1 { "" } else { "s" })
}

/// What a changed definition does to a service that runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Difference {
    /// The keys whose change stops every instance and starts it again:
    /// what a process is started with cannot be changed under it.
    pub restart: Vec<&'static str>,
    /// The count of instances before and after, when it changed.
    pub instances: Option<(u32, u32)>,
    /// The keys whose change is in force at once, with nothing restarted.
    pub in_place: Vec<&'static str>,
    /// Whether the service starts with the daemon, when that changed.
    pub autostart: Option<bool>,
}

impl Difference {
    /// What differs between two definitions of one service.
    pub fn between(old: &Table, new: &Table) -> Self {
        let mut difference = Self::default();

        for (key, changed) in [
            ("command", old.command != new.command),
            ("cwd", old.cwd != new.cwd),
            ("env", old.env != new.env),
            ("user", old.user != new.user),
            ("group", old.group != new.group),
            ("umask", old.umask != new.umask),
            ("readiness", old.readiness != new.readiness),
            ("stop_signal", old.stop_signal != new.stop_signal),
            ("stop_group", old.stop_group != new.stop_group),
        ] {
            if changed {
                difference.restart.push(key);
            }
        }

        for (key, changed) in [
            ("restart", old.restart != new.restart),
            ("stop_timeout", old.stop_timeout != new.stop_timeout),
            ("reload_signal", old.reload_signal != new.reload_signal),
            ("description", old.description != new.description),
        ] {
            if changed {
                difference.in_place.push(key);
            }
        }

        let (before, after) = (instances_of(old), instances_of(new));

        if before != after {
            difference.instances = Some((before, after));
        }

        if autostart_of(old) != autostart_of(new) {
            difference.autostart = Some(autostart_of(new));
        }

        difference
    }

    /// The difference as a plan says it, with what it does.
    pub fn describe(&self, new: &Table) -> String {
        let mut parts = Vec::new();

        if !self.restart.is_empty() {
            let count = instances_of(new);
            parts.push(format!(
                "{} changed; its {} {} restarted",
                self.restart.join(", "),
                counted(count),
                if count == 1 { "is" } else { "are" }
            ));
        } else if let Some((before, after)) = self.instances {
            let by = before.abs_diff(after);
            parts.push(format!(
                "instances {before} to {after}; {} {} {}",
                counted(by),
                if by == 1 { "is" } else { "are" },
                if after > before { "started" } else { "stopped" }
            ));
        }

        if !self.in_place.is_empty() {
            parts.push(format!(
                "{} changed; nothing is restarted",
                self.in_place.join(", ")
            ));
        }

        match self.autostart {
            Some(true) => parts.push("autostart on; it is started".to_owned()),
            Some(false) => parts.push("autostart off; it keeps running until stopped".to_owned()),
            None => {}
        }

        parts.join("; ")
    }
}

/// A service's definition for a plan: for a new one its command, and
/// for a changed one what the change does to its instances.
pub fn describe_change(recorded: Option<&Table>, table: &Table) -> String {
    let Some(recorded) = recorded else {
        let mut text = table.command.join(" ");
        let count = instances_of(table);

        if count > 1 {
            text.push_str(&format!(", {}", counted(count)));
        }

        if !autostart_of(table) {
            text.push_str(", not started automatically");
        }

        return text;
    };

    Difference::between(recorded, table).describe(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_plan_says_what_a_change_does_to_the_instances() {
        let redis = table("command = ['redis-server', '--port', '6379']");
        assert_eq!(describe_change(None, &redis), "redis-server --port 6379");
        assert_eq!(
            describe_change(
                None,
                &table("command = ['x']\ninstances = 4\nautostart = false")
            ),
            "x, 4 instances, not started automatically"
        );

        // What a process was started with restarts it.

        let moved = table("command = ['redis-server', '--port', '6380']\nenv = { A = 'b' }");
        assert_eq!(
            describe_change(Some(&redis), &moved),
            "command, env changed; its 1 instance is restarted"
        );

        // A count alone starts or stops the difference.

        let more = table("command = ['redis-server', '--port', '6379']\ninstances = 3");
        assert_eq!(
            describe_change(Some(&redis), &more),
            "instances 1 to 3; 2 instances are started"
        );
        assert_eq!(
            describe_change(Some(&more), &redis),
            "instances 3 to 1; 2 instances are stopped"
        );

        // The policy and the rest are in force at once.

        let policy = table(
            "command = ['redis-server', '--port', '6379']\nstop_timeout = '30s'\n\
             restart = { give_up_after = 5 }\nautostart = false",
        );
        assert_eq!(
            describe_change(Some(&redis), &policy),
            "restart, stop_timeout changed; nothing is restarted; \
             autostart off; it keeps running until stopped"
        );
    }
}
