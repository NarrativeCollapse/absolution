//! Queries against git checkouts, answered by `git` itself rather than by
//! parsing files under `.git`.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::cook::{fs::run_command, pty::PtyOut};
use crate::{Error, Result, wrap_io_err};

/// Where `HEAD` of a checkout points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// The full commit hash.
    pub rev: String,
    /// The checked out branch, or `None` when `HEAD` is detached.
    pub branch: Option<String>,
}

/// The upstream a local branch tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracking {
    /// Remote name, usually "origin".
    pub remote: String,
    /// URL of that remote, without a trailing ".git".
    pub url: String,
    /// Branch name on the remote.
    pub branch: String,
}

/// Whether `dir` is the root of a checkout (and not merely inside one).
///
/// Every other function here must only be called on such a directory:
/// otherwise git silently answers for the enclosing repository.
pub fn is_checkout(dir: &Path) -> bool {
    dir.join(".git").exists()
}

fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir);
    command
}

/// Run a git query and return its trimmed stdout, or `None` if git exited
/// unsuccessfully (which is how git reports "no such ref/config key").
fn query(dir: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = git(dir)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(wrap_io_err!(dir, "Running git"))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

fn require(dir: &Path, args: &[&str], what: &str) -> Result<String> {
    query(dir, args)?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Other(format!("Could not find {what} in {:?}", dir.display())))
}

/// The commit and branch `HEAD` points to.
pub fn head(dir: &Path) -> Result<Head> {
    let rev = require(dir, &["rev-parse", "--verify", "HEAD^{commit}"], "HEAD")?;
    let branch = query(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])?
        .filter(|branch| !branch.is_empty());
    Ok(Head { rev, branch })
}

/// Resolve a tag, branch or (possibly abbreviated) hash to a full commit hash.
pub fn resolve_commit(dir: &Path, rev: &str) -> Result<String> {
    require(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
        &format!("revision {rev:?}"),
    )
}

/// The default branch of `remote`, as recorded at clone time.
pub fn remote_default_branch(dir: &Path, remote: &str) -> Result<String> {
    let full = require(
        dir,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
        &format!("the default branch of remote {remote:?}"),
    )?;
    Ok(full
        .strip_prefix(&format!("{remote}/"))
        .unwrap_or(&full)
        .to_string())
}

/// The commit of `branch` on `remote` as of the last fetch.
pub fn remote_branch_rev(dir: &Path, remote: &str, branch: &str) -> Result<String> {
    resolve_commit(dir, &format!("refs/remotes/{remote}/{branch}"))
}

/// The upstream tracked by local `branch`, if it tracks one.
pub fn tracking(dir: &Path, branch: &str) -> Result<Option<Tracking>> {
    let config = |key: String| query(dir, &["config", "--get", &key]);
    let Some(remote) = config(format!("branch.{branch}.remote"))? else {
        return Ok(None);
    };
    let Some(merge) = config(format!("branch.{branch}.merge"))? else {
        return Ok(None);
    };
    let Some(url) = config(format!("remote.{remote}.url"))? else {
        return Ok(None);
    };
    Ok(Some(Tracking {
        remote,
        url: normalize_url(&url).to_string(),
        branch: merge
            .strip_prefix("refs/heads/")
            .unwrap_or(&merge)
            .to_string(),
    }))
}

/// Strip the optional ".git" suffix so equivalent URLs compare equal.
pub fn normalize_url(url: &str) -> &str {
    url.strip_suffix(".git").unwrap_or(url)
}

/// Point `origin` at `url` and fetch it.
pub fn fetch_origin(dir: &Path, url: &str, logger: &PtyOut) -> Result<()> {
    let mut command = git(dir);
    command.args(["remote", "set-url", "origin", url]);
    run_command(command, logger)?;

    let mut command = git(dir);
    command.args(["fetch", "origin"]);
    run_command(command, logger)
}

/// The date of the `HEAD` commit, in UTC and strict ISO 8601.
pub fn commit_date(dir: &Path) -> Result<String> {
    let output = git(dir)
        .args(["log", "-1", "--date=iso-strict-local", "--format=%ad"])
        .env("TZ", "UTC")
        .stderr(Stdio::null())
        .output()
        .map_err(wrap_io_err!(dir, "Running git log"))?;
    let date = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || date.is_empty() {
        return Err(Error::Other(format!(
            "Could not read the commit date of {:?}",
            dir.display()
        )));
    }
    Ok(date)
}

