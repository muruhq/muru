//! Named conditions over Muru's own signals (design specification section
//! 9.10, issue 123, ADR 0061): a small language shaped like PromQL, its
//! signals and the labels each takes, and the checks an applied file's
//! condition passes.
//!
//! ```text
//! condition  = term { ("and" | "or") term }       "and" binds tighter than "or"
//! term       = comparison | "(" condition ")"
//! comparison = signal [ "[" duration "]" ] op number
//! signal     = name [ "{" matcher { "," matcher } "}" ]
//! matcher    = label ("=" | "!=") quoted-string
//! op         = ">" | ">=" | "<" | "<="
//! ```
//!
//! Numbers are in base units: ratios from 0 to 1, seconds, bytes.

use std::fmt;
use std::time::Duration;

/// The longest window a term may read: the hour of per-second figures the
/// daemon keeps.
pub const MAX_WINDOW: Duration = Duration::from_secs(3600);

/// A term's window when it names none.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(60);

/// The requests a share, error rate or latency needs in its window to be
/// a figure rather than unknown, when the condition says nothing.
pub const DEFAULT_MIN_REQUESTS: u64 = 100;

/// What kind of figure a signal is, which says how a window reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Requests a second, from the request counter.
    Rate,
    /// A share of the requests in scope, 0 to 1.
    Share,
    /// A quantile of the request durations, seconds.
    Latency,
    /// A gauge: its latest value, or its average over a window.
    Gauge,
}

/// A signal a condition can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal {
    /// Its name in an expression.
    pub name: &'static str,
    /// How its figure is made.
    pub kind: Kind,
    /// The labels it takes.
    pub labels: &'static [&'static str],
    /// Whether its value is a ratio from 0 to 1, so a threshold outside
    /// that range is refused.
    pub ratio: bool,
}

/// Every signal, in the order the reference lists them.
pub const SIGNALS: &[Signal] = &[
    Signal {
        name: "request_rate",
        kind: Kind::Rate,
        labels: &[
            "app",
            "release",
            "group",
            "status",
            "reason",
            "client_class",
        ],
        ratio: false,
    },
    Signal {
        name: "request_share",
        kind: Kind::Share,
        labels: &[
            "app",
            "release",
            "group",
            "status",
            "reason",
            "client_class",
        ],
        ratio: true,
    },
    Signal {
        name: "error_rate",
        kind: Kind::Share,
        labels: &["app", "release", "group", "reason"],
        ratio: true,
    },
    Signal {
        name: "latency",
        kind: Kind::Latency,
        labels: &["app", "release", "group", "quantile", "phase"],
        ratio: false,
    },
    Signal {
        name: "queue_depth",
        kind: Kind::Gauge,
        labels: &["app", "release", "group"],
        ratio: false,
    },
    Signal {
        name: "capacity_utilisation",
        kind: Kind::Gauge,
        labels: &["app", "release", "group"],
        ratio: true,
    },
    Signal {
        name: "scaling_at_max",
        kind: Kind::Gauge,
        labels: &["app", "group"],
        ratio: true,
    },
    Signal {
        name: "scaling_denied",
        kind: Kind::Gauge,
        labels: &["app", "group"],
        ratio: true,
    },
    Signal {
        name: "node_memory_available",
        kind: Kind::Gauge,
        labels: &[],
        ratio: false,
    },
    Signal {
        name: "upstream_healthy",
        kind: Kind::Gauge,
        labels: &["upstream"],
        ratio: true,
    },
];

/// The labels that say which requests are counted, rather than picking
/// some of them out.
pub const SCOPE_LABELS: &[&str] = &["app", "release", "group"];

/// The signal by its name.
pub fn signal(name: &str) -> Option<&'static Signal> {
    SIGNALS.iter().find(|signal| signal.name == name)
}

/// A comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `>`
    Above,
    /// `>=`
    AtLeast,
    /// `<`
    Below,
    /// `<=`
    AtMost,
}

impl Op {
    /// Whether `value` compares so with `threshold`.
    pub fn holds(self, value: f64, threshold: f64) -> bool {
        match self {
            Self::Above => value > threshold,
            Self::AtLeast => value >= threshold,
            Self::Below => value < threshold,
            Self::AtMost => value <= threshold,
        }
    }

