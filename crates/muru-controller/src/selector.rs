//! Choosing one running instance by what a person types (design
//! specification section 13.8, issue 868, ADR 0074).
//!
//! Most of what runs on a development machine was never given a name:
//! every `muructl run`, and every daemon started with its own
//! `--state-dir` and `--run-dir`. A value given to select one is matched
//! against the live registry records by these rules in turn, and the
//! first rule that matches anything decides:
//!
//! 1. the instance's name;
//!
//! 2. its tag, as `muructl instance list` prints it (`run:9080`, or the
//!    short ID of an unnamed daemon);
//!
//! 3. a prefix of its ID of at least [`MIN_PREFIX`] hex digits, as git
//!    takes a short hash;
//!
//! 4. its label, from `--label` or `MURU_INSTANCE_LABEL`;
//!
//! 5. its application, for a run or a serve.
//!
//! A rule that matches more than one instance is an error listing each,
//! with the value that selects it, rather than a reason to try the next
//! rule: two instances serving applications of one name is ordinary, and
//! a convenience that guesses is worse than one that fails loudly.
//!
//! The defined named instances and the default daemons, which a command
//! reaches by their locations even when they are not running, are the
//! caller's to try before this. Nothing here reads the registry, so
//! whatever lists candidates (the instance commands, and the commands
//! that choose among the detours of one daemon) shares the same rules.

use crate::registry::Record;

/// The fewest hex digits of an ID that select by prefix.
pub const MIN_PREFIX: usize = 4;

/// Which rule chose an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Its name.
    Name,
    /// Its tag.
    Tag,
    /// A prefix of its ID.
    Id,
    /// Its label.
    Label,
    /// Its application.
    App,
}

impl Rule {
    /// The rule as a sentence names what it matched: `the label`.
    pub fn what(self) -> &'static str {
        match self {
            Self::Name => "the name",
            Self::Tag => "the tag",
            Self::Id => "the start of the ID",
            Self::Label => "the label",
            Self::App => "the application",
        }
    }
}

/// The instance a value selected, and by which rule.
#[derive(Debug, Clone, Copy)]
pub struct Choice<'a> {
    /// The instance's record.
    pub record: &'a Record,
    /// The rule that chose it.
    pub rule: Rule,
}

/// One instance a refusal offers instead, with the value that selects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// What to pass to select it.
    pub value: String,
    /// The instance, as a sentence names it.
    pub description: String,
}

/// Why a value selected no instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Nothing is running to select from.
    NoneRunning {
        /// The value given.
        wanted: String,
    },
    /// No rule matched.
    NoMatch {
        /// The value given.
        wanted: String,
        /// Every instance there is, with the value that selects it.
        candidates: Vec<Candidate>,
    },
    /// The first rule to match matched more than one.
    Ambiguous {
        /// The value given.
        wanted: String,
        /// The rule that matched them.
        rule: Rule,
        /// The instances it matched, with the value that selects each.
        candidates: Vec<Candidate>,
    },
}

/// The most candidates a refusal lists; the rest are counted.
pub const MAX_LISTED: usize = 5;

impl Refusal {
    /// The refusal as text. `instead` says what to do with a candidate,
    /// such as `pass one of these to --instance`, and `listing` names the
    /// command that lists every instance, such as `muructl instance list`.
    /// An ambiguous value lists the instances it matched, one to a line
    /// with the value that selects each, at most [`MAX_LISTED`]; a value
    /// that matched nothing lists none and points at `listing`, since a
    /// machine may run many.
    pub fn message(&self, instead: &str, listing: &str) -> String {
        match self {
            Self::NoneRunning { wanted } => {
                format!("{wanted} names no instance: none is running on this machine")
            }
            Self::NoMatch { wanted, .. } => format!(
                "{wanted} is not the name, tag, ID, label or application of a running instance; \
                 {listing} shows each running instance with the value that selects it"
            ),
            Self::Ambiguous {
                wanted,
                rule,
                candidates,
            } => {
                let width = candidates
                    .iter()
                    .take(MAX_LISTED)
                    .map(|candidate| candidate.value.len())
                    .max()
                    .unwrap_or(0);
                let mut text = format!(
                    "{wanted} is {} of {} running instances; {instead}:",
                    rule.what(),
                    candidates.len()
                );

                for candidate in candidates.iter().take(MAX_LISTED) {
                    text.push_str(&format!(
                        "\n  {:width$}  {}",
                        candidate.value, candidate.description
                    ));
                }

                if candidates.len() > MAX_LISTED {
                    text.push_str(&format!(
                        "\n  and {} more; {listing} shows them all",
                        candidates.len() - MAX_LISTED
                    ));
                }

                text
            }
        }
    }
}

