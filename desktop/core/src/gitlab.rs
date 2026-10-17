//! GitLab instances: the REST API (v4) with the token of the GitLab CLI (`glab`), and everything the
//! app reads and writes there, in the same shapes as on GitHub. A project is `host/group/…/name`;
//! the host comes from the clone's origin or from what the user entered, never from the app.
//!
//! The rules are GitHub's: read before every write, give GitLab the expected commit where its API
//! takes one (a merge sends `sha`, so a push in between makes GitLab refuse), never force anything,
//! and treat what cannot be read as unknown, not as fine.

use crate::repocare::{Branch, CiWait, Done, Plan, Proof, PullRequest, ReleaseWait, RunState};
use crate::repos::{Access, Ci, CiState, Details, Facts, Fetch, PrBrief, PrDetail, PrList, RepoTarget, Since, DETAIL_LIMIT};
use crate::shell::{output, Runner};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

// MARK: Identity

/// A GitLab project: its instance and its full path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub host: String,
    /// `group/sub/name`
    pub path: String,
}

impl Project {
    /// `projects/<encoded path>`, the start of every project path in the API.
    fn api(&self) -> String {
        format!("projects/{}", encode(&self.path))
    }

    pub fn id(&self) -> String {
        format!("{}/{}", self.host, self.path)
    }

    pub fn web_url(&self) -> String {
        format!("https://{}/{}", self.host, self.path)
    }
}

/// A host as it appears in front of a GitLab project: has a dot or a port, like `gitlab.example.com`.
pub fn is_host(text: &str) -> bool {
    !text.is_empty() && (text.contains('.') || text.contains(':')) && text.chars().all(|c| c.is_ascii_alphanumeric() || "-.:".contains(c))
}

/// `host/group/…/name` as a project; `None` for a GitHub `owner/name` or anything else.
pub fn project(repo: &str) -> Option<Project> {
    let (host, path) = repo.trim().split_once('/')?;
    let segment = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !p.starts_with('.');
    let parts: Vec<&str> = path.split('/').collect();
    (is_host(host) && parts.len() >= 2 && parts.iter().all(|p| segment(p))).then(|| Project { host: host.to_lowercase(), path: path.to_owned() })
}

/// A path segment or a ref in an API path: everything but `A-Z a-z 0-9 - . _ ~` percent-encoded,
/// the slash too (GitLab wants `group%2Fname` and `feature%2Fx`).
pub fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A dependency-update merge request of Renovate (its user is usually `renovate-bot` or `renovate`).
pub fn is_update_bot(username: &str) -> bool {
    username.to_lowercase().contains("renovate")
}

// MARK: Client

static TOKENS: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);

/// The token `glab` keeps for the host (`glab config get token --host <host>`), asked once per host.
/// Never handed to Claude.
pub fn token(host: &str) -> Option<String> {
    #[cfg(test)]
    if mock::signed_out(host) {
        return None;
    } else {
        return Some("test".into());
    }
    #[allow(unreachable_code)]
    {
        let mut tokens = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
        let tokens = tokens.get_or_insert_with(HashMap::new);
        tokens
            .entry(host.to_owned())
            .or_insert_with(|| {
                output("glab", &["config", "get", "token", "--host", host], Path::new("/")).ok().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
            })
            .clone()
    }
}

/// Whether `glab` has a token for the host, if `token` already asked; never starts glab itself.
pub fn signed_in(host: &str) -> Option<bool> {
    TOKENS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.get(host).map(Option::is_some)
}

/// Forgets the asked tokens, so a login since then counts (on "Status aktualisieren").
pub fn forget_tokens() {
    *TOKENS.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn login_hint(host: &str) -> String {
    format!("glab auth login --hostname {host}")
}

fn request(host: &str, method: &str, path: &str) -> ureq::Request {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    let mut request = agent.request(method, &format!("https://{host}/api/v4/{path}")).set("User-Agent", "OpenReleasePilot");
    if let Some(token) = token(host) {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    request
}

/// GET on the instance's API; `Ok(None)` for 404 (GitLab also answers 404 for what this login may not see).
pub(crate) fn get(host: &str, path: &str) -> Result<Option<Value>> {
    #[cfg(test)]
    if let Some(answer) = crate::github::mock::get(&mock::key(host, path)) {
        return answer;
    }
    let response = match request(host, "GET", path).call() {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(ureq::Error::Status(401, _)) => bail!("GitLab {host}: Anmeldung fehlt oder ist abgelaufen ({})", login_hint(host)),
        Err(ureq::Error::Status(429, _)) => bail!("GitLab {host}: Limit erreicht, später erneut versuchen"),
        Err(ureq::Error::Status(403, response)) => {
            let reason = serde_json::from_str(&response.into_string().unwrap_or_default()).map(|json| message(&json)).unwrap_or_default();
            return Err(crate::github::Forbidden(reason).into());
        }
        Err(e) => return Err(e).with_context(|| format!("GitLab {host} nicht erreichbar")),
    };
    Ok(Some(serde_json::from_reader(response.into_reader()).with_context(|| format!("Antwort von GitLab {host} unlesbar"))?))
}

/// GET of a plain-text answer (a raw file); `Ok(None)` for 404.
pub(crate) fn get_text(host: &str, path: &str) -> Result<Option<String>> {
    #[cfg(test)]
    if let Some(answer) = crate::github::mock::get(&mock::key(host, path)) {
        return answer.map(|body| body.map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())));
    }
    match request(host, "GET", path).call() {
        Ok(response) => Ok(Some(response.into_string().with_context(|| format!("Antwort von GitLab {host} unlesbar"))?)),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(ureq::Error::Status(401, _)) => bail!("GitLab {host}: Anmeldung fehlt oder ist abgelaufen ({})", login_hint(host)),
        Err(ureq::Error::Status(403, _)) => bail!("GitLab {host}: Zugriff verweigert"),
        Err(e) => Err(e).with_context(|| format!("GitLab {host} nicht erreichbar")),
    }
}

/// All pages of a list endpoint (`per_page=100` is added), at most `max_pages`.
pub(crate) fn get_list(host: &str, path: &str, max_pages: u32) -> Result<crate::github::Listing> {
    let separator = if path.contains('?') { '&' } else { '?' };
    let mut items = Vec::new();
    for page in 1..=max_pages {
        let batch = get(host, &format!("{path}{separator}per_page=100&page={page}"))?.unwrap_or_default();
        let batch = batch.as_array().cloned().unwrap_or_default();
        let full = batch.len() == 100;
        items.extend(batch);
        if !full {
            return Ok(crate::github::Listing { items, complete: true });
        }
    }
    Ok(crate::github::Listing { items, complete: false })
}

