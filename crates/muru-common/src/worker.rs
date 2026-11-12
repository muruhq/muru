//! A worker as people name it: by its group and its index within the
//! group, `0` for a worker of the default group and `crawlers/0` for one
//! of a group named `crawlers`. The process title, the socket's name, the
//! access log, `muructl status --processes` and `--worker` all use it.

use std::fmt;

/// The group a worker belongs to when none is named.
pub const DEFAULT_GROUP: &str = "default";

/// A worker by its group and its index within the group.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkerName {
    /// Its group, `default` when none is named.
    pub group: String,
    /// Its index within the group.
    pub index: u32,
}

impl WorkerName {
    /// A worker of `group`; an empty group is the default one.
    pub fn new(group: &str, index: u32) -> Self {
        Self {
            group: if group.is_empty() {
                DEFAULT_GROUP.to_owned()
            } else {
                group.to_owned()
            },
            index,
        }
    }

    /// Read `N` (a worker of the default group) or `GROUP/N`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (group, index) = match text.rsplit_once('/') {
            Some((group, index)) if !group.is_empty() => (group, index),
            Some(_) => return Err(format!("{text}: a group is named before the slash")),
            None => (DEFAULT_GROUP, text),
        };

        let index = index.parse::<u32>().map_err(|_| {
            format!("{text}: a worker is its index, or its group and index as GROUP/N")
        })?;

        Ok(Self::new(group, index))
    }

    /// Whether it belongs to the default group.
    pub fn in_default_group(&self) -> bool {
        self.group == DEFAULT_GROUP
    }
}

impl fmt::Display for WorkerName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.in_default_group() {
            write!(formatter, "{}", self.index)
        } else {
            write!(formatter, "{}/{}", self.group, self.index)
        }
    }
}

impl std::str::FromStr for WorkerName {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_is_named_by_its_index_or_its_group_and_index() {
        assert_eq!(WorkerName::parse("1"), Ok(WorkerName::new("default", 1)));
        assert_eq!(
            WorkerName::parse("crawlers/0"),
            Ok(WorkerName::new("crawlers", 0))
        );
        assert_eq!(
            WorkerName::parse("default/2"),
            Ok(WorkerName::new("default", 2))
        );
        assert!(WorkerName::parse("/0").is_err());
        assert!(WorkerName::parse("crawlers").is_err());
        assert!(WorkerName::parse("crawlers/x").is_err());

        assert_eq!(WorkerName::new("", 3).to_string(), "3");
        assert_eq!(WorkerName::new("crawlers", 0).to_string(), "crawlers/0");
    }
}