    /// Whether the comparison is true above its threshold.
    pub fn upward(self) -> bool {
        matches!(self, Self::Above | Self::AtLeast)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Above => ">",
            Self::AtLeast => ">=",
            Self::Below => "<",
            Self::AtMost => "<=",
        }
    }
}

/// A label matcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matcher {
    /// The label.
    pub label: String,
    /// `=` when true, `!=` when false.
    pub equal: bool,
    /// The value.
    pub value: String,
}

/// One comparison of a signal with a number.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// The signal.
    pub signal: &'static Signal,
    /// Its matchers, in the order written.
    pub matchers: Vec<Matcher>,
    /// The window given in brackets, if one was.
    pub window: Option<Duration>,
    /// The comparison.
    pub op: Op,
    /// The threshold, in base units.
    pub threshold: f64,
}

impl Comparison {
    /// The window the term reads: the one given, or the default for a
    /// figure made from counters; none for a gauge without one, which is
    /// its latest value.
    pub fn effective_window(&self) -> Option<Duration> {
        match (self.window, self.signal.kind) {
            (Some(window), _) => Some(window),
            (None, Kind::Gauge) => None,
            (None, _) => Some(DEFAULT_WINDOW),
        }
    }

    /// The value of a label's `=` matcher, if it has one.
    pub fn label(&self, label: &str) -> Option<&str> {
        self.matchers
            .iter()
            .find(|matcher| matcher.equal && matcher.label == label)
            .map(|matcher| matcher.value.as_str())
    }

    /// The signal, its matchers and window, as written: what two terms
    /// share when they read the same figure.
    pub fn selector(&self) -> String {
        let mut text = self.signal.name.to_owned();

        if !self.matchers.is_empty() {
            let matchers: Vec<String> = self
                .matchers
                .iter()
                .map(|matcher| {
                    format!(
                        "{}{}'{}'",
                        matcher.label,
                        if matcher.equal { "=" } else { "!=" },
                        matcher.value
                    )
                })
                .collect();
            text.push('{');
            text.push_str(&matchers.join(","));
            text.push('}');
        }

        if let Some(window) = self.window {
            text.push_str(&format!("[{}]", crate::format_duration(window)));
        }

        text
    }
}

impl fmt::Display for Comparison {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {} {}",
            self.selector(),
            self.op.as_str(),
            self.threshold
        )
    }
}

/// A condition's expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A comparison.
    Term(Comparison),
    /// Both.
    And(Box<Expr>, Box<Expr>),
    /// Either.
    Or(Box<Expr>, Box<Expr>),
}

impl Expr {
    /// Every comparison, in the order written.
    pub fn terms(&self) -> Vec<&Comparison> {
        match self {
            Self::Term(term) => vec![term],
            Self::And(left, right) | Self::Or(left, right) => {
                let mut terms = left.terms();
                terms.extend(right.terms());
                terms
            }
        }
    }
}

/// Read an expression, checking each signal, label, window and ratio
/// threshold. An error names the term and what is wrong with it.
pub fn parse(text: &str) -> Result<Expr, String> {
    let tokens = tokenize(text)?;
    let mut parser = Parser { tokens, at: 0 };
    let expr = parser.or()?;

    match parser.tokens.get(parser.at) {
        None => Ok(expr),
        Some(token) => Err(format!("unexpected {token} after a whole condition")),
    }
}

/// A token of the language.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Number(f64),
    Quoted(String),
    Window(String),
    Op(Op),
    Equal,
    NotEqual,
    Open,
    Close,
    OpenBrace,
    CloseBrace,
    Comma,
}

