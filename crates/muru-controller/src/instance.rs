//! Which instance this process is (design specification sections 2.1
//! and 13.10, issue 806): its ID and its tag, set once at start by the
//! daemon, a development run or `muructl serve`, and given to every
//! process it starts in `MURU_INSTANCE_ID` and `MURU_INSTANCE`. Workers
//! inherit them from their host, and so do the subprocesses a worker
//! starts, which is how a process with no title of Muru's is attributed
//! to its instance.
//!
//! The tag names the instance in titles: `user` or `system` for the
//! default daemons, a named instance's name, `run:PORT` for a run,
//! `serve` for a serve, and the short ID for a daemon whose locations
//! were given by flags.

use std::sync::OnceLock;

/// The variable holding the instance's full ID.
pub const ID_VARIABLE: &str = "MURU_INSTANCE_ID";

/// The variable holding the instance's tag.
pub const TAG_VARIABLE: &str = "MURU_INSTANCE";

/// This process's instance, once set: its ID, its tag and its label.
static IDENTITY: OnceLock<(String, String, Option<String>)> = OnceLock::new();

/// Say which instance this process is, once, before it starts anything.
/// The label is `MURU_INSTANCE_LABEL` from this process's environment,
/// which its processes carry too, so what a test session leaves behind
/// is known by its label after the instance has gone.
pub fn set(id: crate::registry::InstanceId, tag: &str) {
    let label = std::env::var(crate::registry::LABEL_VARIABLE)
        .ok()
        .filter(|label| !label.is_empty());
    let _ = IDENTITY.set((id.to_string(), tag.to_owned(), label));
}

/// This process's instance tag, when it has one.
pub fn tag() -> Option<&'static str> {
    IDENTITY.get().map(|(_, tag, _)| tag.as_str())
}

/// The variables every process this instance starts is given; none
/// before [`set`].
pub fn variables() -> Vec<(&'static str, String)> {
    let Some((id, tag, label)) = IDENTITY.get() else {
        return Vec::new();
    };

    let mut variables = vec![(ID_VARIABLE, id.clone()), (TAG_VARIABLE, tag.clone())];

    if let Some(label) = label {
        variables.push((crate::registry::LABEL_VARIABLE, label.clone()));
    }

    variables
}

/// A daemon's or an edge's title: `muru ROLE SCOPE`, with the tag after it
/// in parentheses when the scope, a state directory, does not already
/// name the instance.
pub fn scoped_title(role: &str, scope: &str, tag: Option<&str>) -> String {
    match tag.filter(|tag| !tag.is_empty() && *tag != scope) {
        Some(tag) => format!("muru {role} {scope} ({tag})"),
        None => format!("muru {role} {scope}").trim_end().to_owned(),
    }
}

/// `title` with the tag in the environment after it in parentheses, as a
/// host's, a relay's and a worker's titles carry it.
pub fn tagged_from_environment(title: &str) -> String {
    match std::env::var(TAG_VARIABLE)
        .ok()
        .filter(|tag| !tag.is_empty())
    {
        Some(tag) => format!("{title} ({tag})"),
        None => title.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_that_names_the_instance_is_not_repeated() {
        assert_eq!(
            scoped_title("daemon", "user", Some("user")),
            "muru daemon user"
        );
        assert_eq!(
            scoped_title("edge", "/srv/state", Some("7f3a9c2e")),
            "muru edge /srv/state (7f3a9c2e)"
        );
        assert_eq!(scoped_title("edge", "", None), "muru edge");
    }
}