/// Whether `record` matches `wanted` by `rule`.
fn matches(record: &Record, wanted: &str, rule: Rule) -> bool {
    match rule {
        Rule::Name => record.name.as_deref() == Some(wanted),
        Rule::Tag => record.tag() == wanted,
        Rule::Id => {
            wanted.len() >= MIN_PREFIX
                && wanted.bytes().all(|byte| byte.is_ascii_hexdigit())
                && record
                    .id
                    .to_string()
                    .starts_with(&wanted.to_ascii_lowercase())
        }
        Rule::Label => record.label.as_deref() == Some(wanted),
        Rule::App => record.app.as_deref() == Some(wanted),
    }
}

/// The rules, in the order they are tried.
const RULES: [Rule; 5] = [Rule::Name, Rule::Tag, Rule::Id, Rule::Label, Rule::App];

/// The first rule that matches any of `records`, with what it matched.
fn first_match<'a>(wanted: &str, records: &'a [Record]) -> Option<(Rule, Vec<&'a Record>)> {
    RULES.into_iter().find_map(|rule| {
        let found: Vec<&Record> = records
            .iter()
            .filter(|record| matches(record, wanted, rule))
            .collect();
        (!found.is_empty()).then_some((rule, found))
    })
}

/// The instance among `records` that `wanted` selects, by the first rule
/// that matches any of them.
pub fn select<'a>(wanted: &str, records: &'a [Record]) -> Result<Choice<'a>, Refusal> {
    let wanted = wanted.trim();

    if records.is_empty() {
        return Err(Refusal::NoneRunning {
            wanted: wanted.to_owned(),
        });
    }

    let offered = |found: &[&Record]| {
        found
            .iter()
            .map(|record| candidate(record, records))
            .collect::<Vec<_>>()
    };

    match first_match(wanted, records) {
        Some((rule, found)) if found.len() == 1 => Ok(Choice {
            record: found[0],
            rule,
        }),
        Some((rule, found)) => Err(Refusal::Ambiguous {
            wanted: wanted.to_owned(),
            rule,
            candidates: offered(&found),
        }),
        None => Err(Refusal::NoMatch {
            wanted: wanted.to_owned(),
            candidates: offered(&records.iter().collect::<Vec<_>>()),
        }),
    }
}

/// The value that selects `record` among `records`: its name, else its
/// tag, else its short ID, whichever is the first that selects it alone.
pub fn handle(record: &Record, records: &[Record]) -> String {
    let id = record.id.to_string();

    let tried = [
        record.name.clone(),
        Some(record.tag()),
        Some(record.id.short()),
    ];

    tried
        .into_iter()
        .flatten()
        .find(|value| {
            first_match(value, records)
                .is_some_and(|(_, found)| found.len() == 1 && found[0].id == record.id)
        })
        .unwrap_or(id)
}

