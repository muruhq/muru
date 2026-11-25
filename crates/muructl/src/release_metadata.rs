//! The metadata a deploy records with its release (design specification
//! section 9.14, issue 866): `commit`, `ref` and `dirty` filled from the
//! git checkout the source directory is in, under what `--metadata`
//! flags say.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// How long the git commands of one fill may take together.
const GIT_LIMIT: Duration = Duration::from_secs(5);

/// What a fill from git found: the keys it filled, and why it filled
/// none when a git command failed or ran out of time. A directory in no
/// checkout, or a machine without git, fills nothing and says nothing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Filled {
    pub metadata: BTreeMap<String, String>,
    pub failure: Option<String>,
}

/// Fill `commit`, `ref` and `dirty` from the checkout `source` is in, as
/// the user who runs muructl, within `GIT_LIMIT`.
pub async fn from_git(source: &Path) -> Filled {
    match tokio::time::timeout(GIT_LIMIT, fill(source)).await {
        Ok(Ok(metadata)) => Filled {
            metadata,
            failure: None,
        },
        Ok(Err(failure)) => Filled {
            metadata: BTreeMap::new(),
            failure,
        },
        Err(_) => Filled {
            metadata: BTreeMap::new(),
            failure: Some(format!("git took longer than {} s", GIT_LIMIT.as_secs())),
        },
    }
}

/// The fill itself: `Err(None)` for a directory git does not track, or
/// no git at all, and `Err(Some(why))` for a git command that failed.
async fn fill(source: &Path) -> Result<BTreeMap<String, String>, Option<String>> {
    let inside = git(source, &["rev-parse", "--is-inside-work-tree"]).await;

    match inside {
        Ok(Output { ok: true, .. }) => {}
        Ok(Output {
            ok: false, stderr, ..
        }) if stderr.contains("not a git repository") => {
            return Err(None);
        }
        Ok(Output { stderr, .. }) => return Err(Some(failed("rev-parse", &stderr))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(None),
        Err(error) => return Err(Some(format!("running git: {error}"))),
    }

    let mut metadata = BTreeMap::new();

    // The commit: a checkout with no commit yet has none to give.

    let head = run(source, &["rev-parse", "HEAD"]).await?;
    metadata.insert("commit".to_owned(), head);

    // The branch, or with a detached HEAD the one tag pointing at it.

    let branch = git(source, &["symbolic-ref", "-q", "HEAD"])
        .await
        .map_err(|error| Some(format!("running git: {error}")))?;

    if branch.ok {
        metadata.insert("ref".to_owned(), branch.stdout);
    } else {
        let tags = run(source, &["tag", "--points-at", "HEAD"]).await?;
        let tags: Vec<&str> = tags.lines().filter(|line| !line.is_empty()).collect();

        if let [tag] = tags.as_slice() {
            metadata.insert("ref".to_owned(), format!("refs/tags/{tag}"));
        }
    }

    // Untracked files count, since they are deployed with the rest.

    if !run(source, &["status", "--porcelain"]).await?.is_empty() {
        metadata.insert("dirty".to_owned(), "true".to_owned());
    }

    Ok(metadata)
}

/// What one git command gave.
struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Run git in `source`.
async fn git(source: &Path, arguments: &[&str]) -> std::io::Result<Output> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(source)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;

    Ok(Output {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

/// Run git in `source` and give what it wrote, or why it failed.
async fn run(source: &Path, arguments: &[&str]) -> Result<String, Option<String>> {
    let output = git(source, arguments)
        .await
        .map_err(|error| Some(format!("running git: {error}")))?;

    if output.ok {
        Ok(output.stdout)
    } else {
        Err(Some(failed(arguments[0], &output.stderr)))
    }
}

/// A git command's failure in words.
fn failed(command: &str, stderr: &str) -> String {
    let first = stderr.lines().next().unwrap_or("it failed");
    format!("git {command}: {first}")
}

/// The metadata a deploy records: what the fill found, under the flags.
/// A flag sets its key, and `KEY=` with nothing after it leaves the key
/// out; a commit a flag gives leaves out the filled `dirty` as well,
/// which said the checkout had changes, not the commit given.
pub fn merged(
    filled: BTreeMap<String, String>,
    flags: &[(String, String)],
) -> BTreeMap<String, String> {
    let mut metadata = filled;

    if flags.iter().any(|(key, _)| key == "commit") {
        metadata.remove("dirty");
    }

    for (key, value) in flags {
        if value.is_empty() {
            metadata.remove(key);
        } else {
            metadata.insert(key.clone(), value.clone());
        }
    }

    metadata
}

/// Parse KEY=VALUE for --metadata; the value may be empty.
pub fn parse_flag(text: &str) -> Result<(String, String), String> {
    match text.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_owned(), value.to_owned())),
        _ => Err(format!("{text:?} is not KEY=VALUE")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_in(dir: &Path, arguments: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Muru", "-c", "user.email=muru@example.com"])
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(arguments)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?}");
    }

    #[tokio::test]
    async fn a_checkout_fills_its_commit_ref_and_whether_it_has_changes() {
        let dir = tempfile::tempdir().unwrap();

        // Not a checkout: nothing filled, and nothing to say.

        assert_eq!(from_git(dir.path()).await, Filled::default());

        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("app.py"), "").unwrap();
        git_in(dir.path(), &["add", "app.py"]);
        git_in(dir.path(), &["commit", "-q", "-m", "first"]);

        let filled = from_git(dir.path()).await;
        assert_eq!(filled.failure, None);
        assert_eq!(filled.metadata["commit"].len(), 40);
        assert_eq!(filled.metadata["ref"], "refs/heads/main");
        assert!(!filled.metadata.contains_key("dirty"));

        // An untracked file makes it dirty; a detached HEAD gives the one
        // tag on it.

        std::fs::write(dir.path().join("new.py"), "").unwrap();
        git_in(dir.path(), &["tag", "v1.0.0"]);
        git_in(dir.path(), &["checkout", "-q", "--detach"]);
        let filled = from_git(dir.path()).await;
        assert_eq!(filled.metadata["dirty"], "true");
        assert_eq!(filled.metadata["ref"], "refs/tags/v1.0.0");
    }

    #[test]
    fn flags_override_the_fill() {
        let filled: BTreeMap<String, String> =
            [("commit", "abc"), ("ref", "main"), ("dirty", "true")]
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect();
        let flag = |text: &str| parse_flag(text).unwrap();

        let metadata = merged(
            filled.clone(),
            &[flag("ref="), flag("description=Fix the total")],
        );
        assert!(!metadata.contains_key("ref"));
        assert_eq!(metadata["dirty"], "true");
        assert_eq!(metadata["description"], "Fix the total");

        // A commit given leaves out the filled dirty.

        let metadata = merged(filled, &[flag("commit=def")]);
        assert_eq!(metadata["commit"], "def");
        assert!(!metadata.contains_key("dirty"));

        assert!(parse_flag("=x").is_err());
        assert!(parse_flag("nothing").is_err());
    }
}