/// A write with the glab token: HTTP status and JSON body. Error statuses are returned, not raised.
pub(crate) fn send(host: &str, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
    #[cfg(test)]
    if let Some(answer) = crate::github::mock::send(method, &mock::key(host, path), body) {
        return answer;
    }
    if token(host).is_none() {
        bail!("GitLab-CLI für {host} nicht angemeldet ({})", login_hint(host));
    }
    let request = request(host, method, path);
    let result = match body {
        Some(body) => request.set("Content-Type", "application/json").send_string(&body.to_string()),
        None => request.call(),
    };
    let response = match result {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(e) => return Err(e).with_context(|| format!("GitLab {host} nicht erreichbar")),
    };
    let status = response.status();
    let text = response.into_string().unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// GitLab's reason in an error answer: `message` as text or as field → messages, or `error`.
pub(crate) fn message(json: &Value) -> String {
    match &json["message"] {
        Value::String(text) => text.clone(),
        Value::Object(fields) => fields
            .iter()
            .map(|(field, why)| match why {
                Value::Array(list) => format!("{field}: {}", list.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
                other => format!("{field}: {other}"),
            })
            .collect::<Vec<_>>()
            .join("; "),
        Value::Array(list) => list.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; "),
        _ => json["error"].as_str().map(str::to_owned).unwrap_or_else(|| "ohne Begründung".into()),
    }
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

// MARK: Repository

/// The project as `projects/:id` reports it; `Ok(None)` for 404.
fn info(p: &Project) -> Result<Option<Value>> {
    get(&p.host, &p.api())
}

/// The access level of this login in the project: the higher of project and group membership.
/// `None` when GitLab reports no permissions (no login).
fn access_level(info: &Value) -> Option<u64> {
    let permissions = &info["permissions"];
    if !permissions.is_object() {
        return None;
    }
    let level = |key: &str| permissions[key]["access_level"].as_u64().unwrap_or(0);
    Some(level("project_access").max(level("group_access")))
}

/// Developer (30) and above may push.
const DEVELOPER: u64 = 30;
/// Maintainer (40) and above may change the project's settings and protected branches.
pub(crate) const MAINTAINER: u64 = 40;

/// The project's access facts; `full_name` carries the host, so it stays a GitLab identity.
fn access_from(host: &str, info: &Value) -> Result<Access> {
    Ok(Access {
        full_name: format!("{host}/{}", info["path_with_namespace"].as_str().context("Antwort ohne path_with_namespace")?),
        url: text(&info["web_url"]),
        private: info["visibility"].as_str() != Some("public"),
        archived: info["archived"].as_bool().unwrap_or(false),
        can_push: access_level(info).map(|level| level >= DEVELOPER),
        default_branch: info["default_branch"].as_str().filter(|b| !b.is_empty()).context("Projekt ohne Default-Branch (leeres Repository?)")?.to_owned(),
        deletes_merged: info["remove_source_branch_after_merge"].as_bool() == Some(true),
    })
}

/// Head commit of a branch.
fn branch_head(p: &Project, branch: &str) -> Result<Option<String>> {
    Ok(get(&p.host, &format!("{}/repository/branches/{}", p.api(), encode(branch)))?.and_then(|b| b["commit"]["id"].as_str().map(str::to_owned)))
}

/// Facts for the overview: project, head, pipeline, merge requests, release, tag, commits since.
pub fn fetch_facts(p: &Project, target: &RepoTarget, now: u64) -> Facts {
    let info = match info(p) {
        Ok(Some(info)) => info,
        Ok(None) => {
            let why = format!("GitLab {} meldet 404: gelöscht, verschoben oder ohne Zugriff", p.host);
            return Facts::new(now, Fetch::Failed(why), true);
        }
        Err(e) if e.is::<crate::github::Forbidden>() => return Facts::new(now, Fetch::Failed(format!("{e:#}")), true),
        Err(e) => return Facts::new(now, Fetch::Failed(format!("{e:#}")), false),
    };
    let access = match access_from(&p.host, &info) {
        Ok(access) => access,
        Err(e) => return Facts::new(now, Fetch::Failed(format!("{e:#}")), false),
    };
    let mut facts = Facts::new(now, Fetch::Ok(access.clone()), false);
    facts.head = Fetch::from(branch_head(p, &access.default_branch).and_then(|h| h.with_context(|| format!("Branch {} nicht gefunden", access.default_branch))));
    facts.ci = match facts.head.ok() {
        Some(sha) => Fetch::from(ci_for(p, sha)),
        None => Fetch::NotQueried(format!("HEAD von {} unbekannt", access.default_branch)),
    };
    facts.prs = Fetch::from(open_merge_requests(p, &access.default_branch));
    facts.release = Fetch::from(latest_release(p));
    facts.tag = match (&facts.release, target.tag_prefix()) {
        (Fetch::Ok(Some(_)), _) => Fetch::NotQueried("es gibt ein veröffentlichtes Release".into()),
        (Fetch::Ok(None), Some(prefix)) => Fetch::from(tag_names(p).and_then(|names| match names {
            Some(names) => Ok(crate::repocare::latest_release_tag(&names, prefix)),
            None => bail!("über 300 Tags; der höchste passende ist nicht sicher bestimmbar"),
        })),
        (Fetch::Ok(None), None) => Fetch::NotQueried("keine Release-Regel, also kein Tag-Präfix".into()),
        _ => Fetch::NotQueried("Release-Abfrage fehlgeschlagen".into()),
    };
    let base = match (&facts.release, &facts.tag) {
        (Fetch::Ok(Some(release)), _) => Some(("release", release.clone())),
        (_, Fetch::Ok(Some(tag))) => Some(("tag", tag.clone())),
        _ => None,
    };
    facts.since = match (base, facts.head.ok()) {
        (Some((kind, base)), Some(head)) => Fetch::from(commits_since(p, &base, head).map(|commits| Since { base_kind: kind, base, commits })),
        (None, _) => Fetch::NotQueried("kein veröffentlichtes Release und kein passender Tag".into()),
        (_, None) => Fetch::NotQueried("HEAD unbekannt".into()),
    };
    facts
}

fn open_merge_requests(p: &Project, default_branch: &str) -> Result<PrList> {
    let list = get_list(&p.host, &format!("{}/merge_requests?state=opened", p.api()), 3)?;
    let items = list
        .items
        .iter()
        .map(|mr| {
            let author = text(&mr["author"]["username"]);
            let base = text(&mr["target_branch"]);
            PrBrief {
                number: mr["iid"].as_u64().unwrap_or(0),
                title: text(&mr["title"]),
                dependabot: is_update_bot(&author),
                author,
                draft: mr["draft"].as_bool() == Some(true) || mr["work_in_progress"].as_bool() == Some(true),
                into_default: base == default_branch,
                base,
                url: text(&mr["web_url"]),
                head_sha: text(&mr["sha"]),
            }
        })
        .collect();
    Ok(PrList { items, complete: list.complete })
}

/// The pipeline of `sha`: the newest one for exactly that commit. Errors only when GitLab cannot be
/// asked; an unclear answer is `Unknown`.
pub fn ci_for(p: &Project, sha: &str) -> Result<Ci> {
    let pipelines = get(&p.host, &format!("{}/pipelines?sha={sha}&order_by=id&sort=desc&per_page=20", p.api()))?.unwrap_or(json!([]));
    Ok(ci_state(sha, &pipelines))
}

/// The newest pipeline of the list to one state. Only `success` counts as passed; a pipeline that
/// waits for a manual job or was skipped has no result yet.
pub fn ci_state(sha: &str, pipelines: &Value) -> Ci {
    let ci = |state, detail: String| Ci { state, sha: sha.to_owned(), detail };
    let Some(list) = pipelines.as_array() else {
        return ci(CiState::Unknown, "Antwort von GitLab unvollständig".into());
    };
    let Some(newest) = list.iter().max_by_key(|p| p["id"].as_u64().unwrap_or(0)) else {
        return ci(CiState::NoChecks, "keine Pipeline gefunden".into());
    };
    let id = newest["id"].as_u64().unwrap_or(0);
    match newest["status"].as_str() {
        Some("success") => ci(CiState::Passed, format!("Pipeline #{id} erfolgreich")),
        Some("failed") => ci(CiState::Failed, format!("Pipeline #{id} fehlgeschlagen")),
        Some("canceled" | "canceling") => ci(CiState::Failed, format!("Pipeline #{id} abgebrochen")),
        Some("created" | "pending" | "running" | "preparing" | "scheduled" | "waiting_for_resource" | "waiting_for_callback") => {
            ci(CiState::Pending, format!("Pipeline #{id} läuft noch"))
        }
        Some("manual") => ci(CiState::Unknown, format!("Pipeline #{id} wartet auf einen manuellen Job")),
        Some("skipped") => ci(CiState::Unknown, format!("Pipeline #{id} übersprungen")),
        other => ci(CiState::Unknown, format!("Pipeline #{id}: {}", other.unwrap_or("ohne Status"))),
    }
}

/// Tag names; `None` when there are more than the app reads (300).
pub fn tag_names(p: &Project) -> Result<Option<Vec<String>>> {
    let list = get_list(&p.host, &format!("{}/repository/tags", p.api()), 3)?;
    Ok(list.complete.then(|| list.items.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect()))
}

/// Tag of the newest release; an upcoming release (date in the future) does not count yet.
pub fn latest_release(p: &Project) -> Result<Option<String>> {
    let list = get(&p.host, &format!("{}/releases?order_by=released_at&sort=desc&per_page=20", p.api()))?.unwrap_or(json!([]));
    Ok(list.as_array().into_iter().flatten().find(|r| r["upcoming_release"].as_bool() != Some(true)).and_then(|r| r["tag_name"].as_str()).map(str::to_owned))
}

/// Whether `tag` has a release that `latest_release` can report.
pub fn has_published_release(p: &Project, tag: &str) -> Result<bool> {
    let Some(release) = get(&p.host, &format!("{}/releases/{}", p.api(), encode(tag)))? else { return Ok(false) };
    Ok(release["upcoming_release"].as_bool() != Some(true))
}

/// The commit a tag points to (GitLab resolves annotated tags itself).
pub fn resolve_tag(p: &Project, tag: &str) -> Result<String> {
    let tag_json = get(&p.host, &format!("{}/repository/tags/{}", p.api(), encode(tag)))?.with_context(|| format!("Tag {tag} nicht gefunden"))?;
    Ok(tag_json["commit"]["id"].as_str().context("Tag ohne Commit")?.to_owned())
}

/// The commit of a branch, tag or commit; `None` if GitLab does not know it.
fn commit_of(p: &Project, rev: &str) -> Result<Option<String>> {
    Ok(get(&p.host, &format!("{}/repository/commits/{}", p.api(), encode(rev)))?.and_then(|c| c["id"].as_str().map(str::to_owned)))
}

fn merge_base(p: &Project, a: &str, b: &str) -> Result<Option<String>> {
    Ok(get(&p.host, &format!("{}/repository/merge_base?refs[]={}&refs[]={}", p.api(), encode(a), encode(b)))?.and_then(|c| c["id"].as_str().map(str::to_owned)))
}

/// Commits in `head` since `base` (a tag), only where `base` is an ancestor of `head`.
pub fn commits_since(p: &Project, base: &str, head: &str) -> Result<u64> {
    let base_sha = commit_of(p, base)?.with_context(|| format!("{base} nicht gefunden"))?;
    if base_sha == head {
        return Ok(0);
    }
    let common = merge_base(p, &base_sha, head)?.context("nicht bestimmbar: kein gemeinsamer Vorfahr")?;
    if common == head {
        bail!("nicht bestimmbar: die Basis liegt nach HEAD");
    }
    if common != base_sha {
        bail!("nicht bestimmbar: Historie divergiert, die Basis ist kein Vorfahr von HEAD");
    }
    let compare = get(&p.host, &format!("{}/repository/compare?from={base_sha}&to={head}", p.api()))?.context("Vergleich nicht möglich")?;
    Ok(compare["commits"].as_array().map(|c| c.len() as u64).unwrap_or(0))
}

/// Every commit of `rev` (a branch or commit) is in `base`; a commit GitLab does not know is not contained.
fn contained(p: &Project, base: &str, rev: &str) -> Result<bool> {
    let Some(sha) = commit_of(p, rev)? else { return Ok(false) };
    Ok(merge_base(p, base, &sha)?.as_deref() == Some(sha.as_str()))
}

// MARK: Merge requests

/// GitLab's merge status in GitHub's words (`clean`, `dirty`, `behind`, `blocked`, `unstable`,
/// `draft`), so the rules and texts are shared; `None` while GitLab is still checking.
pub fn mergeable_state(mr: &Value) -> Option<&'static str> {
    if let Some(detailed) = mr["detailed_merge_status"].as_str() {
        return match detailed {
            "mergeable" => Some("clean"),
            "conflict" => Some("dirty"),
            "need_rebase" => Some("behind"),
            "draft_status" => Some("draft"),
            "ci_must_pass" | "ci_still_running" => Some("unstable"),
            "checking" | "unchecked" | "preparing" | "approvals_syncing" => None,
            _ => Some("blocked"),
        };
    }
    // Before GitLab 15.6: only the coarse status.
    if mr["draft"].as_bool() == Some(true) || mr["work_in_progress"].as_bool() == Some(true) {
        return Some("draft");
    }
    match mr["merge_status"].as_str() {
        Some("can_be_merged") => Some("clean"),
        Some("cannot_be_merged") if mr["has_conflicts"].as_bool() == Some(true) => Some("dirty"),
        Some("cannot_be_merged") => Some("blocked"),
        _ => None,
    }
}

/// The merge request with its merge status computed; GitLab computes it in the background.
pub(crate) fn merge_request(p: &Project, iid: u64) -> Result<Value> {
    let path = format!("{}/merge_requests/{iid}", p.api());
    for _ in 0..5 {
        let mr = get(&p.host, &path)?.with_context(|| format!("MR !{iid} nicht gefunden"))?;
        if mergeable_state(&mr).is_some() || mr["state"].as_str() != Some("opened") {
            return Ok(mr);
        }
        #[cfg(not(test))]
        thread::sleep(Duration::from_secs(2));
    }
    get(&p.host, &path)?.with_context(|| format!("MR !{iid} nicht gefunden"))
}

/// Mergeability and pipeline of the open merge requests, for the detail view; read only.
pub fn fetch_details(p: &Project, prs: &PrList, now: u64) -> Details {
    let details = prs
        .items
        .iter()
        .take(DETAIL_LIMIT)
        .map(|pr| {
            let (state, mergeable) = match merge_request(p, pr.number) {
                Ok(mr) => match mergeable_state(&mr) {
                    Some(state) => (Some(state.to_owned()), crate::repos::mergeable_text(state).replace("GitHub", "GitLab")),
                    None => (None, "von GitLab noch nicht bestimmt".to_owned()),
                },
                Err(e) => (None, format!("nicht abfragbar: {e:#}")),
            };
            PrDetail { number: pr.number, mergeable_state: state, mergeable, ci: Fetch::from(ci_for(p, &pr.head_sha)) }
        })
        .collect();
    Details { fetched_at: now, prs: details, complete: prs.complete && prs.items.len() <= DETAIL_LIMIT }
}

fn same_project(mr: &Value) -> bool {
    mr["source_project_id"].as_u64().is_some() && mr["source_project_id"] == mr["target_project_id"]
}

fn pull_request(mr: &Value) -> PullRequest {
    let author = text(&mr["author"]["username"]);
    PullRequest {
        number: mr["iid"].as_u64().unwrap_or(0),
        title: text(&mr["title"]),
        dependabot: is_update_bot(&author),
        author,
        head_sha: text(&mr["sha"]),
        head_branch: same_project(mr).then(|| text(&mr["source_branch"])),
        base: text(&mr["target_branch"]),
        url: text(&mr["web_url"]),
        size: mr["changes_count"].as_str().map(|n| format!("{n} Dateien")).unwrap_or_default(),
        ..PullRequest::default()
    }
}

/// `None` when GitLab reports the merge request mergeable and the pipeline of its head passed.
fn gate(p: &Project, mr: &Value) -> Result<Option<String>> {
    if mr["draft"].as_bool() == Some(true) || mr["work_in_progress"].as_bool() == Some(true) {
        return Ok(Some("Entwurf".into()));
    }
    if let Some(reason) = blocked_reason(mergeable_state(mr)) {
        return Ok(Some(reason.into()));
    }
    let ci = ci_for(p, mr["sha"].as_str().context("MR ohne sha")?)?;
    if ci.state != CiState::Passed {
        return Ok(Some(match ci.state {
            CiState::NoChecks => "keine Pipeline, also nichts, was grün sein könnte".into(),
            _ => format!("Pipeline nicht grün ({})", ci.detail),
        }));
    }
    Ok(None)
}

fn blocked_reason(state: Option<&str>) -> Option<&'static str> {
    Some(match state {
        Some("clean") => return None,
        Some("unstable") => "Pipeline nicht grün (fehlgeschlagen oder läuft noch)",
        Some("blocked") => "blockiert (Freigaben, Diskussionen oder Pflicht-Prüfungen fehlen)",
        Some("behind") => "nicht auf dem Stand des Zielbranches (Rebase nötig)",
        Some("dirty") => "Merge-Konflikt",
        Some("draft") => "Entwurf",
        _ => "GitLab hat die Mergebarkeit noch nicht bestimmt",
    })
}

/// File paths of a merge request, for the prompt; `…` when cut.
fn mr_files(p: &Project, iid: u64) -> Result<Vec<String>> {
    let base = format!("{}/merge_requests/{iid}", p.api());
    // `/diffs` since GitLab 15.7, `/changes` before.
    let list = get_list(&p.host, &format!("{base}/diffs"), 1);
    let (mut files, complete) = match list {
        Ok(list) => (list.items.iter().filter_map(|d| d["new_path"].as_str().map(str::to_owned)).collect::<Vec<_>>(), list.complete),
        Err(_) => {
            let changes = get(&p.host, &format!("{base}/changes"))?.unwrap_or_default();
            let files = changes["changes"].as_array().into_iter().flatten().filter_map(|d| d["new_path"].as_str().map(str::to_owned)).collect();
            (files, changes["overflow"].as_bool() != Some(true))
        }
    };
    if !complete {
        files.push("…".into());
    }
    Ok(files)
}

// MARK: Care preview

/// The GitLab half of `repocare::scan`: project, open merge requests into the default branch with
/// GitLab's verdict, branches to delete. Reads only.
pub fn look(plan: &mut Plan, target: &RepoTarget, merge: bool, cleanup: bool) -> Result<()> {
    let p = project(&plan.repo).context("kein GitLab-Projekt")?;
    let info = info(&p)?.with_context(|| format!("nicht gefunden oder kein Zugriff (GitLab {} meldet 404)", p.host))?;
    if info["archived"].as_bool() == Some(true) {
        bail!("archiviert: Pflege gesperrt");
    }
    if access_level(&info).is_some_and(|level| level < DEVELOPER) {
        bail!("nur lesbar: kein Schreibzugriff, Pflege gesperrt");
    }
    let default = info["default_branch"].as_str().context("Projekt ohne Default-Branch")?.to_owned();
    plan.default_branch = default.clone();
    // GitLab merges with the project's own method; the app does not choose one.
    plan.merge_method = info["merge_method"].as_str().unwrap_or("merge").to_owned();
    plan.github_deletes_merged = false;
    plan.base_head = head(plan)?;

    let open = get_list(&p.host, &format!("{}/merge_requests?state=opened", p.api()), 3)?;
    if merge {
        if !open.complete {
            plan.review_note = Some("mehr als 300 offene MRs; geprüft werden die ersten 300".into());
        }
        for listed in open.items.iter().filter(|mr| mr["target_branch"].as_str() == Some(default.as_str())) {
            let iid = listed["iid"].as_u64().unwrap_or(0);
            let detail = merge_request(&p, iid)?;
            let mut pr = pull_request(&detail);
            let gate = gate(&p, &detail)?;
            pr.ready = gate.is_none();
            pr.conflict = matches!(mergeable_state(&detail), Some("dirty" | "behind"));
            pr.gate = gate.unwrap_or_default();
            pr.reason = if pr.ready { "von Claude noch nicht bewertet".into() } else { pr.gate.clone() };
            pr.files = mr_files(&p, iid)?;
            plan.prs.push(pr);
        }
    }
    if cleanup {
        if !open.complete {
            plan.cleanup_note = Some("mehr als 300 offene MRs: nicht sicher, welche Branches noch gebraucht werden; kein Aufräumen".into());
            return Ok(());
        }
        let merged = merged_heads(&p)?;
        let heads: HashSet<&str> = open.items.iter().filter(|mr| same_project(mr)).filter_map(|mr| mr["source_branch"].as_str()).collect();
        let bases: HashSet<&str> = open.items.iter().filter_map(|mr| mr["target_branch"].as_str()).collect();
        let branches = get_list(&p.host, &format!("{}/repository/branches", p.api()), 5)?;
        if !branches.complete {
            plan.cleanup_note = Some("mehr als 500 Branches; geprüft wurden die ersten 500".into());
        }
        for branch in &branches.items {
            let (Some(name), Some(sha)) = (branch["name"].as_str(), branch["commit"]["id"].as_str()) else { continue };
            if name == default || branch["protected"].as_bool() == Some(true) || branch["default"].as_bool() == Some(true) || heads.contains(name) || bases.contains(name) {
                continue;
            }
            let (proof, reason) = match merged.get(sha) {
                Some(&iid) => (Proof::MergedPr(iid), format!("MR !{iid} gemergt")),
                None if contained(&p, &default, sha)? => (Proof::Contained, format!("in {default} enthalten")),
                None => continue,
            };
            plan.branches.push(Branch { name: name.to_owned(), sha: sha.to_owned(), reason, proof });
        }
        crate::repocare::look_local_of(plan, target, &merged);
    }
    Ok(())
}

/// Head commit → iid of the recently merged merge requests.
fn merged_heads(p: &Project) -> Result<HashMap<String, u64>> {
    Ok(get_list(&p.host, &format!("{}/merge_requests?state=merged&order_by=updated_at&sort=desc", p.api()), 1)?
        .items
        .iter()
        .filter_map(|mr| Some((mr["sha"].as_str()?.to_owned(), mr["iid"].as_u64()?)))
        .collect())
}

/// Head commit of the default branch.
pub fn head(plan: &Plan) -> Result<String> {
    let p = project(&plan.repo).context("kein GitLab-Projekt")?;
    branch_head(&p, &plan.default_branch)?.context("Default-Branch nicht gefunden")
}

// MARK: Care run

/// Why the merge request is no longer the one the preview judged; `None` if it still is.
fn changed(plan: &Plan, pr: &PullRequest, current: &Value) -> Option<String> {
    match current["state"].as_str() {
        Some("opened") => {}
        Some("merged") => return Some("schon gemergt".into()),
        _ => return Some("nicht mehr offen".into()),
    }
    if current["sha"].as_str() != Some(pr.head_sha.as_str()) {
        return Some("hat seit der Vorschau neue Commits".into());
    }
    let base = current["target_branch"].as_str().unwrap_or("?");
    if base != plan.default_branch {
        return Some(format!("zielt jetzt auf {base} statt auf {}", plan.default_branch));
    }
    None
}

fn note(p: &Project, iid: u64, body: &str) -> Result<()> {
    match send(&p.host, "POST", &format!("{}/merge_requests/{iid}/notes", p.api()), Some(&json!({ "body": body })))? {
        (201, _) => Ok(()),
        (_, answer) => bail!("GitLab lehnt den Kommentar ab: {}", message(&answer)),
    }
}

/// Asks Renovate to rebuild the merge request: the label it watches (`rebase`).
fn ask_rebase(p: &Project, iid: u64) -> Result<()> {
    match send(&p.host, "PUT", &format!("{}/merge_requests/{iid}", p.api()), Some(&json!({ "add_labels": "rebase" })))? {
        (200, _) => Ok(()),
        (_, answer) => bail!("GitLab lehnt das Label ab: {}", message(&answer)),
    }
}

/// Merges a planned merge request after checking it again, as `repocare::merge` does on GitHub.
/// GitLab gets the expected head (`sha`), so a push in between makes it refuse; the merge method
/// is the project's own.
pub fn merge(plan: &Plan, pr: &PullRequest, state: &mut RunState) -> Result<Done> {
    let p = project(&plan.repo).context("kein GitLab-Projekt")?;
    let current = merge_request(&p, pr.number)?;
    if let Some(why) = changed(plan, pr, &current) {
        return Ok(Done::Skipped(why));
    }
    if current["draft"].as_bool() == Some(true) || current["work_in_progress"].as_bool() == Some(true) {
        return Ok(Done::Skipped("ist inzwischen ein Entwurf".into()));
    }
    crate::repocare::note_head_of(plan, state)?;
    let mergeable = mergeable_state(&current);
    if let Some(reason) = blocked_reason(mergeable) {
        if pr.dependabot && matches!(mergeable, Some("dirty" | "behind")) && !state.merged.is_empty() {
            ask_rebase(&p, pr.number)?;
            return Ok(Done::Skipped(format!("{reason} nach den Merges dieses Laufs; Renovate baut den MR neu auf (Label „rebase“), eine neue Vorschau kann ihn mergen")));
        }
        return Ok(Done::Skipped(reason.into()));
    }
    let ci = ci_for(&p, &pr.head_sha)?;
    if ci.state != CiState::Passed {
        return Ok(Done::Skipped(format!("Pipeline des MR-Commits nicht grün ({})", ci.detail)));
    }
    let path = format!("{}/merge_requests/{}/merge", p.api(), pr.number);
    match send(&p.host, "PUT", &path, Some(&json!({ "sha": pr.head_sha }))) {
        Ok((200, answer)) => {
            state.merged.insert(pr.number);
            state.expected_head = merged_commit(&answer);
            Ok(Done::Ok(format!("gemergt ({})", plan.merge_method)))
        }
        Ok((409, _)) => Ok(Done::Skipped("hat seit der Prüfung neue Commits (GitLab lehnt ab)".into())),
        Ok((_, answer)) => bail!("GitLab lehnt den Merge ab: {}", message(&answer)),
        Err(e) => {
            let now = get(&p.host, &format!("{}/merge_requests/{}", p.api(), pr.number))?.unwrap_or_default();
            if now["state"].as_str() == Some("merged") && now["sha"].as_str() == Some(pr.head_sha.as_str()) {
                state.merged.insert(pr.number);
                state.expected_head = merged_commit(&now);
                return Ok(Done::Ok("gemergt (Antwort verloren, Zustand bei GitLab nachgelesen)".into()));
            }
            Err(e.context("Ergebnis des Merges unklar; laut GitLab ist der MR nicht gemergt"))
        }
    }
}

/// Where the default branch stands after a merge: the merge commit, the squash commit, or (fast
/// forward) the merge request's head.
fn merged_commit(mr: &Value) -> Option<String> {
    ["merge_commit_sha", "squash_commit_sha", "sha"].iter().find_map(|key| mr[*key].as_str().filter(|s| !s.is_empty())).map(str::to_owned)
}

/// Closes a superfluous merge request with the reason as a comment, as `repocare::close` does.
pub fn close(plan: &Plan, pr: &PullRequest, commented: bool) -> Result<Done> {
    let p = project(&plan.repo).context("kein GitLab-Projekt")?;
    let current = get(&p.host, &format!("{}/merge_requests/{}", p.api(), pr.number))?.with_context(|| format!("MR !{} nicht gefunden", pr.number))?;
    if current["state"].as_str() != Some("opened") {
        return Ok(Done::Skipped("war schon geschlossen".into()));
    }
    if let Some(why) = changed(plan, pr, &current) {
        return Ok(Done::Skipped(why));
    }
    if let Some(other) = pr.superseded_by {
        match get(&p.host, &format!("{}/merge_requests/{other}", p.api()))? {
            None => return Ok(Done::Skipped(format!("Ersatz-MR !{other} nicht gefunden; die Begründung trägt nicht mehr"))),
            Some(mr) if mr["state"].as_str() == Some("closed") => {
                return Ok(Done::Skipped(format!("Ersatz-MR !{other} wurde ohne Merge geschlossen; die Begründung trägt nicht mehr")));
            }
            Some(_) => {}
        }
    }
    if !commented {
        note(&p, pr.number, &format!("{} {}", crate::repocare::CLOSE_COMMENT, pr.reason)).context("Kommentar nicht gepostet; der MR bleibt offen")?;
    }
    let path = format!("{}/merge_requests/{}", p.api(), pr.number);
    match send(&p.host, "PUT", &path, Some(&json!({ "state_event": "close" }))) {
        Ok((200, answer)) if answer["state"].as_str() == Some("closed") || answer.is_null() || answer == json!({}) => Ok(Done::Ok("Kommentar gepostet, geschlossen".into())),
        Ok((_, answer)) => bail!("Kommentar gepostet, aber GitLab lehnt das Schließen ab: {}", message(&answer)),
        Err(e) => {
            let now = get(&p.host, &path)?.unwrap_or_default();
            if now["state"].as_str() == Some("closed") {
                return Ok(Done::Ok("Kommentar gepostet, geschlossen (Zustand bei GitLab nachgelesen)".into()));
            }
            Err(e.context("Kommentar gepostet, Ergebnis des Schließens unklar; laut GitLab ist der MR offen"))
        }
    }
}

/// Asks Renovate to rebuild a conflicting merge request, after checking it again.
pub fn rebase(plan: &Plan, pr: &PullRequest) -> Result<Done> {
    let p = project(&plan.repo).context("kein GitLab-Projekt")?;
    let current = merge_request(&p, pr.number)?;
    if let Some(why) = changed(plan, pr, &current) {
        return Ok(Done::Skipped(why));
    }
    if !is_update_bot(current["author"]["username"].as_str().unwrap_or_default()) {
        return Ok(Done::Skipped("kein Renovate-MR".into()));
    }
    match mergeable_state(&current) {
        Some("dirty" | "behind") => {}
        Some(other) => return Ok(Done::Skipped(format!("kein Konflikt mehr ({})", crate::repos::mergeable_text(other)))),
        None => return Ok(Done::Skipped("Mergebarkeit von GitLab noch nicht bestimmt".into())),
    }
    ask_rebase(&p, pr.number)?;
    Ok(Done::Ok("Label „rebase“ gesetzt; Renovate baut den MR neu auf".into()))
}

/// Whether a comment starting with `prefix` was posted on the merge request since `since`.
pub fn commented_since(repo: &str, iid: u64, prefix: &str, since: u64) -> Result<bool> {
    let p = project(repo).context("kein GitLab-Projekt")?;
    let list = get_list(&p.host, &format!("{}/merge_requests/{iid}/notes?sort=desc&order_by=created_at", p.api()), 3)?;
    let since = crate::repocare::iso8601(since);
    Ok(list.items.iter().any(|n| {
        let at = n["created_at"].as_str().unwrap_or_default();
        at.get(..19).unwrap_or(at) >= since.get(..19).unwrap_or(&since) && n["body"].as_str().is_some_and(|b| b.starts_with(prefix))
    }))
}

/// Deletes a planned branch after checking it again, as `repocare::delete_branch` does.
pub fn delete_branch(plan: &Plan, branch: &Branch, state: &RunState) -> Result<Done> {
    let p = project(&plan.repo).context("kein GitLab-Projekt")?;
    if let Proof::AfterMerge(iid) = branch.proof {
        if !state.merged.contains(&iid) {
            return Ok(Done::Skipped(format!("MR !{iid} wurde nicht gemergt")));
        }
    }
    let path = format!("{}/repository/branches/{}", p.api(), encode(&branch.name));
    let Some(current) = get(&p.host, &path)? else {
        // A 404 for the branch means it is gone only while the project itself is still readable.
        if info(&p)?.is_none() {
            return Ok(Done::Skipped("Projekt nicht lesbar; ob der Branch noch besteht, ist unklar".into()));
        }
        return Ok(Done::Nothing("war schon gelöscht".into()));
    };
    if current["commit"]["id"].as_str() != Some(branch.sha.as_str()) {
        return Ok(Done::Skipped("hat seit der Vorschau neue Commits".into()));
    }
    if current["protected"].as_bool() == Some(true) {
        return Ok(Done::Skipped("ist geschützt".into()));
    }
    let project_info = info(&p)?.context("Projekt nicht gefunden")?;
    if project_info["default_branch"].as_str() == Some(branch.name.as_str()) {
        return Ok(Done::Skipped("ist jetzt der Default-Branch".into()));
    }
    for (query, what) in [("source_branch", "kommt aus ihm"), ("target_branch", "zielt auf ihn")] {
        let open = get_list(&p.host, &format!("{}/merge_requests?state=opened&{query}={}", p.api(), encode(&branch.name)), 1)?;
        if let Some(mr) = open.items.first() {
            return Ok(Done::Skipped(format!("offener MR !{} {what}", mr["iid"].as_u64().unwrap_or(0))));
        }
    }
    if let Some(why) = proof_gone(plan, branch, state, false)? {
        return Ok(Done::Skipped(why));
    }
    match send(&p.host, "DELETE", &path, None) {
        Ok((204, _)) => Ok(Done::Ok("gelöscht".into())),
        Ok((_, answer)) => bail!("GitLab lehnt das Löschen ab: {}", message(&answer)),
        Err(e) => {
            if get(&p.host, &path)?.is_none() {
                return Ok(Done::Ok("gelöscht (Antwort verloren, Zustand bei GitLab nachgelesen)".into()));
            }
            Err(e.context("Ergebnis unklar; laut GitLab gibt es den Branch noch"))
        }
    }
}

/// Why the branch's proof no longer holds, judged by GitLab's current state.
pub fn proof_gone(plan: &Plan, branch: &Branch, state: &RunState, local: bool) -> Result<Option<String>> {
    let p = project(&plan.repo).context("kein GitLab-Projekt")?;
    Ok(match branch.proof {
        Proof::AfterMerge(iid) => (!state.merged.contains(&iid)).then(|| format!("MR !{iid} wurde nicht gemergt")),
        Proof::MergedPr(iid) => {
            let mr = get(&p.host, &format!("{}/merge_requests/{iid}", p.api()))?.unwrap_or_default();
            let holds = mr["state"].as_str() == Some("merged") && mr["sha"].as_str() == Some(branch.sha.as_str());
            (!holds).then(|| format!("MR !{iid} belegt den Merge nicht mehr"))
        }
        Proof::Contained => {
            let rev = if local { &branch.sha } else { &branch.name };
            let holds = contained(&p, &plan.default_branch, rev)?;
            (!holds).then(|| format!("nicht mehr vollständig in {} auf GitLab enthalten", plan.default_branch))
        }
    })
}

/// Sets a tag on `sha`; `Ok(false)` when GitLab refuses because it exists.
pub fn create_tag(repo: &str, tag: &str, sha: &str) -> Result<Result<(), String>> {
    let p = project(repo).context("kein GitLab-Projekt")?;
    match send(&p.host, "POST", &format!("{}/repository/tags", p.api()), Some(&json!({ "tag_name": tag, "ref": sha }))) {
        Ok((201, _)) => Ok(Ok(())),
        Ok((400 | 409, answer)) => Ok(Err(message(&answer))),
        Ok((_, answer)) => bail!("GitLab lehnt den Tag ab: {}", message(&answer)),
        Err(e) => {
            if get(&p.host, &format!("{}/repository/tags/{}", p.api(), encode(tag)))?.is_some_and(|t| t["commit"]["id"].as_str() == Some(sha)) {
                return Ok(Ok(()));
            }
            Err(e.context("Ergebnis unklar; laut GitLab gibt es den Tag nicht"))
        }
    }
}

/// Waits until the GitLab release of `tag` exists and the tag still points to `commit`; stops early
/// when a pipeline for the tag failed. Reads only.
pub fn wait_for_release(p: &Project, tag: &str, commit: &str, runner: &Runner, wait: &CiWait, progress: &dyn Fn(&str)) -> Result<ReleaseWait> {
    let start = Instant::now();
    let mut reported = false;
    loop {
        runner.check()?;
        let now = resolve_tag(p, tag)?;
        if now != commit {
            return Ok(ReleaseWait::Failed(format!("{tag} zeigt inzwischen auf {} statt auf {}", short(&now), short(commit))));
        }
        if let Some(release) = get(&p.host, &format!("{}/releases/{}", p.api(), encode(tag)))? {
            if release["upcoming_release"].as_bool() != Some(true) {
                let url = release["_links"]["self"].as_str().map(str::to_owned).unwrap_or_else(|| format!("{}/-/releases/{tag}", p.web_url()));
                return Ok(ReleaseWait::Published { url });
            }
        }
        // A failed tag pipeline means no release; waiting longer would not change that.
        if let Ok(Some(pipelines)) = get(&p.host, &format!("{}/pipelines?ref={}&sha={commit}&per_page=20", p.api(), encode(tag))) {
            let failed = pipelines.as_array().into_iter().flatten().max_by_key(|pl| pl["id"].as_u64().unwrap_or(0)).filter(|pl| matches!(pl["status"].as_str(), Some("failed" | "canceled")));
            if let Some(pipeline) = failed {
                return Ok(ReleaseWait::WorkflowFailed(format!(
                    "Pipeline #{} für {tag} endete mit {}: {}",
                    pipeline["id"].as_u64().unwrap_or(0),
                    pipeline["status"].as_str().unwrap_or("?"),
                    pipeline["web_url"].as_str().unwrap_or_default()
                )));
            }
        }
        if start.elapsed() >= wait.timeout {
            return Ok(ReleaseWait::Pending(format!("nach {} Minuten noch kein Release {tag}", wait.timeout.as_secs() / 60)));
        }
        if !reported {
            progress(&format!("warte auf das Release {tag} …"));
            reported = true;
        }
        let mut slept = Duration::ZERO;
        while slept < wait.poll {
            runner.check()?;
            let step = Duration::from_secs(1).min(wait.poll - slept);
            thread::sleep(step);
            slept += step;
        }
    }
}

// MARK: Content

/// The default branch and its head, for a source pinned to the current state.
pub fn default_head(p: &Project) -> Result<(String, String)> {
    let info = info(p)?.with_context(|| format!("{} nicht gefunden", p.id()))?;
    let branch = info["default_branch"].as_str().context("Projekt ohne Default-Branch")?.to_owned();
    let head = branch_head(p, &branch)?.with_context(|| format!("Branch {branch} nicht gefunden"))?;
    Ok((branch, head))
}

/// Subjects of the commits from `from` to `to` (oldest first, at most 60).
pub fn commits_between(p: &Project, from: &str, to: &str) -> Vec<String> {
    let Ok(Some(compare)) = get(&p.host, &format!("{}/repository/compare?from={}&to={}", p.api(), encode(from), encode(to))) else { return Vec::new() };
    let commits = compare["commits"].as_array().cloned().unwrap_or_default();
    let skip = commits.len().saturating_sub(60);
    commits
        .iter()
        .skip(skip)
        .filter_map(|c| c["title"].as_str().map(|subject| format!("{} {subject}", short(c["id"].as_str().unwrap_or("")))))
        .collect()
}

/// Has the target branch moved since the proposal, and did anyone touch the proposed files?
pub fn docs_conflict(p: &Project, base_branch: &str, base_commit: &str, paths: &[&str]) -> Result<Option<String>> {
    let head = branch_head(p, base_branch)?.with_context(|| format!("Branch {base_branch} nicht mehr gefunden"))?;
    if head == base_commit {
        return Ok(None);
    }
    if merge_base(p, base_commit, &head)?.as_deref() != Some(base_commit) {
        return Ok(Some(format!("{base_branch} wurde seit dem Vorschlag umgeschrieben")));
    }
    let compare = get(&p.host, &format!("{}/repository/compare?from={base_commit}&to={head}", p.api()))?.context("Vergleich nicht möglich")?;
    let diffs = compare["diffs"].as_array().cloned().unwrap_or_default();
    let changed: Vec<&str> = diffs.iter().flat_map(|d| [d["new_path"].as_str(), d["old_path"].as_str()]).flatten().collect();
    let touched: Vec<&str> = paths.iter().copied().filter(|p| changed.contains(p)).collect();
    if !touched.is_empty() {
        return Ok(Some(format!("{} wurde seit dem Vorschlag auf {base_branch} geändert", touched.join(", "))));
    }
    if compare["compare_timeout"].as_bool() == Some(true) || diffs.len() >= 1000 {
        return Ok(Some(format!("{base_branch} hat sich stark bewegt; nicht sicher prüfbar")));
    }
    Ok(None)
}

/// One file of a docs proposal for the commit.
pub struct DocsFile<'a> {
    pub path: &'a str,
    pub added: bool,
    pub executable: bool,
    pub data: &'a [u8],
}