/// `record` as a refusal offers it.
fn candidate(record: &Record, records: &[Record]) -> Candidate {
    Candidate {
        value: handle(record, records),
        description: record.describe(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::registry::{Endpoint, InstanceId, Kind, Listener};

    /// A live record of `kind`, with the ID given in hex and the rest set
    /// by the caller.
    fn record(id: &str, kind: Kind) -> Record {
        let mut record = Record::new(
            id.parse::<InstanceId>().expect("an ID"),
            kind,
            Endpoint::unix(format!("/tmp/{id}/admin.sock")),
            PathBuf::from(format!("/tmp/{id}")),
        );
        record.label = None;
        record
    }

    fn run(id: &str, port: u16, app: &str) -> Record {
        let mut record = record(id, Kind::Run);
        record.app = Some(app.to_owned());
        record.listeners = vec![Listener {
            name: "http".to_owned(),
            configured: format!("127.0.0.1:{port}"),
            bound: Some(format!("127.0.0.1:{port}")),
            tls: false,
        }];
        record
    }

    /// The machine the issue describes: an unnamed daemon, two runs, and
    /// a named daemon whose site is called `site`, like one run's.
    fn machine() -> Vec<Record> {
        let daemon = record("91720d04aaaaaaaaaaaaaaaaaaaaaaaa", Kind::Daemon);

        let mut helper = run("9ae9ffdbbbbbbbbbbbbbbbbbbbbbbbbb", 50907, "helper");
        helper.label = Some("admin-demo".to_owned());

        let hub = run("12345678cccccccccccccccccccccccc", 9080, "site");

        let mut dev = record("12349999dddddddddddddddddddddddd", Kind::Daemon);
        dev.name = Some("dev".to_owned());
        dev.label = Some("site".to_owned());

        vec![daemon, helper, hub, dev]
    }

    fn chosen(wanted: &str, records: &[Record]) -> (String, Rule) {
        let choice = select(wanted, records).expect("a choice");
        (choice.record.tag(), choice.rule)
    }

    #[test]
    fn each_rule_selects_in_turn() {
        let records = machine();

        assert_eq!(chosen("dev", &records), ("dev".to_owned(), Rule::Name));
        assert_eq!(
            chosen("run:50907", &records),
            ("run:50907".to_owned(), Rule::Tag)
        );
        assert_eq!(
            chosen("91720d04", &records),
            ("91720d04".to_owned(), Rule::Tag)
        );
        assert_eq!(chosen("9AE9", &records), ("run:50907".to_owned(), Rule::Id));
        assert_eq!(
            chosen("admin-demo", &records),
            ("run:50907".to_owned(), Rule::Label)
        );
        assert_eq!(
            chosen("helper", &records),
            ("run:50907".to_owned(), Rule::App)
        );

        // A label is tried before an application, so `site` is the named
        // daemon's label and not the run's application.

        assert_eq!(chosen("site", &records), ("dev".to_owned(), Rule::Label));
    }

    #[test]
    fn an_ambiguous_or_unmatched_value_lists_what_to_pass() {
        let records = machine();

        // Too short a prefix selects nothing; one shared by two IDs is
        // refused with both, each by a value that selects it alone.

        let Err(Refusal::NoMatch { candidates, .. }) = select("123", &records) else {
            panic!("123 is too short to be a prefix");
        };
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.value.as_str())
                .collect::<Vec<_>>(),
            ["91720d04", "run:50907", "run:9080", "dev"]
        );

        let refusal = select("1234", &records).expect_err("two IDs start 1234");
        let Refusal::Ambiguous {
            rule, candidates, ..
        } = &refusal
        else {
            panic!("ambiguous");
        };

        assert_eq!(*rule, Rule::Id);
        assert_eq!(candidates[0].value, "run:9080");
        assert_eq!(candidates[1].value, "dev");
        assert_eq!(
            refusal.message("pass one of these to --instance", "muructl instance list"),
            format!(
                "1234 is the start of the ID of 2 running instances; pass one of these to \
                 --instance:\n  run:9080  {}\n  dev       {}",
                candidates[0].description, candidates[1].description
            )
        );

        // Of many, five are listed and the rest counted.

        let many: Vec<Record> = (0..7)
            .map(|n| {
                let mut serve = record(
                    &format!("{n:x}000aaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                    Kind::Serve,
                );
                serve.app = Some("site".to_owned());
                serve
            })
            .collect();
        let message = select("site", &many)
            .expect_err("seven serve site")
            .message("pass one of these to --instance", "muructl instance list");
        assert_eq!(message.lines().count(), 7, "{message}");
        assert!(message.ends_with("\n  and 2 more; muructl instance list shows them all"));

        // Two serves share the tag `serve`, so each is offered by its ID.

        let serves = [
            record("aaaa0000aaaaaaaaaaaaaaaaaaaaaaaa", Kind::Serve),
            record("bbbb0000bbbbbbbbbbbbbbbbbbbbbbbb", Kind::Serve),
        ];
        let Err(Refusal::Ambiguous { candidates, .. }) = select("serve", &serves) else {
            panic!("two serves");
        };
        assert_eq!(candidates[0].value, "aaaa0000");
        assert_eq!(handle(&serves[1], &serves), "bbbb0000");

        assert!(matches!(
            select("dev", &[]),
            Err(Refusal::NoneRunning { .. })
        ));
    }
}
