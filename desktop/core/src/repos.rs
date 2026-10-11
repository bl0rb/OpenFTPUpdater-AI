//! The Repositories area: the repositories of the site's products, what GitHub reports about them
//! (read only, without Claude) and what follows from it: a health state with its reasons, and the
//! care actions that are possible.
//!
//! A repository is identified by `owner/name`, case-insensitive; several products may share one and
//! keep their own documented version and tag prefix. Facts carry when they were read and which part
//! failed, so "0 PRs", "not asked" and "query failed" stay apart, and nothing old or incomplete ever
//! reads as healthy. Nothing in this module writes: no GitHub write, no git command.

use crate::github::{encode_ref, get, get_list, is_newer, Forbidden};
use crate::localrepo;
use crate::repocare::latest_release_tag;
use crate::site::Site;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// How long facts count as current in the view. Never replaces the checks before a change.
pub const FRESH_SECS: u64 = 300;
/// Pull requests whose mergeability and CI the detail view reads.
pub const DETAIL_LIMIT: usize = 30;

// MARK: Identity

/// One product's view of its repository.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductLink {
    pub id: String,
    pub name: String,
    pub documented: String,
    pub tag_prefix: String,
    pub local_path: Option<String>,
    /// Selected for Precheck and Update; the Repositories area shows it either way.
    pub enabled: bool,
}