/// A merge request as a delivery: iid, page, the commit it was opened for.
pub struct Delivered {
    pub iid: u64,
    pub url: String,
    pub commit: String,
}

/// Creates the commit with exactly the approved files on a new branch from the proposal's base
/// commit, then a merge request into the base branch. Picks up what an interrupted earlier
/// attempt created. `Err` with `content::Conflict` when someone else owns the branch.
pub fn publish_docs(p: &Project, branch: &str, base_branch: &str, base_commit: &str, files: &[DocsFile], title: &str, body: &str) -> Result<Delivered> {
    if let Some(found) = find_delivery(p, branch, base_commit, files)? {
        return Ok(found);
    }
    use base64::Engine;
    let actions: Vec<Value> = files
        .iter()
        .map(|f| {
            json!({
                "action": if f.added { "create" } else { "update" },
                "file_path": f.path,
                "content": base64::engine::general_purpose::STANDARD.encode(f.data),
                "encoding": "base64",
                "execute_filemode": f.executable,
            })
        })
        .collect();
    let commit_message = format!("{title}\n\n{}", body.lines().take(20).collect::<Vec<_>>().join("\n"));
    let request = json!({ "branch": branch, "start_sha": base_commit, "commit_message": commit_message, "actions": actions });
    let commit = match send(&p.host, "POST", &format!("{}/repository/commits", p.api()), Some(&request))? {
        (201, commit) => commit["id"].as_str().context("Commit ohne id")?.to_owned(),
        (_, answer) => {
            // The branch may exist from an interrupted attempt; only that attempt's content is accepted.
            if let Some(found) = find_delivery(p, branch, base_commit, files)? {
                return Ok(found);
            }
            bail!("GitLab lehnt den Commit ab: {}", message(&answer))
        }
    };
    open_merge_request(p, branch, base_branch, &commit, title, body)
}

