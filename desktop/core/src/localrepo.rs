//! Finding the local clone of a repository (GitHub or a GitLab instance) next to the project, so
//! Claude can read the released state through `git -C <path>`.

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

/// Hosts of other services: their clones are not GitLab projects and stay out.
const OTHER_HOSTS: [&str; 6] = ["bitbucket.org", "codeberg.org", "dev.azure.com", "ssh.dev.azure.com", "vs-ssh.visualstudio.com", "git.sr.ht"];

/// The repository an origin URL points to: `owner/name` on GitHub, `host/group/…/name` on any other
/// host, taken as a GitLab instance. An SSH port is dropped (the API is reached over HTTPS); an
/// HTTPS port is kept.
pub fn repo_from_url(url: &str) -> Option<String> {
    if let Some(repo) = github_repo_from_url(url) {
        return Some(repo);
    }
    let url = url.trim();
    let (host, path) = if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        let (authority, path) = rest.split_once('/')?;
        (authority.rsplit('@').next()?.to_owned(), path)
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        (host.split(':').next()?.to_owned(), path)
    } else {
        // scp-like: git@host:group/name.git
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        (authority.rsplit('@').next()?.to_owned(), path)
    };
    let host = host.to_lowercase();
    if host == "github.com" || OTHER_HOSTS.contains(&host.as_str()) {
        return None;
    }
    let path = path.trim_matches('/').trim_end_matches(".git");
    let id = format!("{host}/{path}");
    crate::gitlab::project(&id).map(|p| format!("{}/{}", p.host, p.path))
}

/// The repository of the `origin` remote (see `repo_from_url`), read from `.git/config` without starting git.
pub fn origin_repo(dir: &Path) -> Option<String> {
    let config = fs::read_to_string(dir.join(".git").join("config")).ok()?;
    let mut in_origin = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_origin = line == "[remote \"origin\"]";
        } else if in_origin {
            if let Some(url) = line.strip_prefix("url").map(str::trim_start).and_then(|l| l.strip_prefix('=')) {
                return repo_from_url(url);
            }
        }
    }
    None
}

/// A repository with clones in a folder.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalClone {
    /// `owner/name` (GitHub) or `host/group/…/name` (GitLab) as the origin names it.
    pub repo: String,
    /// The clone to use: the folder named like the repository, else the first by name.
    pub path: PathBuf,
    /// Further clones of the same repository.
    pub others: Vec<PathBuf>,
}

/// The repositories with clones directly inside `dir`, each once, sorted by name. Folders without
/// a recognised origin and clones of a wiki (`…/name.wiki`) are left out.
pub fn clones(dir: &Path) -> Vec<LocalClone> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|p| origin_repo(&p).map(|repo| (repo, p)))
        .filter(|(repo, _)| !repo.to_lowercase().ends_with(".wiki"))
        .collect();
    found.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then(a.1.cmp(&b.1)));
    let mut clones: Vec<LocalClone> = Vec::new();
    for (repo, path) in found {
        match clones.last_mut().filter(|c| c.repo.eq_ignore_ascii_case(&repo)) {
            Some(clone) => clone.others.push(path),
            None => clones.push(LocalClone { repo, path, others: Vec::new() }),
        }
    }
    for clone in &mut clones {
        let name = clone.repo.rsplit('/').next().unwrap_or_default().to_lowercase();
        let named = |p: &PathBuf| p.file_name().is_some_and(|f| f.to_string_lossy().to_lowercase() == name);
        if !named(&clone.path) {
            if let Some(i) = clone.others.iter().position(named) {
                std::mem::swap(&mut clone.path, &mut clone.others[i]);
                clone.others.sort();
            }
        }
    }
    clones
}

/// All clones directly inside `dir`, keyed by the lowercase repository (see `repo_from_url`).
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

/// `git clone <https URL> <workspace>/<name>`; uses the user's git credentials.
pub fn clone(repo: &str, workspace: &Path) -> Result<PathBuf> {
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let target = workspace.join(name);
    if target.exists() {
        bail!("{} existiert bereits", target.display());
    }
    fs::create_dir_all(workspace)?;
    let url = crate::forge::clone_url(repo);
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
    fn other_hosts_are_gitlab_projects() {
        assert_eq!(repo_from_url("https://gitpro.example.de/WERK/rag_optimizer.git").as_deref(), Some("gitpro.example.de/WERK/rag_optimizer"));
        assert_eq!(repo_from_url("git@gitlab.example.com:werk/tools/app.git").as_deref(), Some("gitlab.example.com/werk/tools/app"));
        assert_eq!(repo_from_url("ssh://git@gitlab.example.com:2222/werk/app.git").as_deref(), Some("gitlab.example.com/werk/app"), "the SSH port is not the API's");
        assert_eq!(repo_from_url("https://user@git.example.org:8443/g/app").as_deref(), Some("git.example.org:8443/g/app"));
        assert_eq!(repo_from_url("https://github.com/bl0rb/Weave.git").as_deref(), Some("bl0rb/Weave"));
        assert_eq!(repo_from_url("https://bitbucket.org/o/app.git"), None);
        assert_eq!(repo_from_url("https://gitlab.example.com/app.git"), None, "a project needs a namespace");
        assert_eq!(repo_from_url("/local/path/repo"), None);
    }

    #[test]
    fn lists_each_repository_of_a_folder_once() {
        let dir = std::env::temp_dir().join(format!("orp-local-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (name, url) in [
            ("zeta", Some("git@github.com:o/Zeta.git")),
            ("alpha", Some("https://github.com/o/alpha")),
            ("garden-copy", Some("https://github.com/o/Garden.git")),
            ("Garden", Some("https://github.com/o/Garden.git")),
            ("app-wiki", Some("https://github.com/o/app.wiki.git")),
            ("other", Some("https://gitlab.com/o/x.git")),
            ("plain", None),
        ] {
            let git = dir.join(name).join(".git");
            fs::create_dir_all(&git).unwrap();
            let config = url.map(|u| format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {u}\n")).unwrap_or_default();
            fs::write(git.join("config"), config).unwrap();
        }
        fs::write(dir.join("file.txt"), "").unwrap();
        let found = clones(&dir);
        let repos: Vec<&str> = found.iter().map(|c| c.repo.as_str()).collect();
        assert_eq!(repos, ["gitlab.com/o/x", "o/alpha", "o/Garden", "o/Zeta"], "GitHub and GitLab origins, no wiki, each once, sorted, case kept");
        assert!(found[2].path.ends_with("Garden"), "the folder named like the repository: {:?}", found[2]);
        assert_eq!(found[2].others.len(), 1);
        assert!(found[2].others[0].ends_with("garden-copy"));
        assert!(clones(&dir.join("missing")).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn makes_sibling_paths_relative() {
        let project = Path::new("/code/site");
        assert_eq!(relative_to_project(project, Path::new("/code/app")), "../app");
        assert_eq!(relative_to_project(project, Path::new("/other/app")), "/other/app");
    }
}
