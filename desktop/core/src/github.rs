//! Latest releases on GitHub, for the precheck and the build.

use crate::shell::output;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

/// Token of the GitHub CLI (`gh auth token`), if it is installed and signed in. Raises the API
/// limit from 60 to 5000 requests per hour; also handed to the app's own build as `GITHUB_TOKEN`, never to Claude.
pub fn token() -> Option<&'static str> {
    TOKEN.get_or_init(|| output("gh", &["auth", "token"], Path::new("/")).ok().filter(|t| !t.is_empty())).as_deref()
}

static TOKEN: OnceLock<Option<String>> = OnceLock::new();

/// Whether gh is signed in, if `token` already looked; never starts gh itself.
pub fn signed_in() -> Option<bool> {
    TOKEN.get().map(Option::is_some)
}

fn request(method: &str, path: &str) -> ureq::Request {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    let mut request = agent
        .request(method, &format!("https://api.github.com/{path}"))
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "OpenReleasePilot");
    if let Some(token) = token() {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    request
}

/// GitHub refused access with HTTP 403 for another reason than its rate limit (e.g. SSO
/// enforcement, a missing scope). Unlike a network error this is an answer about the repository.
#[derive(Debug)]
pub struct Forbidden(pub String);

impl std::fmt::Display for Forbidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Zugriff verweigert (HTTP 403): {}", self.0)
    }
}

impl std::error::Error for Forbidden {}

/// GET on the GitHub API; `Ok(None)` for 404.
pub(crate) fn get(path: &str) -> Result<Option<serde_json::Value>> {
    #[cfg(test)]
    if let Some(answer) = mock::get(path) {
        return answer;
    }
    let response = match request("GET", path).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(ureq::Error::Status(code @ (403 | 429), response)) => {
            // A 403 is a rate limit only when GitHub says so; otherwise access is denied.
            let limited = code == 429 || response.header("x-ratelimit-remaining") == Some("0");
            let reason = serde_json::from_str(&response.into_string().unwrap_or_default()).map(|json| message(&json)).unwrap_or_default();
            if limited || reason.to_lowercase().contains("rate limit") {
                bail!("GitHub-Limit erreicht{}", if token().is_none() { " (mit `gh auth login` gilt ein höheres Limit)" } else { "" });
            }
            return Err(Forbidden(reason).into());
        }
        Err(e) => return Err(e).context("GitHub nicht erreichbar"),
    };
    Ok(Some(serde_json::from_reader(response.into_reader()).context("Antwort von GitHub unlesbar")?))
}

/// HTTP status of a GET whose answer is only its status, e.g. 204 for "on" and 404 for "off".
pub(crate) fn probe(path: &str) -> Result<u16> {
    #[cfg(test)]
    if let Some(answer) = mock::probe(path) {
        return answer;
    }
    match request("GET", path).call() {
        Ok(response) => Ok(response.status()),
        Err(ureq::Error::Status(code, _)) => Ok(code),
        Err(e) => Err(e).context("GitHub nicht erreichbar"),
    }
}

/// A list endpoint read page by page; `complete` is false when `max_pages` cut it off.
pub(crate) struct Listing {
    pub items: Vec<serde_json::Value>,
    pub complete: bool,
}

/// All pages of a list endpoint (`per_page=100` is added), at most `max_pages`.
pub(crate) fn get_list(path: &str, max_pages: u32) -> Result<Listing> {
    let separator = if path.contains('?') { '&' } else { '?' };
    let mut items = Vec::new();
    for page in 1..=max_pages {
        let batch = get(&format!("{path}{separator}per_page=100&page={page}"))?.unwrap_or_default();
        let batch = batch.as_array().cloned().unwrap_or_default();
        let full = batch.len() == 100;
        items.extend(batch);
        if !full {
            return Ok(Listing { items, complete: true });
        }
    }
    Ok(Listing { items, complete: false })
}

/// A write on the GitHub API with the gh token: HTTP status and JSON body (`Null` when empty).
/// Error statuses are returned, not raised, so the caller can say what GitHub refused.
pub(crate) fn send(method: &str, path: &str, body: Option<&serde_json::Value>) -> Result<(u16, serde_json::Value)> {
    #[cfg(test)]
    if let Some(answer) = mock::send(method, path, body) {
        return answer;
    }
    if token().is_none() {
        bail!("GitHub-CLI nicht angemeldet (gh auth login)");
    }
    let request = request(method, path);
    let result = match body {
        Some(body) => request.set("Content-Type", "application/json").send_string(&body.to_string()),
        None => request.call(),
    };
    let response = match result {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(e) => return Err(e).context("GitHub nicht erreichbar"),
    };
    let status = response.status();
    let text = response.into_string().unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)))
}