fn open_merge_request(p: &Project, branch: &str, base_branch: &str, commit: &str, title: &str, body: &str) -> Result<Delivered> {
    let request = json!({ "source_branch": branch, "target_branch": base_branch, "title": title, "description": body, "remove_source_branch": true });
    match send(&p.host, "POST", &format!("{}/merge_requests", p.api()), Some(&request))? {
        (201, mr) => Ok(Delivered { iid: mr["iid"].as_u64().context("MR ohne iid")?, url: text(&mr["web_url"]), commit: commit.to_owned() }),
        (_, answer) => match existing_merge_request(p, branch, commit)? {
            Some(found) => Ok(found),
            None => bail!("Branch {branch} angelegt, aber GitLab lehnt den Merge Request ab: {}", message(&answer)),
        },
    }
}

fn existing_merge_request(p: &Project, branch: &str, commit: &str) -> Result<Option<Delivered>> {
    let list = get(&p.host, &format!("{}/merge_requests?state=all&source_branch={}&per_page=10", p.api(), encode(branch)))?.unwrap_or_default();
    Ok(list
        .as_array()
        .into_iter()
        .flatten()
        .find(|mr| mr["sha"].as_str() == Some(commit))
        .map(|mr| Delivered { iid: mr["iid"].as_u64().unwrap_or(0), url: text(&mr["web_url"]), commit: commit.to_owned() }))
}