impl fmt::Display for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(name) => write!(formatter, "{name:?}"),
            Self::Number(number) => write!(formatter, "{number}"),
            Self::Quoted(text) => write!(formatter, "'{text}'"),
            Self::Window(text) => write!(formatter, "[{text}]"),
            Self::Op(op) => formatter.write_str(op.as_str()),
            Self::Equal => formatter.write_str("="),
            Self::NotEqual => formatter.write_str("!="),
            Self::Open => formatter.write_str("("),
            Self::Close => formatter.write_str(")"),
            Self::OpenBrace => formatter.write_str("{"),
            Self::CloseBrace => formatter.write_str("}"),
            Self::Comma => formatter.write_str(","),
        }
    }
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut at = 0;

    while at < chars.len() {
        let c = chars[at];

        match c {
            _ if c.is_whitespace() => at += 1,
            '(' | ')' | '{' | '}' | ',' => {
                tokens.push(match c {
                    '(' => Token::Open,
                    ')' => Token::Close,
                    '{' => Token::OpenBrace,
                    '}' => Token::CloseBrace,
                    _ => Token::Comma,
                });
                at += 1;
            }
            '>' | '<' | '=' | '!' => {
                let next = chars.get(at + 1).copied();

                let (token, width) = match (c, next) {
                    ('>', Some('=')) => (Token::Op(Op::AtLeast), 2),
                    ('>', _) => (Token::Op(Op::Above), 1),
                    ('<', Some('=')) => (Token::Op(Op::AtMost), 2),
                    ('<', _) => (Token::Op(Op::Below), 1),
                    ('!', Some('=')) => (Token::NotEqual, 2),
                    ('=', Some('=')) => {
                        return Err("== is not a comparison; a matcher uses =".to_owned());
                    }
                    ('=', _) => (Token::Equal, 1),
                    _ => return Err("! is followed by = in a matcher".to_owned()),
                };

                tokens.push(token);
                at += width;
            }
            '\'' | '"' => {
                let end = chars[at + 1..]
                    .iter()
                    .position(|other| *other == c)
                    .ok_or_else(|| "a quoted label value is not closed".to_owned())?;
                tokens.push(Token::Quoted(chars[at + 1..at + 1 + end].iter().collect()));
                at += end + 2;
            }
            '[' => {
                let end = chars[at + 1..]
                    .iter()
                    .position(|other| *other == ']')
                    .ok_or_else(|| "a window's [ is not closed".to_owned())?;
                tokens.push(Token::Window(chars[at + 1..at + 1 + end].iter().collect()));
                at += end + 2;
            }
            _ if c.is_ascii_digit() || c == '.' => {
                let start = at;

                while at < chars.len()
                    && (chars[at].is_ascii_alphanumeric() || matches!(chars[at], '.' | '_'))
                {
                    at += 1;
                }

                let word: String = chars[start..at].iter().collect();
                let number = word.parse::<f64>().map_err(|_| {
                    if word.ends_with('%') || word.chars().any(char::is_alphabetic) {
                        format!(
                            "{word} is not a number in base units: a ratio is 0.2, not 20%, and a time 0.5 seconds, not 500ms"
                        )
                    } else {
                        format!("{word} is not a number")
                    }
                })?;
                tokens.push(Token::Number(number));
            }
            _ if c.is_ascii_alphabetic() || c == '_' => {
                let start = at;

                while at < chars.len() && (chars[at].is_ascii_alphanumeric() || chars[at] == '_') {
                    at += 1;
                }

                tokens.push(Token::Name(chars[start..at].iter().collect()));
            }
            '%' => {
                return Err(
                    "a ratio is a number from 0 to 1, such as 0.2, not a percentage".to_owned(),
                );
            }
            _ => return Err(format!("{c:?} has no meaning in a condition")),
        }
    }

    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Name(name)) if name == word)
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut expr = self.and()?;

        while self.keyword("or") {
            self.at += 1;
            expr = Expr::Or(Box::new(expr), Box::new(self.and()?));
        }

        Ok(expr)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut expr = self.term()?;

        while self.keyword("and") {
            self.at += 1;
            expr = Expr::And(Box::new(expr), Box::new(self.term()?));
        }

        Ok(expr)
    }

    fn term(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Token::Open) => {
                let expr = self.or()?;

                match self.next() {
                    Some(Token::Close) => Ok(expr),
                    _ => Err("a ( is not closed".to_owned()),
                }
            }
            Some(Token::Name(name)) => self.comparison(&name).map(Expr::Term),
            Some(token) => Err(format!("expected a signal or (, not {token}")),
            None => Err("the condition ends where a term was expected".to_owned()),
        }
    }

    fn comparison(&mut self, name: &str) -> Result<Comparison, String> {
        let signal = signal(name).ok_or_else(|| {
            let names: Vec<&str> = SIGNALS.iter().map(|signal| signal.name).collect();
            format!(
                "{name} is not a signal; the signals are {}",
                names.join(", ")
            )
        })?;
        let mut matchers = Vec::new();

        if self.peek() == Some(&Token::OpenBrace) {
            self.at += 1;

            loop {
                let label = match self.next() {
                    Some(Token::Name(label)) => label,
                    Some(Token::CloseBrace) if matchers.is_empty() => break,
                    _ => return Err(format!("{name}: expected a label in {{}}")),
                };

                if !signal.labels.contains(&label.as_str()) {
                    return Err(format!(
                        "{name} takes no label {label}; it takes {}",
                        if signal.labels.is_empty() {
                            "none".to_owned()
                        } else {
                            signal.labels.join(", ")
                        }
                    ));
                }

                let equal = match self.next() {
                    Some(Token::Equal) => true,
                    Some(Token::NotEqual) => false,
                    _ => return Err(format!("{name}: {label} is followed by = or !=")),
                };

                let Some(Token::Quoted(value)) = self.next() else {
                    return Err(format!(
                        "{name}: {label}'s value is quoted, as {label}='shop'"
                    ));
                };

                if label == "quantile" {
                    let quantile = value.parse::<f64>().ok().filter(|q| *q > 0.0 && *q < 1.0);

                    if quantile.is_none() || !equal {
                        return Err(format!(
                            "{name}: quantile is = a number between 0 and 1, such as '0.99'"
                        ));
                    }
                }

                if label == "phase" && !["total", "queue", "application"].contains(&value.as_str())
                {
                    return Err(format!(
                        "{name}: phase is 'total', 'queue' or 'application', not '{value}'"
                    ));
                }

                matchers.push(Matcher {
                    label,
                    equal,
                    value,
                });

                match self.next() {
                    Some(Token::Comma) => {}
                    Some(Token::CloseBrace) => break,
                    _ => return Err(format!("{name}: a matcher is followed by , or }}")),
                }
            }
        }

        let window = match self.peek() {
            Some(Token::Window(text)) => {
                let text = text.clone();
                self.at += 1;
                let window = crate::parse_duration(&text)
                    .map_err(|error| format!("{name}[{text}]: {error}"))?;

                if window.is_zero() {
                    return Err(format!("{name}[{text}]: a window is longer than nothing"));
                }

                if window > MAX_WINDOW {
                    return Err(format!(
                        "{name}[{text}]: a window is at most 1h, the history the daemon keeps"
                    ));
                }

                Some(window)
            }
            _ => None,
        };

        let Some(Token::Op(op)) = self.next() else {
            return Err(format!("{name} is followed by >, >=, < or <= and a number"));
        };

        let threshold = match self.next() {
            Some(Token::Number(number)) => number,
            _ => return Err(format!("{name} {} is followed by a number", op.as_str())),
        };

        if signal.ratio && !(0.0..=1.0).contains(&threshold) {
            return Err(format!(
                "{name} is a ratio from 0 to 1, so {threshold} is no threshold for it"
            ));
        }

        Ok(Comparison {
            signal,
            matchers,
            window,
            op,
            threshold,
        })
    }
}

