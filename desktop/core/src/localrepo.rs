//! Finding the local clone of a GitHub repository next to the project, so Claude can read the
//! released state through `git -C <path>`.

use crate::shell;
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// `owner/name` from a GitHub remote URL (`https://github.com/o/n(.git)`, `git@github.com:o/n.git`).
pub fn github_repo_from_url(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !name.is_empty() && parts.next().is_none()).then(|| format!("{owner}/{name}"))
}

/// `owner/name` of the `origin` remote, read from `.git/config` without starting git.
pub fn origin_repo(dir: &Path) -> Option<String> {
    let config = fs::read_to_string(dir.join(".git").join("config")).ok()?;
    let mut in_origin = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_origin = line == "[remote \"origin\"]";
        } else if in_origin {
            if let Some(url) = line.strip_prefix("url").map(str::trim_start).and_then(|l| l.strip_prefix('=')) {
                return github_repo_from_url(url);
            }
        }
    }
    None
}

/// All GitHub clones directly inside `dir`, keyed by lowercase `owner/name`.
pub fn clones_in(dir: &Path) -> HashMap<String, PathBuf> {
    let mut clones = HashMap::new();
    let Ok(entries) = fs::read_dir(dir) else { return clones };
    let mut dirs: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        if let Some(repo) = origin_repo(&dir) {
            clones.entry(repo.to_lowercase()).or_insert(dir);
        }
    }
    clones
}

/// `git clone https://github.com/<repo>.git <workspace>/<name>`; uses the user's git credentials.
pub fn clone(repo: &str, workspace: &Path) -> Result<PathBuf> {
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let target = workspace.join(name);
    if target.exists() {
        bail!("{} existiert bereits", target.display());
    }
    fs::create_dir_all(workspace)?;
    let url = format!("https://github.com/{repo}.git");
    shell::output("git", &["clone", "--quiet", &url, &target.display().to_string()], workspace)?;
    Ok(target)
}

/// How site.json refers to a clone: `../name` next to the project, otherwise the absolute path.
pub fn relative_to_project(project: &Path, clone: &Path) -> String {
    match (project.parent(), clone.parent(), clone.file_name()) {
        (Some(a), Some(b), Some(name)) if a == b => format!("../{}", name.to_string_lossy()),
        _ => clone.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_urls() {
        assert_eq!(github_repo_from_url("https://github.com/bl0rb/nudgit.git").as_deref(), Some("bl0rb/nudgit"));
        assert_eq!(github_repo_from_url("git@github.com:bl0rb/ClaudeKeyZapper.git").as_deref(), Some("bl0rb/ClaudeKeyZapper"));
        assert_eq!(github_repo_from_url("https://github.com/bl0rb/Weave/").as_deref(), Some("bl0rb/Weave"));
        assert_eq!(github_repo_from_url("https://gitlab.com/a/b.git"), None);
    }

    #[test]
    fn makes_sibling_paths_relative() {
        let project = Path::new("/code/site");
        assert_eq!(relative_to_project(project, Path::new("/code/app")), "../app");
        assert_eq!(relative_to_project(project, Path::new("/other/app")), "/other/app");
    }
}