/// The merge request an earlier attempt opened from the branch; a branch without one gets one, as
/// long as it carries exactly the approved files on the base commit.
pub fn find_delivery(p: &Project, branch: &str, base_commit: &str, files: &[DocsFile]) -> Result<Option<Delivered>> {
    let Some(head) = branch_head(p, branch)? else { return Ok(None) };
    if let Some(found) = existing_merge_request(p, branch, &head)? {
        return Ok(Some(found));
    }
    let commit = get(&p.host, &format!("{}/repository/commits/{head}", p.api()))?.context("Commit des Branches nicht gefunden")?;
    let parent_ok = commit["parent_ids"].as_array().is_some_and(|ids| ids.len() == 1 && ids[0].as_str() == Some(base_commit));
    let diff = get(&p.host, &format!("{}/repository/commits/{head}/diff", p.api()))?.unwrap_or_default();
    let mut changed: Vec<&str> = diff.as_array().into_iter().flatten().filter_map(|d| d["new_path"].as_str()).collect();
    let mut expected: Vec<&str> = files.iter().map(|f| f.path).collect();
    changed.sort_unstable();
    expected.sort_unstable();
    if !parent_ok || changed != expected {
        return Err(crate::content::Conflict(format!("Branch {branch} gibt es schon mit anderem Inhalt")).into());
    }
    let title = commit["title"].as_str().unwrap_or("Dokumentation aktualisieren").to_owned();
    let base_branch = get(&p.host, &p.api())?.and_then(|i| i["default_branch"].as_str().map(str::to_owned)).unwrap_or_else(|| "main".into());
    open_merge_request(p, branch, &base_branch, &head, &title, "Fortgesetzt von OpenReleasePilot nach einer Unterbrechung.").map(Some)
}