/// A condition's table, checked: its expressions read, its durations
/// parse, and a warning when `when` and `clear_when` are one comparison
/// each of the same figure with no gap between their thresholds, which
/// is no hysteresis. Gives the warnings.
pub fn check(name: &str, condition: &crate::Condition) -> Result<Vec<String>, String> {
    let what = format!("condition {name}");

    if !crate::valid_name(name) {
        return Err(format!(
            "{what}: a condition's name is lowercase letters, digits and hyphens"
        ));
    }

    let when = parse(&condition.when).map_err(|error| format!("{what}: when: {error}"))?;
    let clear = condition
        .clear_when
        .as_deref()
        .map(parse)
        .transpose()
        .map_err(|error| format!("{what}: clear_when: {error}"))?;

    for (key, value) in [
        ("for", &condition.for_),
        ("clear_for", &condition.clear_for),
    ] {
        if let Some(text) = value {
            crate::parse_duration(text).map_err(|error| format!("{what}: {key}: {error}"))?;
        }
    }

    let mut warnings = Vec::new();

    if let (Expr::Term(fire), Some(Expr::Term(resolve))) = (&when, &clear)
        && fire.selector() == resolve.selector()
    {
        let no_gap = match (fire.op.upward(), resolve.op.upward()) {
            (true, false) => resolve.threshold >= fire.threshold,
            (false, true) => resolve.threshold <= fire.threshold,
            _ => false,
        };

        if no_gap {
            warnings.push(format!(
                "{what}: clear_when reads {} with no gap below when's threshold, so the condition has no hysteresis and may flap",
                fire.selector()
            ));
        }
    }

    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_condition_reads_as_written_with_and_binding_tighter_than_or() {
        let expr = parse(
            "error_rate{app='shop',reason='app_error'} > 0.2 or request_rate{app=\"shop\"}[5m] >= 10 and queue_depth < 3",
        )
        .unwrap();

        let Expr::Or(left, right) = &expr else {
            panic!("{expr:?}");
        };
        assert!(matches!(**left, Expr::Term(_)));
        assert!(matches!(**right, Expr::And(_, _)));

        let terms = expr.terms();
        assert_eq!(
            terms[0].to_string(),
            "error_rate{app='shop',reason='app_error'} > 0.2"
        );
        assert_eq!(terms[1].window, Some(Duration::from_secs(300)));
        assert_eq!(terms[1].effective_window(), Some(Duration::from_secs(300)));
        assert_eq!(terms[2].effective_window(), None);
        assert_eq!(terms[0].effective_window(), Some(DEFAULT_WINDOW));
        assert_eq!(terms[0].label("app"), Some("shop"));

        let grouped =
            parse("(queue_depth > 1 or queue_depth > 2) and node_memory_available < 1e9").unwrap();
        assert!(matches!(grouped, Expr::And(_, _)));
    }

    #[test]
    fn what_is_not_the_language_is_refused_saying_why() {
        let refused = |text: &str| parse(text).unwrap_err();

        assert!(refused("errors > 1").contains("errors is not a signal"));
        assert!(
            refused("error_rate{status='5xx'} > 0.1").contains("error_rate takes no label status")
        );
        assert!(refused("error_rate > 20%").contains("not a percentage"));
        assert!(refused("latency > 500ms").contains("not a number in base units"));
        assert!(refused("error_rate > 1.5").contains("is a ratio from 0 to 1"));
        assert!(refused("request_rate[2h] > 1").contains("at most 1h"));
        assert!(refused("request_rate{app=shop} > 1").contains("quoted"));
        assert!(refused("latency{quantile='99'} > 0.5").contains("between 0 and 1"));
        assert!(refused("latency{phase='wait'} > 0.5").contains("phase is"));
        assert!(refused("(queue_depth > 1").contains("not closed"));
        assert!(refused("queue_depth > 1 queue_depth").contains("after a whole condition"));
        assert!(refused("queue_depth").contains("followed by >"));
        assert!(refused("node_memory_available{app='shop'} < 1").contains("it takes none"));
    }

    #[test]
    fn a_clear_threshold_with_no_gap_is_warned_about() {
        let condition = |when: &str, clear: &str| crate::Condition {
            when: when.to_owned(),
            clear_when: Some(clear.to_owned()),
            ..crate::Condition::default()
        };

        let warned = check(
            "shop-errors",
            &condition(
                "error_rate{app='shop'} > 0.2",
                "error_rate{app='shop'} < 0.2",
            ),
        )
        .unwrap();
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains("no hysteresis"));

        assert!(
            check(
                "shop-errors",
                &condition(
                    "error_rate{app='shop'} > 0.2",
                    "error_rate{app='shop'} < 0.02"
                )
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            check(
                "shop-errors",
                &condition(
                    "error_rate{app='shop'} > 0.2",
                    "error_rate{app='blog'} < 0.3"
                )
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            check("Shop", &condition("queue_depth > 1", "queue_depth < 1"))
                .unwrap_err()
                .contains("lowercase")
        );
        assert!(
            check(
                "q",
                &crate::Condition {
                    when: "queue_depth > 1".to_owned(),
                    for_: Some("soon".to_owned()),
                    ..crate::Condition::default()
                }
            )
            .unwrap_err()
            .contains("for:")
        );
    }
}
