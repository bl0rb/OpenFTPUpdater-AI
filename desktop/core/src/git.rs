//! The git operations of the steps, through the `git` command line (uses the user's credentials and hooks).

use crate::shell::output;
use anyhow::Result;
use std::path::Path;

/// Changed and untracked files (`git status --porcelain`), empty when the tree is clean.
pub fn changes(dir: &Path) -> Result<Vec<String>> {
    Ok(output("git", &["status", "--porcelain"], dir)?.lines().map(str::to_owned).collect())
}

pub fn branch(dir: &Path) -> Result<String> {
    output("git", &["rev-parse", "--abbrev-ref", "HEAD"], dir)
}

pub fn head(dir: &Path) -> Result<String> {
    output("git", &["rev-parse", "HEAD"], dir)
}

pub fn has_upstream(dir: &Path) -> bool {
    output("git", &["rev-parse", "--abbrev-ref", "@{u}"], dir).is_ok()
}

pub fn fetch(dir: &Path) -> Result<()> {
    output("git", &["fetch", "--quiet"], dir).map(|_| ())
}

/// Commits ahead of and behind the upstream branch.
pub fn ahead_behind(dir: &Path) -> Result<(u32, u32)> {
    let counts = output("git", &["rev-list", "--left-right", "--count", "HEAD...@{u}"], dir)?;
    let mut parts = counts.split_whitespace().map(|n| n.parse().unwrap_or(0));
    Ok((parts.next().unwrap_or(0), parts.next().unwrap_or(0)))
}

/// Number of commits from `since` to HEAD; `None` if `since` is unknown here.
pub fn commits_since(dir: &Path, since: &str) -> Option<u32> {
    output("git", &["rev-list", "--count", &format!("{since}..HEAD")], dir).ok()?.parse().ok()
}

pub fn pull_ff(dir: &Path) -> Result<String> {
    output("git", &["pull", "--ff-only"], dir)
}

pub fn diff_stat(dir: &Path) -> Result<String> {
    output("git", &["diff", "HEAD", "--stat"], dir)
}

/// Stages everything and commits; returns the short hash.
pub fn commit_all(dir: &Path, subject: &str, body: &str) -> Result<String> {
    output("git", &["add", "--all"], dir)?;
    output("git", &["commit", "--quiet", "-m", subject, "-m", body], dir)?;
    output("git", &["rev-parse", "--short", "HEAD"], dir)
}

pub fn push(dir: &Path) -> Result<String> {
    output("git", &["push", "--quiet", "origin", "HEAD"], dir)
}