/// State of a delivered merge request.
pub fn merge_request_state(p: &Project, iid: u64) -> Result<crate::content::PrState> {
    let mr = get(&p.host, &format!("{}/merge_requests/{iid}", p.api()))?.with_context(|| format!("MR !{iid} nicht gefunden"))?;
    Ok(match mr["state"].as_str() {
        Some("merged") => crate::content::PrState::Merged,
        Some("closed") => crate::content::PrState::Closed,
        _ => crate::content::PrState::Open,
    })
}

/// Merges a delivered docs merge request once the pipeline of exactly its commit passed; GitLab gets
/// the commit. `Ok(Err(why))`: not merged, the merge request stays open.
pub fn merge_when_green(p: &Project, iid: u64, commit: &str, runner: &Runner, wait: &CiWait, progress: &dyn Fn(&str)) -> Result<Result<String, String>> {
    let ci = crate::repocare::wait_for_checks(&p.id(), commit, runner, wait, progress)?;
    if ci.state != CiState::Passed {
        return Ok(Err(match ci.state {
            CiState::NoChecks => "keine Pipeline gefunden; ohne grüne Pipeline kein automatischer Merge".into(),
            _ => format!("Pipeline nicht grün ({})", ci.detail),
        }));
    }
    let current = merge_request(p, iid)?;
    if current["state"].as_str() != Some("opened") || current["sha"].as_str() != Some(commit) {
        return Ok(Err("der MR hat sich seit der Erstellung verändert".into()));
    }
    if let Some(reason) = blocked_reason(mergeable_state(&current)) {
        return Ok(Err(format!("nicht mergebar: {reason}")));
    }
    match send(&p.host, "PUT", &format!("{}/merge_requests/{iid}/merge", p.api()), Some(&json!({ "sha": commit }))) {
        Ok((200, _)) => Ok(Ok("gemergt".into())),
        Ok((_, answer)) => Ok(Err(format!("GitLab lehnt den Merge ab: {}", message(&answer)))),
        Err(e) => match merge_request_state(p, iid)? {
            crate::content::PrState::Merged => Ok(Ok("gemergt (Antwort verloren, Zustand bei GitLab nachgelesen)".into())),
            _ => Err(e.context("Ergebnis des Merges unklar; laut GitLab ist der MR nicht gemergt")),
        },
    }
}