/// The local clone named by the products' `localPath`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum LocalCopy {
    /// No product names a `localPath`.
    NotConfigured,
    Found { path: String },
    Missing { path: String },
    /// The folder is not a clone of this repository.
    Foreign { path: String, origin: Option<String> },
    /// The products name different folders.
    Conflict { paths: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoTarget {
    /// `owner/name` in lower case: the identity.
    pub key: String,
    /// `owner/name` as the first product writes it.
    pub repo: String,
    pub products: Vec<ProductLink>,
    /// `None` when the products disagree; `tag_conflict` says how.
    pub tag_prefix: Option<String>,
    pub tag_conflict: Option<String>,
    pub local: LocalCopy,
    /// It is also the website's own repository.
    pub website: bool,
}

impl RepoTarget {
    /// At least one of its products is selected.
    pub fn selected(&self) -> bool {
        self.products.iter().any(|p| p.enabled)
    }

    /// The product names, e.g. `App, App Pro`.
    pub fn name(&self) -> String {
        self.products.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
    }

    /// The clone, only if it exists and belongs to this repository.
    pub fn clone_dir(&self) -> Option<PathBuf> {
        match &self.local {
            LocalCopy::Found { path } => Some(PathBuf::from(path)),
            _ => None,
        }
    }

    /// What a preview depends on: a preview is not run once this changed.
    pub fn fingerprint(&self) -> String {
        format!("{}|{:?}|{:?}", self.key, self.tag_prefix, self.local)
    }
}

/// The repositories of all products in site.json, each once, in the order of their first product.
/// Turned-off products stay in: their selection only limits Precheck and Update.
pub fn targets(site: &Site, project: &Path, skipped: &BTreeSet<String>, website: Option<&str>) -> Vec<RepoTarget> {
    let mut targets: Vec<RepoTarget> = Vec::new();
    for product in &site.products {
        let repo = product.repo.trim();
        if repo.is_empty() {
            continue;
        }
        let link = ProductLink {
            id: product.id.clone(),
            name: product.name.clone(),
            documented: product.documented.clone(),
            tag_prefix: product.tag_prefix.clone(),
            local_path: product.local_path.clone().filter(|p| !p.trim().is_empty()),
            enabled: !skipped.contains(&product.id),
        };
        let key = repo.to_lowercase();
        match targets.iter_mut().find(|t| t.key == key) {
            Some(target) => target.products.push(link),
            None => targets.push(RepoTarget {
                website: website.is_some_and(|w| w.eq_ignore_ascii_case(repo)),
                key,
                repo: repo.to_owned(),
                products: vec![link],
                tag_prefix: None,
                tag_conflict: None,
                local: LocalCopy::NotConfigured,
            }),
        }
    }
    for target in &mut targets {
        let prefixes: BTreeSet<&str> = target.products.iter().map(|p| p.tag_prefix.as_str()).collect();
        if prefixes.len() == 1 {
            target.tag_prefix = prefixes.into_iter().next().map(str::to_owned);
        } else {
            let each: Vec<String> = target.products.iter().map(|p| format!("„{}“ ({})", p.tag_prefix, p.name)).collect();
            target.tag_conflict = Some(format!("widersprüchliche tagPrefix: {}", each.join(", ")));
        }
        target.local = local_copy(project, &target.repo, &target.products);
    }
    targets
}

fn local_copy(project: &Path, repo: &str, products: &[ProductLink]) -> LocalCopy {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for path in products.iter().filter_map(|p| p.local_path.as_deref()) {
        let dir = project.join(path.trim());
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    match dirs.as_slice() {
        [] => LocalCopy::NotConfigured,
        [dir] => {
            let path = dir.display().to_string();
            if !dir.is_dir() {
                return LocalCopy::Missing { path };
            }
            match localrepo::origin_repo(dir) {
                Some(origin) if origin.eq_ignore_ascii_case(repo) => LocalCopy::Found { path },
                origin => LocalCopy::Foreign { path, origin },
            }
        }
        _ => LocalCopy::Conflict { paths: dirs.iter().map(|d| d.display().to_string()).collect() },
    }
}

// MARK: Facts

/// One part of the facts: read, failed, or not asked (with why).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "camelCase")]
pub enum Fetch<T> {
    NotQueried(String),
    Ok(T),
    Failed(String),
}

impl<T> Fetch<T> {
    pub fn ok(&self) -> Option<&T> {
        match self {
            Fetch::Ok(value) => Some(value),
            _ => None,
        }
    }

    fn from(result: Result<T>) -> Self {
        match result {
            Ok(value) => Fetch::Ok(value),
            Err(e) => Fetch::Failed(format!("{e:#}")),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Access {
    pub full_name: String,
    pub url: String,
    pub private: bool,
    pub archived: bool,
    /// `None` without a gh login: GitHub only reports permissions to a signed-in user.
    pub can_push: Option<bool>,
    pub default_branch: String,
    /// GitHub deletes head branches after a merge itself.
    pub deletes_merged: bool,
}

impl Access {
    fn from_json(json: &Value) -> Result<Self> {
        Ok(Self {
            full_name: json["full_name"].as_str().context("Antwort ohne full_name")?.to_owned(),
            url: json["html_url"].as_str().unwrap_or_default().to_owned(),
            private: json["private"].as_bool().unwrap_or(false),
            archived: json["archived"].as_bool().unwrap_or(false),
            can_push: json["permissions"]["push"].as_bool(),
            default_branch: json["default_branch"].as_str().filter(|b| !b.is_empty()).context("Antwort ohne default_branch")?.to_owned(),
            deletes_merged: json["delete_branch_on_merge"].as_bool() == Some(true),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CiState {
    Passed,
    Failed,
    /// Checks still queued or running.
    Pending,
    /// No check run and no commit status found; not proof that the repository has no CI.
    NoChecks,
    /// Finished with a result the app does not know, or not all checks could be read.
    Unknown,
}

/// The CI of one commit: check runs and commit statuses together.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ci {
    pub state: CiState,
    pub sha: String,
    pub detail: String,
}

/// The CI of `sha`. Errors only when GitHub cannot be asked; an unclear answer is `Unknown`.
pub fn ci_for(repo: &str, sha: &str) -> Result<Ci> {
    let runs = get(&format!("repos/{repo}/commits/{sha}/check-runs?per_page=100"))?.with_context(|| format!("Commit {} nicht gefunden", short(sha)))?;
    let status = get(&format!("repos/{repo}/commits/{sha}/status?per_page=100"))?.with_context(|| format!("Commit {} nicht gefunden", short(sha)))?;
    Ok(ci_state(sha, &runs, &status))
}

/// Check runs (`commits/{sha}/check-runs`) and commit statuses (`commits/{sha}/status`) to one state.
/// Only known successful results count as passed; anything unread or unknown keeps it from passing.
pub fn ci_state(sha: &str, runs: &Value, status: &Value) -> Ci {
    let ci = |state, detail: String| Ci { state, sha: sha.to_owned(), detail };
    let (Some(run_list), Some(statuses)) = (runs["check_runs"].as_array(), status["statuses"].as_array()) else {
        return ci(CiState::Unknown, "Antwort von GitHub unvollständig".into());
    };
    let truncated = runs["total_count"].as_u64().is_some_and(|n| n > run_list.len() as u64)
        || status["total_count"].as_u64().is_some_and(|n| n > statuses.len() as u64);
    let (mut passed, mut failed, mut pending, mut unknown) = (0, Vec::new(), Vec::new(), Vec::new());
    for run in run_list {
        let name = run["name"].as_str().unwrap_or("?").to_owned();
        if run["status"].as_str() != Some("completed") {
            pending.push(name);
            continue;
        }
        match run["conclusion"].as_str() {
            Some("success" | "neutral" | "skipped") => passed += 1,
            Some("failure" | "cancelled" | "timed_out" | "action_required" | "startup_failure") => failed.push(name),
            other => unknown.push(format!("{name} ({})", other.unwrap_or("ohne Ergebnis"))),
        }
    }
    for status in statuses {
        let name = status["context"].as_str().unwrap_or("?").to_owned();
        match status["state"].as_str() {
            Some("success") => passed += 1,
            Some("pending") => pending.push(name),
            Some("failure" | "error") => failed.push(name),
            other => unknown.push(format!("{name} ({})", other.unwrap_or("ohne Ergebnis"))),
        }
    }
    if !failed.is_empty() {
        ci(CiState::Failed, format!("fehlgeschlagen: {}", failed.join(", ")))
    } else if truncated {
        ci(CiState::Unknown, "mehr Checks, als die App liest; Ergebnis nicht vollständig".into())
    } else if !unknown.is_empty() {
        ci(CiState::Unknown, format!("Ergebnis unklar: {}", unknown.join(", ")))
    } else if !pending.is_empty() {
        ci(CiState::Pending, format!("läuft noch: {}", pending.join(", ")))
    } else if passed == 0 {
        ci(CiState::NoChecks, "keine Checks gefunden".into())
    } else {
        ci(CiState::Passed, format!("{passed} Check{} erfolgreich", if passed == 1 { "" } else { "s" }))
    }
}

/// An open pull request as the list endpoint reports it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrBrief {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub dependabot: bool,
    pub draft: bool,
    /// Target branch.
    pub base: String,
    pub into_default: bool,
    pub url: String,
    pub head_sha: String,
}

/// All open pull requests, not only those care looks at (into the default branch).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrList {
    pub items: Vec<PrBrief>,
    /// False when there were more than the app reads; counts are then "at least".
    pub complete: bool,
}

impl PrList {
    pub fn dependabot(&self) -> usize {
        self.items.iter().filter(|pr| pr.dependabot).count()
    }
}

/// Commits on the default branch since a release or tag.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Since {
    /// `release` or `tag`.
    pub base_kind: &'static str,
    pub base: String,
    pub commits: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Facts {
    /// Unix seconds.
    pub checked_at: u64,
    pub access: Fetch<Access>,
    /// GitHub answered 404 or refused access: the repository cannot be read with this login.
    pub unreadable: bool,
    /// Head commit of the default branch, the commit everything else refers to.
    pub head: Fetch<String>,
    pub ci: Fetch<Ci>,
    pub prs: Fetch<PrList>,
    /// Newest published release (no draft, no pre-release); `None` if there is none.
    pub release: Fetch<Option<String>>,
    /// Highest stable tag with the products' prefix; only asked when there is no release.
    pub tag: Fetch<Option<String>>,
    pub since: Fetch<Since>,
}

impl Facts {
    fn new(checked_at: u64, access: Fetch<Access>, unreadable: bool) -> Self {
        fn skipped<T>() -> Fetch<T> {
            Fetch::NotQueried("Repository nicht gelesen".into())
        }
        Self { checked_at, access, unreadable, head: skipped(), ci: skipped(), prs: skipped(), release: skipped(), tag: skipped(), since: skipped() }
    }

    /// Nothing could be read, not even the repository; an older reading is worth more.
    pub fn failed_entirely(&self) -> bool {
        matches!(self.access, Fetch::Failed(_)) && !self.unreadable
    }
}

/// Reads what the overview needs, about eight GitHub requests. Each part fails on its own.
pub fn fetch_facts(target: &RepoTarget, now: u64) -> Facts {
    let repo = &target.repo;
    let info = match get(&format!("repos/{repo}")) {
        Ok(Some(info)) => info,
        Ok(None) => {
            let why = "GitHub meldet 404: gelöscht, ohne Weiterleitung umbenannt oder privat ohne Zugriff";
            return Facts::new(now, Fetch::Failed(why.into()), true);
        }
        Err(e) if e.is::<Forbidden>() => return Facts::new(now, Fetch::Failed(format!("{e:#}")), true),
        Err(e) => return Facts::new(now, Fetch::Failed(format!("{e:#}")), false),
    };
    let access = match Access::from_json(&info) {
        Ok(access) => access,
        Err(e) => return Facts::new(now, Fetch::Failed(format!("{e:#}")), false),
    };
    let mut facts = Facts::new(now, Fetch::Ok(access.clone()), false);
    facts.head = Fetch::from(head_of(repo, &access.default_branch));
    facts.ci = match facts.head.ok() {
        Some(sha) => Fetch::from(ci_for(repo, sha)),
        None => Fetch::NotQueried(format!("HEAD von {} unbekannt", access.default_branch)),
    };
    facts.prs = Fetch::from(open_prs(repo, &access.default_branch));
    facts.release = Fetch::from(crate::github::latest_release(repo));
    facts.tag = match (&facts.release, &target.tag_prefix) {
        (Fetch::Ok(Some(_)), _) => Fetch::NotQueried("es gibt ein veröffentlichtes Release".into()),
        (Fetch::Ok(None), Some(prefix)) => Fetch::from(stable_tag(repo, prefix)),
        (Fetch::Ok(None), None) => Fetch::NotQueried(target.tag_conflict.clone().unwrap_or_default()),
        _ => Fetch::NotQueried("Release-Abfrage fehlgeschlagen".into()),
    };
    let base = match (&facts.release, &facts.tag) {
        (Fetch::Ok(Some(release)), _) => Some(("release", release.clone())),
        (_, Fetch::Ok(Some(tag))) => Some(("tag", tag.clone())),
        _ => None,
    };
    facts.since = match (base, facts.head.ok()) {
        (Some((kind, base)), Some(head)) => Fetch::from(commits_since(repo, &base, head).map(|commits| Since { base_kind: kind, base, commits })),
        (None, _) => Fetch::NotQueried("kein veröffentlichtes Release und kein passender Tag".into()),
        (_, None) => Fetch::NotQueried("HEAD unbekannt".into()),
    };
    facts
}

fn head_of(repo: &str, branch: &str) -> Result<String> {
    let json = get(&format!("repos/{repo}/branches/{}", encode_ref(branch)))?.with_context(|| format!("Branch {branch} nicht gefunden"))?;
    Ok(json["commit"]["sha"].as_str().context("Branch ohne Commit")?.to_owned())
}

fn open_prs(repo: &str, default_branch: &str) -> Result<PrList> {
    let list = get_list(&format!("repos/{repo}/pulls?state=open"), 3)?;
    let items = list
        .items
        .iter()
        .map(|pr| {
            let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
            let author = text(&pr["user"]["login"]);
            let base = text(&pr["base"]["ref"]);
            PrBrief {
                number: pr["number"].as_u64().unwrap_or(0),
                title: text(&pr["title"]),
                dependabot: author.starts_with("dependabot"),
                author,
                draft: pr["draft"].as_bool() == Some(true),
                into_default: base == default_branch,
                base,
                url: text(&pr["html_url"]),
                head_sha: text(&pr["head"]["sha"]),
            }
        })
        .collect();
    Ok(PrList { items, complete: list.complete })
}

/// The highest stable tag with the prefix; an error when the tag list was cut off.
fn stable_tag(repo: &str, prefix: &str) -> Result<Option<String>> {
    let list = get_list(&format!("repos/{repo}/tags"), 3)?;
    if !list.complete {
        anyhow::bail!("über 300 Tags; der höchste passende ist nicht sicher bestimmbar");
    }
    let names: Vec<String> = list.items.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect();
    Ok(latest_release_tag(&names, prefix))
}

/// Commits in `head` (a commit) since `base` (a tag), only where `base` is an ancestor of `head`.
pub fn commits_since(repo: &str, base: &str, head: &str) -> Result<u64> {
    let compare = get(&format!("repos/{repo}/compare/{}...{}", encode_ref(base), encode_ref(head)))?.with_context(|| format!("{base} nicht gefunden"))?;
    since_from_compare(&compare).map_err(|why| anyhow::anyhow!("nicht bestimmbar: {why}"))
}

/// The number from a compare answer; never a guessed 0 for a history that diverged.
pub fn since_from_compare(compare: &Value) -> Result<u64, String> {
    match compare["status"].as_str() {
        Some("identical") => Ok(0),
        Some("ahead") => compare["ahead_by"].as_u64().ok_or_else(|| "Antwort ohne ahead_by".to_owned()),
        Some("diverged") => Err("Historie divergiert, die Basis ist kein Vorfahr von HEAD".into()),
        Some("behind") => Err("die Basis liegt nach HEAD".into()),
        _ => Err("Antwort ohne Vergleichsstatus".into()),
    }
}

// MARK: Details

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrDetail {
    pub number: u64,
    /// GitHub's `mergeable_state`; `None` while GitHub has not computed it or could not be asked.
    pub mergeable_state: Option<String>,
    pub mergeable: String,
    pub ci: Fetch<Ci>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Details {
    pub fetched_at: u64,
    pub prs: Vec<PrDetail>,
    /// Every open pull request was read (there are at most `DETAIL_LIMIT`).
    pub complete: bool,
}

/// Mergeability and CI of the open pull requests, for the detail view; read only.
pub fn fetch_details(repo: &str, prs: &PrList, now: u64) -> Details {
    let details = prs
        .items
        .iter()
        .take(DETAIL_LIMIT)
        .map(|pr| {
            let (state, mergeable) = match get(&format!("repos/{repo}/pulls/{}", pr.number)) {
                Ok(Some(json)) if json["mergeable"].is_null() => (None, "von GitHub noch nicht bestimmt".to_owned()),
                Ok(Some(json)) => {
                    let state = json["mergeable_state"].as_str().unwrap_or("unknown").to_owned();
                    let text = mergeable_text(&state).to_owned();
                    (Some(state), text)
                }
                Ok(None) => (None, "nicht mehr gefunden".to_owned()),
                Err(e) => (None, format!("nicht abfragbar: {e:#}")),
            };
            PrDetail { number: pr.number, mergeable_state: state, mergeable, ci: Fetch::from(ci_for(repo, &pr.head_sha)) }
        })
        .collect();
    Details { fetched_at: now, prs: details, complete: prs.complete && prs.items.len() <= DETAIL_LIMIT }
}

pub fn mergeable_text(state: &str) -> &'static str {
    match state {
        "clean" | "has_hooks" => "mergebar",
        "unstable" => "mergebar, aber CI nicht grün",
        "blocked" => "durch Schutzregeln blockiert",
        "behind" => "hinter dem Zielbranch",
        "dirty" => "Merge-Konflikt",
        "draft" => "Entwurf",
        _ => "von GitHub noch nicht bestimmt",
    }
}

// MARK: Health

/// In this order: a blocker outranks what needs attention, which outranks missing facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HealthState {
    Blocked,
    NeedsAttention,
    Unknown,
    Healthy,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub state: HealthState,
    /// Blockers, then what needs attention; the first is the main reason.
    pub reasons: Vec<String>,
    /// Facts that are missing, incomplete or old; without a reason they make the state unknown.
    pub gaps: Vec<String>,
    /// Hints that do not change the state: optional data, local clone, read-only access.
    pub notes: Vec<String>,
    /// The facts are older than `FRESH_SECS`.
    pub stale: bool,
}

/// The health of one repository from its facts (and the detail view's pull request data, if read).
pub fn health(target: &RepoTarget, facts: Option<&Facts>, details: Option<&Details>, now: u64) -> Health {
    let (mut blockers, mut attention, mut gaps, mut notes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    match &target.local {
        LocalCopy::NotConfigured => notes.push("keine lokale Kopie eingetragen (localPath)".to_owned()),
        LocalCopy::Found { .. } => {}
        LocalCopy::Missing { path } => notes.push(format!("lokale Kopie fehlt: {path}")),
        LocalCopy::Foreign { path, origin } => {
            notes.push(format!("{path} ist keine Kopie dieses Repositorys ({})", origin.as_deref().unwrap_or("ohne GitHub-origin")))
        }
        LocalCopy::Conflict { .. } => notes.push("Produkte nennen verschiedene lokale Kopien".to_owned()),
    }
    if let Some(conflict) = &target.tag_conflict {
        notes.push(conflict.clone());
    }
    let Some(facts) = facts else {
        gaps.insert(0, "noch nicht geprüft".to_owned());
        return Health { state: HealthState::Unknown, reasons: Vec::new(), gaps, notes, stale: false };
    };
    let stale = now.saturating_sub(facts.checked_at) > FRESH_SECS;
    let default = facts.access.ok().map(|a| a.default_branch.clone()).unwrap_or_else(|| "Default-Branch".into());
    match &facts.access {
        Fetch::Failed(why) if facts.unreadable => blockers.push(format!("nicht lesbar: {why}")),
        Fetch::Failed(why) => gaps.push(format!("GitHub-Abfrage fehlgeschlagen: {why}")),
        Fetch::NotQueried(why) => gaps.push(why.clone()),
        Fetch::Ok(access) => {
            if access.archived {
                notes.push("archiviert: nur lesbar".to_owned());
            } else if access.can_push == Some(false) {
                notes.push("nur lesbar: kein Schreibzugriff".to_owned());
            }
        }
    }
    if facts.access.ok().is_some() {
        match &facts.ci {
            Fetch::Ok(ci) => match ci.state {
                CiState::Failed => blockers.push(format!("CI auf {default} {}", ci.detail)),
                CiState::Passed => {}
                CiState::Pending => gaps.push(format!("CI auf {default} läuft noch")),
                CiState::NoChecks => gaps.push(format!("keine CI-Checks auf {default} gefunden")),
                CiState::Unknown => gaps.push(format!("CI auf {default}: {}", ci.detail)),
            },
            Fetch::Failed(why) => gaps.push(format!("CI nicht abfragbar: {why}")),
            Fetch::NotQueried(why) => gaps.push(format!("CI nicht abgefragt: {why}")),
        }
        match &facts.prs {
            Fetch::Ok(list) => {
                let at_least = if list.complete { "" } else { "mindestens " };
                match list.dependabot() {
                    0 => {}
                    1 => attention.push(format!("{at_least}1 Dependabot-PR kann geprüft werden")),
                    n => attention.push(format!("{at_least}{n} Dependabot-PRs können geprüft werden")),
                }
                if !list.complete {
                    gaps.push("mehr als 300 offene PRs; Liste unvollständig".to_owned());
                }
            }
            Fetch::Failed(why) => gaps.push(format!("PRs nicht abfragbar: {why}")),
            Fetch::NotQueried(why) => gaps.push(format!("PRs nicht abgefragt: {why}")),
        }
        match &facts.release {
            Fetch::Ok(Some(tag)) => {
                for product in &target.products {
                    if is_newer(tag, &product.documented) {
                        let text = format!("Website beschreibt {} {}, neuestes Release {tag}", product.name, product.documented);
                        if product.enabled {
                            attention.push(text);
                        } else {
                            notes.push(format!("{text} (Produkt nicht ausgewählt)"));
                        }
                    }
                }
            }
            Fetch::Ok(None) => {}
            Fetch::Failed(why) => notes.push(format!("Release nicht abfragbar: {why}")),
            Fetch::NotQueried(_) => {}
        }
        match &facts.since {
            Fetch::Ok(since) if since.commits > 0 => {
                let what = if since.base_kind == "release" { "Release" } else { "Tag" };
                attention.push(format!("{} Commit{} seit {what} {}", since.commits, if since.commits == 1 { "" } else { "s" }, since.base));
            }
            Fetch::Ok(_) => {}
            Fetch::Failed(why) => notes.push(format!("Commits seit Release {why}")),
            Fetch::NotQueried(_) => {}
        }
    }
    if let Some(details) = details {
        let conflicts: Vec<String> = details.prs.iter().filter(|d| d.mergeable_state.as_deref() == Some("dirty")).map(|d| format!("#{}", d.number)).collect();
        if !conflicts.is_empty() {
            attention.push(format!("Merge-Konflikt in {}", conflicts.join(", ")));
        }
        let red: Vec<String> = details.prs.iter().filter(|d| d.ci.ok().is_some_and(|ci| ci.state == CiState::Failed)).map(|d| format!("#{}", d.number)).collect();
        if !red.is_empty() {
            attention.push(format!("PR-CI fehlgeschlagen in {}", red.join(", ")));
        }
    }
    if stale {
        // Last, so a gap with content stays the main reason.
        gaps.push(format!("Stand älter als {} Minuten", FRESH_SECS / 60));
    }
    let state = if !blockers.is_empty() {
        HealthState::Blocked
    } else if !attention.is_empty() {
        HealthState::NeedsAttention
    } else if !gaps.is_empty() {
        HealthState::Unknown
    } else {
        HealthState::Healthy
    };
    blockers.extend(attention);
    Health { state, reasons: blockers, gaps, notes, stale }
}

// MARK: Capabilities

/// Which care actions are possible; each `Some` says why not.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// Preview and run.
    pub care: Option<String>,
    pub tag: Option<String>,
    /// Local branches.
    pub local: Option<String>,
}

/// `signed_in`: whether gh is signed in, `None` while unknown (the care check asks again).
pub fn capabilities(target: &RepoTarget, facts: Option<&Facts>, signed_in: Option<bool>) -> Capabilities {
    let care = if signed_in == Some(false) {
        Some("GitHub-CLI nicht angemeldet (gh auth login)".to_owned())
    } else {
        match facts {
            Some(facts) if facts.unreadable => Some("Repository nicht lesbar".to_owned()),
            Some(facts) => match facts.access.ok() {
                Some(access) if access.archived => Some("archiviert".to_owned()),
                Some(access) if access.can_push == Some(false) => Some("nur lesbar: kein Schreibzugriff".to_owned()),
                _ => None,
            },
            None => None,
        }
    };
    let local = match &target.local {
        LocalCopy::NotConfigured => Some("keine lokale Kopie eingetragen".to_owned()),
        other => local_problem(other),
    };
    Capabilities { care, tag: target.tag_conflict.clone(), local }
}

/// Why a configured local clone cannot be used; `None` when it can (or none is configured).
pub fn local_problem(local: &LocalCopy) -> Option<String> {
    match local {
        LocalCopy::Found { .. } | LocalCopy::NotConfigured => None,
        LocalCopy::Missing { path } => Some(format!("lokale Kopie fehlt: {path}")),
        LocalCopy::Foreign { path, .. } => Some(format!("{path} gehört nicht zu diesem Repository")),
        LocalCopy::Conflict { paths } => Some(format!("widersprüchliche lokale Kopien: {}", paths.join(", "))),
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::mock;
    use serde_json::json;

    fn site(products: Value) -> Site {
        serde_json::from_value(json!({ "name": "s", "url": "https://s", "build": "b", "output": "dist", "products": products })).unwrap()
    }

    fn product(id: &str, repo: &str, prefix: &str) -> Value {
        json!({ "id": id, "name": id.to_uppercase(), "repo": repo, "tagPrefix": prefix, "documented": "1.0.0" })
    }

    fn target(repo: &str) -> RepoTarget {
        targets(&site(json!([product("a", repo, "")])), Path::new("/nowhere"), &BTreeSet::new(), None).remove(0)
    }

    #[test]
    fn one_repository_per_identity_with_all_its_products() {
        let site = site(json!([product("a", "Owner/App", "v"), product("b", "owner/app", "v"), product("c", "owner/other", "")]));
        let skipped = BTreeSet::from(["b".to_owned(), "c".to_owned()]);
        let targets = targets(&site, Path::new("/nowhere"), &skipped, Some("OWNER/OTHER"));
        assert_eq!(targets.len(), 2, "case does not make a second repository");
        assert_eq!(targets[0].repo, "Owner/App");
        assert_eq!(targets[0].products.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert!(targets[0].selected());
        assert!(!targets[1].selected(), "turned off, but still listed");
        assert!(targets[1].website);
        assert_eq!(targets[0].tag_prefix.as_deref(), Some("v"));
    }

    #[test]
    fn conflicting_configuration_is_not_guessed() {
        let mut a = product("a", "o/app", "v");
        a["localPath"] = json!("../one");
        let mut b = product("b", "o/app", "");
        b["localPath"] = json!("../two");
        let target = targets(&site(json!([a, b])), Path::new("/nowhere/site"), &BTreeSet::new(), None).remove(0);
        assert_eq!(target.tag_prefix, None);
        assert!(target.tag_conflict.as_deref().unwrap().contains("„v“ (A)"));
        assert!(matches!(target.local, LocalCopy::Conflict { .. }));
        assert_eq!(target.clone_dir(), None);
        let caps = capabilities(&target, None, Some(true));
        assert!(caps.tag.is_some() && caps.local.is_some() && caps.care.is_none());
    }

    fn runs(list: Value) -> Value {
        let n = list.as_array().unwrap().len();
        json!({ "total_count": n, "check_runs": list })
    }

    fn statuses(list: Value) -> Value {
        let n = list.as_array().unwrap().len();
        json!({ "total_count": n, "statuses": list })
    }

    #[test]
    fn ci_states_stay_apart() {
        let done = |c: &str| json!({ "name": c, "status": "completed", "conclusion": c });
        let state = |r: Value, s: Value| ci_state("abc", &r, &s).state;
        assert_eq!(state(runs(json!([done("success"), done("skipped")])), statuses(json!([]))), CiState::Passed);
        assert_eq!(state(runs(json!([done("success"), done("failure")])), statuses(json!([]))), CiState::Failed);
        assert_eq!(state(runs(json!([])), statuses(json!([{ "context": "ci", "state": "error" }]))), CiState::Failed);
        assert_eq!(state(runs(json!([{ "name": "b", "status": "in_progress" }])), statuses(json!([]))), CiState::Pending);
        assert_eq!(state(runs(json!([])), statuses(json!([{ "context": "ci", "state": "pending" }]))), CiState::Pending);
        assert_eq!(state(runs(json!([])), statuses(json!([]))), CiState::NoChecks);
        assert_eq!(state(runs(json!([done("stale")])), statuses(json!([]))), CiState::Unknown, "finished but unknown is no success");
        assert_eq!(state(json!({ "total_count": 120, "check_runs": [done("success")] }), statuses(json!([]))), CiState::Unknown, "unread checks");
        assert_eq!(state(json!({}), statuses(json!([]))), CiState::Unknown);
    }

    #[test]
    fn commits_since_are_never_guessed() {
        assert_eq!(since_from_compare(&json!({ "status": "identical", "ahead_by": 0 })), Ok(0));
        assert_eq!(since_from_compare(&json!({ "status": "ahead", "ahead_by": 4 })), Ok(4));
        assert!(since_from_compare(&json!({ "status": "diverged", "ahead_by": 4 })).is_err());
        assert!(since_from_compare(&json!({ "status": "behind", "ahead_by": 0 })).is_err());
        assert!(since_from_compare(&json!({ "status": "ahead" })).is_err());
        assert!(since_from_compare(&json!({})).is_err());
    }

    /// Facts of a clean repository, as `fetch_facts` would read them.
    fn clean(now: u64) -> Facts {
        let access = Access {
            full_name: "o/app".into(),
            url: String::new(),
            private: false,
            archived: false,
            can_push: Some(true),
            default_branch: "main".into(),
            deletes_merged: false,
        };
        let mut facts = Facts::new(now, Fetch::Ok(access), false);
        facts.head = Fetch::Ok("abc".into());
        facts.ci = Fetch::Ok(Ci { state: CiState::Passed, sha: "abc".into(), detail: String::new() });
        facts.prs = Fetch::Ok(PrList { items: Vec::new(), complete: true });
        facts.release = Fetch::Ok(Some("1.0.0".into()));
        facts.tag = Fetch::NotQueried(String::new());
        facts.since = Fetch::Ok(Since { base_kind: "release", base: "1.0.0".into(), commits: 0 });
        facts
    }

    fn pr(number: u64, dependabot: bool) -> PrBrief {
        PrBrief {
            number,
            title: String::new(),
            author: if dependabot { "dependabot[bot]".into() } else { "me".into() },
            dependabot,
            draft: false,
            base: "main".into(),
            into_default: true,
            url: String::new(),
            head_sha: format!("h{number}"),
        }
    }

    #[test]
    fn health_follows_the_rules() {
        let target = target("o/app");
        let state = |facts: &Facts| health(&target, Some(facts), None, 1000).state;
        assert_eq!(state(&clean(1000)), HealthState::Healthy);
        assert_eq!(health(&target, None, None, 1000).state, HealthState::Unknown, "never checked");

        let mut red = clean(1000);
        red.ci = Fetch::Ok(Ci { state: CiState::Failed, sha: "abc".into(), detail: "fehlgeschlagen: build".into() });
        red.prs = Fetch::Ok(PrList { items: vec![pr(1, true)], complete: true });
        let h = health(&target, Some(&red), None, 1000);
        assert_eq!(h.state, HealthState::Blocked, "a blocker outranks attention");
        assert!(h.reasons[0].contains("CI auf main"), "{:?}", h.reasons);
        assert_eq!(h.reasons.len(), 2);

        let mut dependabot = clean(1000);
        dependabot.prs = Fetch::Ok(PrList { items: vec![pr(1, true), pr(2, false)], complete: true });
        assert_eq!(state(&dependabot), HealthState::NeedsAttention);
        let mut human = clean(1000);
        human.prs = Fetch::Ok(PrList { items: vec![pr(2, false)], complete: true });
        assert_eq!(state(&human), HealthState::Healthy, "an open PR alone is no care need");

        let mut unreleased = clean(1000);
        unreleased.since = Fetch::Ok(Since { base_kind: "tag", base: "1.0.0".into(), commits: 3 });
        assert_eq!(health(&target, Some(&unreleased), None, 1000).reasons, ["3 Commits seit Tag 1.0.0"]);

        let mut outdated = clean(1000);
        outdated.release = Fetch::Ok(Some("1.1.0".into()));
        assert_eq!(state(&outdated), HealthState::NeedsAttention, "the website documents 1.0.0");
    }

    #[test]
    fn missing_or_old_facts_are_never_healthy() {
        let target = target("o/app");
        let state = |facts: &Facts, now| health(&target, Some(facts), None, now).state;
        assert_eq!(state(&clean(1000), 1000 + FRESH_SECS + 1), HealthState::Unknown, "old");
        for ci in [CiState::Pending, CiState::NoChecks, CiState::Unknown] {
            let mut facts = clean(1000);
            facts.ci = Fetch::Ok(Ci { state: ci, sha: "abc".into(), detail: String::new() });
            assert_eq!(state(&facts, 1000), HealthState::Unknown, "{ci:?}");
        }
        let mut truncated = clean(1000);
        truncated.prs = Fetch::Ok(PrList { items: vec![pr(2, false)], complete: false });
        assert_eq!(state(&truncated, 1000), HealthState::Unknown);
        let mut failed = clean(1000);
        failed.prs = Fetch::Failed("Netz".into());
        assert_eq!(state(&failed, 1000), HealthState::Unknown);
        let mut optional = clean(1000);
        optional.release = Fetch::Failed("Netz".into());
        optional.since = Fetch::NotQueried(String::new());
        assert_eq!(state(&optional, 1000), HealthState::Healthy, "release data is optional");
        let mut old_but_red = clean(1000);
        old_but_red.ci = Fetch::Ok(Ci { state: CiState::Failed, sha: "abc".into(), detail: String::new() });
        let h = health(&target, Some(&old_but_red), None, 1000 + FRESH_SECS + 1);
        assert_eq!(h.state, HealthState::Blocked);
        assert!(h.stale);
    }

    #[test]
    fn a_red_pull_request_needs_attention_but_does_not_block_the_repository() {
        let target = target("o/app");
        let mut facts = clean(1000);
        facts.prs = Fetch::Ok(PrList { items: vec![pr(2, false)], complete: true });
        let details = Details {
            fetched_at: 1000,
            prs: vec![PrDetail {
                number: 2,
                mergeable_state: Some("unstable".into()),
                mergeable: String::new(),
                ci: Fetch::Ok(Ci { state: CiState::Failed, sha: "h2".into(), detail: String::new() }),
            }],
            complete: true,
        };
        let h = health(&target, Some(&facts), Some(&details), 1000);
        assert_eq!(h.state, HealthState::NeedsAttention);
        assert_eq!(h.reasons, ["PR-CI fehlgeschlagen in #2"]);
    }

    /// `cargo test -p openreleasepilot-core reads_a_real_repository -- --ignored --nocapture`:
    /// facts, details and health of this app's own repository; reads only, no Claude.
    #[test]
    #[ignore = "network"]
    fn reads_a_real_repository() {
        let target = target(crate::update::REPOSITORY);
        let facts = fetch_facts(&target, 1000);
        println!("{}", serde_json::to_string_pretty(&facts).unwrap());
        assert!(facts.access.ok().is_some(), "{:?}", facts.access);
        assert!(facts.ci.ok().is_some(), "{:?}", facts.ci);
        let details = facts.prs.ok().map(|prs| fetch_details(crate::update::REPOSITORY, prs, 1000));
        println!("{}", serde_json::to_string_pretty(&details).unwrap());
        println!("{:#?}", health(&target, Some(&facts), details.as_ref(), 1000));
    }

    #[test]
    fn reading_works_without_write_access_and_writes_nothing() {
        let target = target("o/app");
        mock::install([
            ("repos/o/app".to_owned(), Some(json!({ "full_name": "o/app", "default_branch": "main", "archived": false }))),
            ("repos/o/app/branches/main".to_owned(), Some(json!({ "commit": { "sha": "abc" } }))),
            ("repos/o/app/commits/abc/check-runs?per_page=100".to_owned(), Some(runs(json!([{ "name": "build", "status": "completed", "conclusion": "success" }])))),
            ("repos/o/app/commits/abc/status?per_page=100".to_owned(), Some(statuses(json!([])))),
            ("repos/o/app/pulls?state=open&per_page=100&page=1".to_owned(), Some(json!([]))),
            ("repos/o/app/releases/latest".to_owned(), None),
            ("repos/o/app/tags?per_page=100&page=1".to_owned(), Some(json!([{ "name": "1.0.0" }, { "name": "1.0.10" }, { "name": "1.0.9" }]))),
            ("repos/o/app/compare/1.0.10...abc".to_owned(), Some(json!({ "status": "ahead", "ahead_by": 2 }))),
        ]);
        crate::shell::RAN.with(|ran| ran.borrow_mut().clear());
        let facts = fetch_facts(&target, 1000);
        assert_eq!(facts.tag.ok().cloned(), Some(Some("1.0.10".into())), "numeric order, not text order");
        assert_eq!(facts.since.ok().map(|s| (s.base_kind, s.commits)), Some(("tag", 2)));
        assert_eq!(facts.access.ok().unwrap().can_push, None, "public data without a login");
        let h = health(&target, Some(&facts), None, 1000);
        assert_eq!(h.state, HealthState::NeedsAttention);
        assert_eq!(capabilities(&target, Some(&facts), Some(false)).care.as_deref(), Some("GitHub-CLI nicht angemeldet (gh auth login)"));
        assert!(mock::writes().is_empty());
        assert!(crate::shell::RAN.with(|ran| ran.borrow().is_empty()), "no git command at all");

        let facts = Facts::new(1000, Fetch::Ok(Access::from_json(&json!({ "full_name": "o/app", "default_branch": "main", "archived": true })).unwrap()), false);
        assert_eq!(capabilities(&target, Some(&facts), Some(true)).care.as_deref(), Some("archiviert"));

        mock::install([("repos/o/app".to_owned(), None)]);
        let facts = fetch_facts(&target, 1000);
        assert!(facts.unreadable);
        let h = health(&target, Some(&facts), None, 1000);
        assert_eq!(h.state, HealthState::Blocked);
        assert!(h.reasons[0].contains("privat ohne Zugriff"), "404 is not called deleted: {:?}", h.reasons);
    }
}
