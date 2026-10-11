//! Latest releases on GitHub, for the precheck and the build.

use crate::shell::output;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

/// Token of the GitHub CLI (`gh auth token`), if it is installed and signed in. Raises the API
/// limit from 60 to 5000 requests per hour; also handed to the app's own build as `GITHUB_TOKEN`, never to Claude.
pub fn token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| output("gh", &["auth", "token"], Path::new("/")).ok().filter(|t| !t.is_empty()))
        .as_deref()
}

/// GET on the GitHub API; `Ok(None)` for 404.
pub(crate) fn get(path: &str) -> Result<Option<serde_json::Value>> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    let mut request = agent
        .get(&format!("https://api.github.com/{path}"))
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "OpenFTPUpdater-AI");
    if let Some(token) = token() {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(ureq::Error::Status(403, _)) => bail!("GitHub-Limit erreicht (mit `gh auth login` gilt ein höheres Limit)"),
        Err(e) => return Err(e).context("GitHub nicht erreichbar"),
    };
    Ok(Some(serde_json::from_reader(response.into_reader()).context("Antwort von GitHub unlesbar")?))
}

/// Tag of the newest published release (no pre-releases, no drafts); `None` if there is none.
pub fn latest_release(repo: &str) -> Result<Option<String>> {
    let Some(json) = get(&format!("repos/{repo}/releases/latest"))? else { return Ok(None) };
    match json["tag_name"].as_str() {
        Some(tag) if !tag.is_empty() => Ok(Some(tag.to_owned())),
        _ => bail!("Antwort ohne tag_name"),
    }
}

/// Like `latest_release`, but a missing release is an error.
pub fn latest_tag(repo: &str) -> Result<String> {
    latest_release(repo)?.context("kein veröffentlichtes Release")
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoInfo {
    /// `owner/name`
    pub full_name: String,
    pub name: String,
    pub description: Option<String>,
    pub private: bool,
    pub fork: bool,
    pub archived: bool,
    /// SPDX id, e.g. `MIT`; `NOASSERTION` for licenses GitHub does not recognize.
    pub license: Option<String>,
    pub pushed_at: String,
}

impl RepoInfo {
    fn from_json(json: &serde_json::Value) -> Option<Self> {
        Some(Self {
            full_name: json["full_name"].as_str()?.to_owned(),
            name: json["name"].as_str()?.to_owned(),
            description: json["description"].as_str().filter(|d| !d.trim().is_empty()).map(str::to_owned),
            private: json["private"].as_bool().unwrap_or(false),
            fork: json["fork"].as_bool().unwrap_or(false),
            archived: json["archived"].as_bool().unwrap_or(false),
            license: json["license"]["spdx_id"].as_str().map(str::to_owned),
            pushed_at: json["pushed_at"].as_str().unwrap_or_default().to_owned(),
        })
    }
}

/// `owner/name`, both parts non-empty and without spaces or further slashes.
pub fn is_repo_name(text: &str) -> bool {
    let mut parts = text.split('/');
    let valid = |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)));
    valid(parts.next()) && valid(parts.next()) && parts.next().is_none()
}

pub fn repo_info(repo: &str) -> Result<RepoInfo> {
    if !is_repo_name(repo) {
        bail!("„{repo}“ ist kein Repository im Format owner/name");
    }
    let json = get(&format!("repos/{repo}"))?.with_context(|| format!("{repo} nicht gefunden (oder privat ohne Zugriff)"))?;
    RepoInfo::from_json(&json).context("Antwort von GitHub unvollständig")
}

/// Repositories the signed-in gh user can reach (own, collaborator, organizations), newest push
/// first; includes private ones. Needs `gh auth login`.
pub fn user_repos() -> Result<Vec<RepoInfo>> {
    if token().is_none() {
        bail!("Für die Liste deiner Repositories die GitHub-CLI anmelden: gh auth login");
    }
    let mut repos = Vec::new();
    for page in 1..=5 {
        let json = get(&format!("user/repos?per_page=100&page={page}&sort=pushed&affiliation=owner,collaborator,organization_member"))?
            .unwrap_or_default();
        let batch: Vec<RepoInfo> = json.as_array().into_iter().flatten().filter_map(RepoInfo::from_json).collect();
        let full = batch.len() == 100;
        repos.extend(batch);
        if !full {
            break;
        }
    }
    Ok(repos)
}

/// Public repositories of a user or organization, newest push first (up to 100).
pub fn owner_repos(owner: &str) -> Result<Vec<RepoInfo>> {
    let json = get(&format!("users/{owner}/repos?per_page=100&type=owner&sort=pushed"))?.unwrap_or_default();
    Ok(json.as_array().into_iter().flatten().filter_map(RepoInfo::from_json).collect())
}

/// Compares dotted versions numerically (`1.10.0` > `1.9.0`); pre-release suffixes are ignored.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|n| n.parse().unwrap_or(0))
            .collect()
    };
    parse(candidate) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn checks_repo_names() {
        assert!(super::is_repo_name("bl0rb/nudgit"));
        assert!(super::is_repo_name("owner/repo.js"));
        assert!(!super::is_repo_name("nudgit"));
        assert!(!super::is_repo_name("a/b/c"));
        assert!(!super::is_repo_name("a b/c"));
    }

    #[test]
    fn compares_numerically() {
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(is_newer("v1.2.0", "1.1.9"));
        assert!(!is_newer("1.2.0", "1.2.0"));
        assert!(!is_newer("0.7.2", "0.7.10"));
    }
}