/// The signed-in GitLab user as commit author: name and the instance's no-reply address.
pub fn commit_identity(host: &str) -> Result<(String, String)> {
    let user = get(host, "user")?.with_context(|| format!("GitLab-Benutzer auf {host} nicht lesbar"))?;
    let login = user["username"].as_str().context("Antwort ohne username")?;
    let id = user["id"].as_u64().context("Antwort ohne id")?;
    let name = user["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(login);
    let email = user["commit_email"]
        .as_str()
        .or(user["public_email"].as_str())
        .filter(|e| !e.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{id}-{login}@users.noreply.{}", host.split(':').next().unwrap_or(host)));
    Ok((name.to_owned(), email))
}

// MARK: Listing

/// Basics of a project, for adding it by name.
pub struct ProjectInfo {
    /// `host/group/name` as GitLab spells it.
    pub id: String,
    pub description: Option<String>,
    pub private: bool,
    pub archived: bool,
}

pub fn project_info(p: &Project) -> Result<ProjectInfo> {
    let info = info(p)?.with_context(|| format!("{} nicht gefunden (oder ohne Zugriff)", p.id()))?;
    Ok(ProjectInfo {
        id: format!("{}/{}", p.host, info["path_with_namespace"].as_str().context("Antwort ohne path_with_namespace")?),
        description: info["description"].as_str().filter(|d| !d.trim().is_empty()).map(str::to_owned),
        private: info["visibility"].as_str() != Some("public"),
        archived: info["archived"].as_bool().unwrap_or(false),
    })
}

/// The hosts `glab` is signed in to, from its configuration file; nothing is started.
pub fn configured_hosts() -> Vec<String> {
    let home = dirs::home_dir().unwrap_or_default();
    let candidates = [
        std::env::var_os("GLAB_CONFIG_DIR").map(std::path::PathBuf::from).map(|d| d.join("config.yml")),
        std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from).map(|d| d.join("glab-cli").join("config.yml")),
        Some(home.join(".config").join("glab-cli").join("config.yml")),
        Some(home.join("Library").join("Application Support").join("glab-cli").join("config.yml")),
    ];
    let Some(text) = candidates.into_iter().flatten().find_map(|f| std::fs::read_to_string(f).ok()) else { return Vec::new() };
    hosts_in_config(&text)
}

/// The keys under `hosts:` of glab's YAML configuration.
pub fn hosts_in_config(text: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    let mut inside = false;
    let mut indent = None;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let depth = line.len() - line.trim_start().len();
        if depth == 0 {
            inside = line.trim_end() == "hosts:";
            indent = None;
            continue;
        }
        if !inside {
            continue;
        }
        let level = *indent.get_or_insert(depth);
        if depth == level {
            if let Some(key) = line.trim().strip_suffix(':') {
                let key = key.trim_matches('"').trim_matches('\'');
                if is_host(key) {
                    hosts.push(key.to_lowercase());
                }
            }
        }
    }
    hosts
}

#[cfg(test)]
pub(crate) mod mock {
    use std::cell::RefCell;