/// The newest commit reachable from `HEAD` that is not newer than `date`.
pub fn rev_before_date(dir: &Path, date: &str) -> Result<String> {
    require(
        dir,
        &["rev-list", "-n", "1", &format!("--before={date}"), "HEAD"],
        &format!("a commit before {date}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run(dir: &Path, args: &[&str]) {
        let status = git(dir)
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// An upstream repository with two commits and tags, and a clone of it.
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        fs::create_dir(&upstream).unwrap();
        run(&upstream, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(upstream.join("a"), "1").unwrap();
        run(&upstream, &["add", "a"]);
        run(&upstream, &["commit", "--quiet", "-m", "one"]);
        run(&upstream, &["tag", "v1.0"]);
        fs::write(upstream.join("a"), "2").unwrap();
        run(&upstream, &["commit", "--quiet", "-am", "two"]);
        run(&upstream, &["tag", "-a", "v1.0.1", "-m", "annotated"]);
        run(&upstream, &["branch", "feature/with-slash"]);

        let clone = tmp.path().join("clone");
        let status = Command::new("git")
            .args(["clone", "--quiet"])
            .arg(&upstream)
            .arg(&clone)
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        (tmp, upstream, clone)
    }

    #[test]
    fn head_reports_branch_and_detached() {
        let (_tmp, _upstream, clone) = fixture();
        let on_branch = head(&clone).unwrap();
        assert_eq!(on_branch.branch.as_deref(), Some("main"));
        assert_eq!(on_branch.rev.len(), 40);

        run(&clone, &["checkout", "--quiet", "v1.0"]);
        let detached = head(&clone).unwrap();
        assert_eq!(detached.branch, None);
        assert_ne!(detached.rev, on_branch.rev);
    }

    #[test]
    fn tags_resolve_exactly_even_with_a_common_prefix() {
        let (_tmp, _upstream, clone) = fixture();
        let v1 = resolve_commit(&clone, "v1.0").unwrap();
        let v101 = resolve_commit(&clone, "v1.0.1").unwrap();
        assert_ne!(v1, v101);
        // An annotated tag resolves to its commit, not to the tag object.
        assert_eq!(v101, head(&clone).unwrap().rev);
        // Abbreviated hashes resolve to the full hash.
        assert_eq!(resolve_commit(&clone, &v1[..8]).unwrap(), v1);
        assert!(resolve_commit(&clone, "no-such-tag").is_err());
    }

    #[test]
    fn tracking_handles_branches_with_slashes() {
        let (_tmp, upstream, clone) = fixture();
        assert_eq!(remote_default_branch(&clone, "origin").unwrap(), "main");

        let main = tracking(&clone, "main").unwrap().unwrap();
        assert_eq!(main.remote, "origin");
        assert_eq!(main.branch, "main");
        assert_eq!(main.url, upstream.to_str().unwrap());

        run(&clone, &["checkout", "--quiet", "feature/with-slash"]);
        let branch = head(&clone).unwrap().branch.unwrap();
        assert_eq!(branch, "feature/with-slash");
        let feature = tracking(&clone, &branch).unwrap().unwrap();
        assert_eq!(feature.branch, "feature/with-slash");
        assert_eq!(
            remote_branch_rev(&clone, "origin", "feature/with-slash").unwrap(),
            head(&clone).unwrap().rev
        );

        assert_eq!(tracking(&clone, "untracked").unwrap(), None);
    }

    #[test]
    fn fetch_sees_new_upstream_commits() {
        let (_tmp, upstream, clone) = fixture();
        let before = remote_branch_rev(&clone, "origin", "main").unwrap();
        fs::write(upstream.join("a"), "3").unwrap();
        run(&upstream, &["commit", "--quiet", "-am", "three"]);

        fetch_origin(&clone, upstream.to_str().unwrap(), &None).unwrap();
        let after = remote_branch_rev(&clone, "origin", "main").unwrap();
        assert_ne!(before, after);
        assert_eq!(after, head(&upstream).unwrap().rev);
    }

    #[test]
    fn dates() {
        let (_tmp, _upstream, clone) = fixture();
        let date = commit_date(&clone).unwrap();
        assert!(date.ends_with("+00:00") || date.ends_with('Z'), "{date}");
        assert!(rev_before_date(&clone, "1971-01-01T00:00:00Z").is_err());
        assert_eq!(
            rev_before_date(&clone, "2037-01-01T00:00:00Z").unwrap(),
            head(&clone).unwrap().rev
        );
    }

    #[test]
    fn url_normalization() {
        assert_eq!(normalize_url("https://x/y.git"), "https://x/y");
        assert_eq!(normalize_url("https://x/y"), "https://x/y");
        assert!(!is_checkout(Path::new("/nonexistent")));
    }
}