/// GitHub's reason in an error response, e.g. `Pull Request is not mergeable`.
pub(crate) fn message(json: &serde_json::Value) -> String {
    json["message"].as_str().unwrap_or("ohne Begründung").to_owned()
}

/// A branch or tag name in an API path: `/` stays, anything but `A-Z a-z 0-9 - . _ ~` is percent-encoded.
pub(crate) fn encode_ref(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Tag of the newest published release (no pre-releases, no drafts); `None` if there is none.
pub fn latest_release(repo: &str) -> Result<Option<String>> {
    let Some(json) = get(&format!("repos/{repo}/releases/latest"))? else { return Ok(None) };
    match json["tag_name"].as_str() {
        Some(tag) if !tag.is_empty() => Ok(Some(tag.to_owned())),
        _ => bail!("Antwort ohne tag_name"),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoInfo {
    /// GitHub's id; stays the same when the repository is renamed.
    pub id: u64,
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
            id: json["id"].as_u64()?,
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

/// GitHub as the tests see it: canned answers per path, every write recorded. Installed per test
/// thread; an unlisted read fails, so a test notices any call it did not expect.
#[cfg(test)]
pub(crate) mod mock {
    use anyhow::{anyhow, Result};
    use serde_json::{json, Value};
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    pub struct Mock {
        /// GET path → body; `None` answers 404.
        pub gets: HashMap<String, Option<Value>>,
        /// GET path → answers in order, the last one repeated; for state that changes while the code waits.
        pub sequences: HashMap<String, std::collections::VecDeque<Option<Value>>>,
        /// `METHOD path` → status and body; unlisted writes succeed.
        pub replies: HashMap<String, (u16, Value)>,
        /// `METHOD path` of every write, with its body.
        pub writes: Vec<(String, Option<Value>)>,
    }

    thread_local! {
        static MOCK: RefCell<Option<Mock>> = const { RefCell::new(None) };
    }

    pub fn install(gets: impl IntoIterator<Item = (String, Option<Value>)>) {
        MOCK.with(|m| *m.borrow_mut() = Some(Mock { gets: gets.into_iter().collect(), ..Mock::default() }));
    }

    pub fn answer(path: &str, body: Option<Value>) {
        MOCK.with(|m| m.borrow_mut().as_mut().expect("mock installed").gets.insert(path.to_owned(), body));
    }

    pub fn sequence(path: &str, answers: Vec<Option<Value>>) {
        MOCK.with(|m| m.borrow_mut().as_mut().expect("mock installed").sequences.insert(path.to_owned(), answers.into()));
    }

    pub fn reply(write: &str, status: u16, body: Value) {
        MOCK.with(|m| m.borrow_mut().as_mut().expect("mock installed").replies.insert(write.to_owned(), (status, body)));
    }

    pub fn writes() -> Vec<String> {
        MOCK.with(|m| m.borrow().as_ref().map(|m| m.writes.iter().map(|(w, _)| w.clone()).collect()).unwrap_or_default())
    }

    pub(super) fn get(path: &str) -> Option<Result<Option<Value>>> {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            let mock = m.as_mut()?;
            if let Some(answers) = mock.sequences.get_mut(path) {
                let answer = if answers.len() > 1 { answers.pop_front() } else { answers.front().cloned() };
                return Some(answer.ok_or_else(|| anyhow!("leere Folge für {path}")));
            }
            Some(mock.gets.get(path).cloned().ok_or_else(|| anyhow!("unerwartete Abfrage {path}")))
        })
    }

    /// A listed body answers 204, a listed `None` 404.
    pub(super) fn probe(path: &str) -> Option<Result<u16>> {
        MOCK.with(|m| {
            let m = m.borrow();
            let mock = m.as_ref()?;
            Some(mock.gets.get(path).map(|body| if body.is_some() { 204 } else { 404 }).ok_or_else(|| anyhow!("unerwartete Abfrage {path}")))
        })
    }

    pub fn written(write: &str) -> Option<Value> {
        MOCK.with(|m| m.borrow().as_ref()?.writes.iter().find(|(w, _)| w == write).and_then(|(_, body)| body.clone()))
    }

    pub(super) fn send(method: &str, path: &str, body: Option<&Value>) -> Option<Result<(u16, Value)>> {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            let mock = m.as_mut()?;
            let key = format!("{method} {path}");
            mock.writes.push((key.clone(), body.cloned()));
            let default = match method {
                "POST" => (201, json!({})),
                "DELETE" => (204, Value::Null),
                _ => (200, json!({})),
            };
            Some(Ok(mock.replies.get(&key).cloned().unwrap_or(default)))
        })
    }
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