    thread_local! {
        static SIGNED_OUT: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    /// The key a GitLab request has in the GitHub mock: `gitlab <host> <path>`.
    pub fn key(host: &str, path: &str) -> String {
        format!("gitlab {host} {path}")
    }

    pub fn signed_out(host: &str) -> bool {
        SIGNED_OUT.with(|s| s.borrow().iter().any(|h| h == host))
    }

    #[allow(dead_code)]
    pub fn sign_out(host: &str) {
        SIGNED_OUT.with(|s| s.borrow_mut().push(host.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::mock as gh;
    use crate::repocare::Action;

    const HOST: &str = "gitlab.example.com";

    fn at(path: &str) -> String {
        mock::key(HOST, path)
    }

    #[test]
    fn projects_are_recognised_by_their_host() {
        assert_eq!(project("gitlab.example.com/werk/tools/app"), Some(Project { host: HOST.into(), path: "werk/tools/app".into() }));
        assert_eq!(project("Gitlab.Example.com/werk/app").map(|p| p.host), Some(HOST.into()));
        assert_eq!(project("bl0rb/Weave"), None);
        assert_eq!(project("gitlab.example.com/app"), None);
        assert_eq!(project("gitlab.example.com/../app"), None);
        assert_eq!(encode("werk/tools app"), "werk%2Ftools%20app");
        assert!(is_update_bot("renovate-bot") && is_update_bot("Renovate") && !is_update_bot("matze"));
    }

    #[test]
    fn a_pipeline_passes_only_with_success() {
        let state = |list: Value| ci_state("abc", &list).state;
        assert_eq!(state(json!([{ "id": 2, "status": "success" }, { "id": 1, "status": "failed" }])), CiState::Passed, "the newest counts");
        assert_eq!(state(json!([{ "id": 1, "status": "success" }, { "id": 2, "status": "failed" }])), CiState::Failed);
        assert_eq!(state(json!([{ "id": 3, "status": "canceled" }])), CiState::Failed);
        assert_eq!(state(json!([{ "id": 3, "status": "running" }])), CiState::Pending);
        assert_eq!(state(json!([{ "id": 3, "status": "manual" }])), CiState::Unknown, "waiting for a manual job is no result");
        assert_eq!(state(json!([{ "id": 3, "status": "skipped" }])), CiState::Unknown);
        assert_eq!(state(json!([])), CiState::NoChecks);
        assert_eq!(state(json!({})), CiState::Unknown);
    }

    #[test]
    fn merge_status_in_githubs_words() {
        assert_eq!(mergeable_state(&json!({ "detailed_merge_status": "mergeable" })), Some("clean"));
        assert_eq!(mergeable_state(&json!({ "detailed_merge_status": "conflict" })), Some("dirty"));
        assert_eq!(mergeable_state(&json!({ "detailed_merge_status": "need_rebase" })), Some("behind"));
        assert_eq!(mergeable_state(&json!({ "detailed_merge_status": "not_approved" })), Some("blocked"));
        assert_eq!(mergeable_state(&json!({ "detailed_merge_status": "ci_still_running" })), Some("unstable"));
        assert_eq!(mergeable_state(&json!({ "detailed_merge_status": "checking" })), None);
        assert_eq!(mergeable_state(&json!({ "merge_status": "can_be_merged" })), Some("clean"), "older GitLab");
        assert_eq!(mergeable_state(&json!({ "merge_status": "cannot_be_merged", "has_conflicts": true })), Some("dirty"));
        assert_eq!(mergeable_state(&json!({ "merge_status": "unchecked" })), None);
        assert_eq!(blocked_reason(Some("clean")), None);
        assert!(blocked_reason(None).is_some(), "not determined yet is no go");
    }

    #[test]
    fn error_messages_are_readable() {
        assert_eq!(message(&json!({ "message": "405 Method Not Allowed" })), "405 Method Not Allowed");
        assert_eq!(message(&json!({ "message": { "name": ["has already been taken"] } })), "name: has already been taken");
        assert_eq!(message(&json!({ "error": "insufficient_scope" })), "insufficient_scope");
    }

    #[test]
    fn hosts_come_from_glabs_configuration() {
        let config = "git_protocol: ssh\nhosts:\n    gitlab.com:\n        token: x\n        api_host: gitlab.com\n    gitpro.example.de:\n        token: y\n    \"git.example.org:8443\":\n        token: z\neditor: vim\n";
        assert_eq!(hosts_in_config(config), ["gitlab.com", "gitpro.example.de", "git.example.org:8443"]);
        assert!(hosts_in_config("editor: vim\n").is_empty());
    }

    fn project_json() -> Value {
        json!({
            "id": 7, "path_with_namespace": "werk/app", "web_url": "https://gitlab.example.com/werk/app", "visibility": "private",
            "archived": false, "default_branch": "main", "merge_method": "merge",
            "permissions": { "project_access": { "access_level": 40 }, "group_access": null },
        })
    }

    fn target() -> RepoTarget {
        let mut config = crate::config::Config::default();
        config.add_repository(crate::config::ManagedRepo {
            release: Some(crate::config::ReleaseRule { tag_prefix: "v".into(), publishes_release: true }),
            ..crate::config::ManagedRepo::new("gitlab.example.com/werk/app")
        });
        crate::repos::targets(&config, None, None).remove(0)
    }

    #[test]
    fn reads_a_project_like_a_github_repository() {
        let api = "projects/werk%2Fapp";
        gh::install([
            (at(api), Some(project_json())),
            (at(&format!("{api}/repository/branches/main")), Some(json!({ "commit": { "id": "h2" } }))),
            (at(&format!("{api}/pipelines?sha=h2&order_by=id&sort=desc&per_page=20")), Some(json!([{ "id": 9, "status": "success" }]))),
            (
                at(&format!("{api}/merge_requests?state=opened&per_page=100&page=1")),
                Some(json!([
                    { "iid": 4, "title": "Update dependency serde to v1.0.2", "author": { "username": "renovate-bot" }, "target_branch": "main", "web_url": "u", "sha": "m4" },
                    { "iid": 5, "title": "Feature", "author": { "username": "matze" }, "target_branch": "dev", "draft": true, "web_url": "u", "sha": "m5" },
                ])),
            ),
            (at(&format!("{api}/releases?order_by=released_at&sort=desc&per_page=20")), Some(json!([{ "tag_name": "v1.3.0", "upcoming_release": true }, { "tag_name": "v1.2.0" }]))),
            (at(&format!("{api}/repository/commits/v1.2.0")), Some(json!({ "id": "h1" }))),
            (at(&format!("{api}/repository/merge_base?refs[]=h1&refs[]=h2")), Some(json!({ "id": "h1" }))),
            (at(&format!("{api}/repository/compare?from=h1&to=h2")), Some(json!({ "commits": [{ "id": "a" }, { "id": "h2" }] }))),
        ]);
        let p = project("gitlab.example.com/werk/app").unwrap();
        let facts = fetch_facts(&p, &target(), 1000);
        let access = facts.access.ok().unwrap();
        assert_eq!((access.private, access.can_push, access.default_branch.as_str()), (true, Some(true), "main"));
        assert_eq!(access.full_name, "gitlab.example.com/werk/app", "the identity keeps its host");
        assert_eq!(facts.ci.ok().unwrap().state, CiState::Passed);
        let prs = facts.prs.ok().unwrap();
        assert_eq!(prs.dependabot(), 1, "Renovate counts as the update bot");
        assert!(prs.items[1].draft && !prs.items[1].into_default);
        assert_eq!(facts.release.ok().cloned(), Some(Some("v1.2.0".into())), "an upcoming release is not out yet");
        assert_eq!(facts.since.ok().map(|s| s.commits), Some(2));
        assert!(gh::writes().is_empty(), "reading writes nothing");
    }

    #[test]
    fn commits_since_need_the_base_as_ancestor() {
        let api = "projects/werk%2Fapp";
        let p = project("gitlab.example.com/werk/app").unwrap();
        gh::install([(at(&format!("{api}/repository/commits/v1")), Some(json!({ "id": "b" }))), (at(&format!("{api}/repository/merge_base?refs[]=b&refs[]=h")), Some(json!({ "id": "x" })))]);
        assert!(commits_since(&p, "v1", "h").unwrap_err().to_string().contains("divergiert"));
        gh::answer(&at(&format!("{api}/repository/merge_base?refs[]=b&refs[]=h")), Some(json!({ "id": "h" })));
        assert!(commits_since(&p, "v1", "h").unwrap_err().to_string().contains("nach HEAD"));
        assert_eq!(commits_since(&p, "v1", "b").unwrap(), 0);
    }

    fn plan() -> Plan {
        Plan { name: "app".into(), key: "gitlab.example.com/werk/app".into(), repo: "gitlab.example.com/werk/app".into(), default_branch: "main".into(), merge_method: "merge".into(), ..Plan::default() }
    }

    fn approved(iid: u64) -> PullRequest {
        PullRequest { number: iid, head_sha: format!("m{iid}"), base: "main".into(), action: Action::Merge, reviewed: true, risk: Some("green".into()), ready: true, ..PullRequest::default() }
    }

    fn open_mr(iid: u64, status: &str) -> Value {
        json!({ "iid": iid, "state": "opened", "sha": format!("m{iid}"), "target_branch": "main", "detailed_merge_status": status, "author": { "username": "renovate-bot" } })
    }

    #[test]
    fn merges_only_what_still_holds_and_names_the_expected_head() {
        let api = "projects/werk%2Fapp";
        let plan = plan();
        let mut state = RunState::new(&plan);
        gh::install([
            (at(&format!("{api}/merge_requests/4")), Some(open_mr(4, "mergeable"))),
            (at(&format!("{api}/pipelines?sha=m4&order_by=id&sort=desc&per_page=20")), Some(json!([{ "id": 1, "status": "success" }]))),
        ]);
        gh::reply(&format!("PUT {}", at(&format!("{api}/merge_requests/4/merge"))), 200, json!({ "state": "merged", "merge_commit_sha": "mc", "sha": "m4" }));
        assert!(matches!(merge(&plan, &approved(4), &mut state).unwrap(), Done::Ok(_)));
        assert_eq!(gh::written(&format!("PUT {}", at(&format!("{api}/merge_requests/4/merge")))), Some(json!({ "sha": "m4" })), "GitLab gets the expected head");
        assert!(state.merged.contains(&4));
        assert_eq!(state.expected_head.as_deref(), Some("mc"));

        gh::install([(at(&format!("{api}/merge_requests/4")), Some(json!({ "iid": 4, "state": "opened", "sha": "new", "target_branch": "main", "detailed_merge_status": "mergeable" })))]);
        assert!(matches!(merge(&plan, &approved(4), &mut RunState::new(&plan)).unwrap(), Done::Skipped(t) if t.contains("neue Commits")));
        gh::install([
            (at(&format!("{api}/merge_requests/4")), Some(open_mr(4, "mergeable"))),
            (at(&format!("{api}/pipelines?sha=m4&order_by=id&sort=desc&per_page=20")), Some(json!([{ "id": 1, "status": "running" }]))),
        ]);
        assert!(matches!(merge(&plan, &approved(4), &mut RunState::new(&plan)).unwrap(), Done::Skipped(t) if t.contains("Pipeline")));
        assert!(gh::writes().is_empty(), "nothing merged without a green pipeline");
    }

    #[test]
    fn a_conflicting_renovate_request_gets_the_rebase_label() {
        let api = "projects/werk%2Fapp";
        let plan = plan();
        let mut pr = approved(6);
        pr.dependabot = true;
        gh::install([(at(&format!("{api}/merge_requests/6")), Some(open_mr(6, "conflict")))]);
        assert!(matches!(rebase(&plan, &pr).unwrap(), Done::Ok(_)));
        assert_eq!(gh::written(&format!("PUT {}", at(&format!("{api}/merge_requests/6")))), Some(json!({ "add_labels": "rebase" })));
        gh::install([(at(&format!("{api}/merge_requests/6")), Some(open_mr(6, "mergeable")))]);
        assert!(matches!(rebase(&plan, &pr).unwrap(), Done::Skipped(t) if t.contains("kein Konflikt")));
    }

    #[test]
    fn branches_go_only_while_nothing_needs_them() {
        let api = "projects/werk%2Fapp";
        let plan = plan();
        let state = RunState::new(&plan);
        let branch = Branch { name: "feature/x".into(), sha: "f1".into(), reason: String::new(), proof: Proof::MergedPr(3) };
        let world = |open_from: Value| {
            vec![
                (at(&format!("{api}/repository/branches/feature%2Fx")), Some(json!({ "commit": { "id": "f1" }, "protected": false }))),
                (at(api), Some(project_json())),
                (at(&format!("{api}/merge_requests?state=opened&source_branch=feature%2Fx&per_page=100&page=1")), Some(open_from)),
                (at(&format!("{api}/merge_requests?state=opened&target_branch=feature%2Fx&per_page=100&page=1")), Some(json!([]))),
                (at(&format!("{api}/merge_requests/3")), Some(json!({ "state": "merged", "sha": "f1" }))),
            ]
        };
        gh::install(world(json!([{ "iid": 8 }])));
        assert!(matches!(delete_branch(&plan, &branch, &state).unwrap(), Done::Skipped(t) if t.contains("!8")));
        gh::install(world(json!([])));
        assert!(matches!(delete_branch(&plan, &branch, &state).unwrap(), Done::Ok(_)));
        assert_eq!(gh::writes(), [format!("DELETE {}", at(&format!("{api}/repository/branches/feature%2Fx")))]);
        let mut gone = world(json!([]));
        gone[0].1 = None;
        gh::install(gone);
        assert!(matches!(delete_branch(&plan, &branch, &state).unwrap(), Done::Nothing(_)), "already gone is reached");
    }

    #[test]
    fn a_docs_proposal_becomes_one_commit_and_a_merge_request() {
        let api = "projects/werk%2Fapp";
        let p = project("gitlab.example.com/werk/app").unwrap();
        gh::install([(at(&format!("{api}/repository/branches/orp%2Freadme")), None)]);
        gh::reply(&format!("POST {}", at(&format!("{api}/repository/commits"))), 201, json!({ "id": "c1" }));
        gh::reply(&format!("POST {}", at(&format!("{api}/merge_requests"))), 201, json!({ "iid": 12, "web_url": "https://gitlab.example.com/werk/app/-/merge_requests/12" }));
        let files = [DocsFile { path: "README.md", added: false, executable: false, data: b"neu\n" }];
        let delivered = publish_docs(&p, "orp/readme", "main", "base", &files, "README", "Text").unwrap();
        assert_eq!((delivered.iid, delivered.commit.as_str()), (12, "c1"));
        let commit = gh::written(&format!("POST {}", at(&format!("{api}/repository/commits")))).unwrap();
        assert_eq!((commit["branch"].as_str(), commit["start_sha"].as_str()), (Some("orp/readme"), Some("base")), "a new branch from exactly the base commit");
        assert_eq!(commit["actions"][0]["action"], "update");
        assert_eq!(gh::written(&format!("POST {}", at(&format!("{api}/merge_requests")))).unwrap()["target_branch"], "main");
    }

    #[test]
    fn writes_need_a_login() {
        mock::sign_out("other.example.com");
        assert!(send("other.example.com", "POST", "projects/x", None).is_err(), "a missing login stops writes");
        assert_eq!(token("other.example.com"), None);
    }
}
