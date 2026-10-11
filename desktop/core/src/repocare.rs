//! "Repos pflegen" for the selected products' repositories on GitHub: merge open pull requests that
//! GitHub reports mergeable with green checks and Claude judges safe, close superfluous ones, ask
//! Dependabot to rebase conflicting ones, tag the next patch version once the default branch's CI is
//! green, delete branches whose commits are all in the default branch (on GitHub and in the clone).
//!
//! `scan` only looks; Claude's review (`apply_review`) and `finish` complete the preview. The run
//! checks every item again before it acts, so a preview that has aged cannot merge, close or delete
//! something that changed since.

use crate::github::{encode_ref, get, get_pages, message, send};
use crate::localrepo;
use crate::shell::{output, Runner};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// What happens with an open pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    #[default]
    Keep,
    Merge,
    Close,
    /// Comment `@dependabot rebase`; a later run can merge it.
    Rebase,
}

/// What the run will do in one repository, as shown in the preview.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub name: String,
    /// `owner/name`
    pub repo: String,
    pub default_branch: String,
    /// `merge`, `squash` or `rebase`, whichever the repository allows first.
    pub merge_method: String,
    pub tag_prefix: String,
    /// Open pull requests into the default branch; merges in this order.
    pub prs: Vec<PullRequest>,
    /// What Claude's review cost.
    pub review_cost: Option<f64>,
    /// Why there is no review; then every pull request stays open.
    pub review_note: Option<String>,
    /// Newest release tag `x.y.z` before the run.
    pub last_tag: Option<String>,
    /// Next patch version, if the default branch moved since `last_tag` (or will by the merges).
    pub tag: Option<String>,
    /// Why there is no tag.
    pub tag_note: Option<String>,
    /// Branches on GitHub to delete.
    pub branches: Vec<Branch>,
    /// Absolute path of the local clone, if it exists and belongs to the repository.
    pub clone: Option<String>,
    pub local_branches: Vec<Branch>,
    /// Why the local clone was left out.
    pub clone_note: Option<String>,
    /// The repository could not be looked at; nothing happens there.
    pub error: Option<String>,
    /// GitHub deletes head branches after a merge itself.
    #[serde(skip)]
    pub github_deletes_merged: bool,
}

impl Plan {
    pub fn count(&self, action: Action) -> usize {
        self.prs.iter().filter(|pr| pr.action == action).count()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub dependabot: bool,
    pub head_sha: String,
    /// Head branch, if it lives in this repository (not in a fork).
    pub head_branch: Option<String>,
    pub url: String,
    /// GitHub reports it mergeable, and it has checks, all green.
    pub ready: bool,
    /// Why GitHub keeps it from merging; empty when ready.
    pub gate: String,
    /// Merge conflict or behind the target branch.
    pub conflict: bool,
    pub action: Action,
    /// `green`, `yellow` or `red`, as Claude judged it.
    pub risk: Option<String>,
    /// e.g. `18.0.14 → 18.1.0`.
    pub bump: Option<String>,
    /// Why the action: Claude's reason, or the gate.
    pub reason: String,
    /// e.g. `+12 −3 in 2 Dateien`, for the prompt.
    #[serde(skip)]
    pub size: String,
    #[serde(skip)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Branch {
    pub name: String,
    /// Commit the plan saw; the branch is only deleted while it still points there.
    pub sha: String,
    /// e.g. `in main enthalten`, `PR #12 gemergt`.
    pub reason: String,
    /// Head of a pull request merged by the run: deleted only if that merge succeeds.
    pub after_merge: Option<u64>,
}

/// Claude's answer (`claude::REVIEW_SCHEMA`).
#[derive(Debug, Clone, Deserialize)]
pub struct Review {
    pub prs: Vec<Verdict>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Verdict {
    pub number: u64,
    /// `merge`, `keep` or `close`
    pub action: String,
    /// `green`, `yellow` or `red`
    pub risk: String,
    #[serde(default)]
    pub bump: String,
    pub reason: String,
}

/// What happened with one item of the plan.
pub enum Done {
    Ok(String),
    Skipped(String),
}

/// The facts of one repository: open pull requests with GitHub's verdict, branches to delete.
/// Errors end up in `Plan::error`, so one unreachable repository does not hide the others.
pub fn scan(name: &str, repo: &str, tag_prefix: &str, clone: Option<&Path>, merge: bool, cleanup: bool) -> Plan {
    let mut plan = Plan { name: name.to_owned(), repo: repo.to_owned(), tag_prefix: tag_prefix.to_owned(), ..Plan::default() };
    if let Err(e) = look(&mut plan, clone, merge, cleanup) {
        plan.prs.clear();
        plan.branches.clear();
        plan.local_branches.clear();
        plan.error = Some(format!("{e:#}"));
    }
    plan
}

fn look(plan: &mut Plan, clone: Option<&Path>, merge: bool, cleanup: bool) -> Result<()> {
    let repo = plan.repo.clone();
    let info = get(&format!("repos/{repo}"))?.context("Repository nicht gefunden oder kein Zugriff")?;
    if info["archived"].as_bool() == Some(true) {
        bail!("Repository ist archiviert");
    }
    if info["permissions"]["push"].as_bool() == Some(false) {
        bail!("kein Schreibzugriff");
    }
    let default = info["default_branch"].as_str().context("Antwort ohne default_branch")?.to_owned();
    plan.default_branch = default.clone();
    plan.merge_method = merge_method(&info).to_owned();
    plan.github_deletes_merged = info["delete_branch_on_merge"].as_bool() == Some(true);

    let open = get_pages(&format!("repos/{repo}/pulls?state=open"), 3)?;
    if merge {
        for listed in open.iter().filter(|pr| pr["base"]["ref"].as_str() == Some(default.as_str())) {
            let number = listed["number"].as_u64().unwrap_or(0);
            let detail = pull(&repo, number)?;
            let mut pr = pull_request(&detail, &repo);
            let gate = gate(&repo, &detail)?;
            pr.ready = gate.is_none();
            pr.conflict = matches!(detail["mergeable_state"].as_str(), Some("dirty" | "behind"));
            pr.gate = gate.unwrap_or_default();
            pr.reason = if pr.ready { "von Claude noch nicht bewertet".into() } else { pr.gate.clone() };
            pr.files = get_pages(&format!("repos/{repo}/pulls/{number}/files"), 1)?
                .iter()
                .filter_map(|f| f["filename"].as_str().map(str::to_owned))
                .collect();
            plan.prs.push(pr);
        }
    }
    if cleanup {
        let merged = merged_heads(&repo)?;
        let open_heads: HashSet<&str> = open.iter().filter(|pr| same_repo(pr, &repo)).filter_map(|pr| pr["head"]["ref"].as_str()).collect();
        for branch in get_pages(&format!("repos/{repo}/branches"), 5)? {
            let (Some(name), Some(sha)) = (branch["name"].as_str(), branch["commit"]["sha"].as_str()) else { continue };
            if name == default || branch["protected"].as_bool() == Some(true) || open_heads.contains(name) {
                continue;
            }
            let reason = match merged.get(sha) {
                Some(number) => format!("PR #{number} gemergt"),
                None if contained(&repo, &default, name)? => format!("in {default} enthalten"),
                None => continue,
            };
            plan.branches.push(Branch { name: name.to_owned(), sha: sha.to_owned(), reason, after_merge: None });
        }
        if let Some(dir) = clone {
            if let Err(e) = look_local(plan, dir, &merged) {
                plan.local_branches.clear();
                plan.clone_note = Some(format!("{e:#}"));
            }
        }
    }
    Ok(())
}

/// Branches of the local clone that are merged; `git fetch --prune` first, so `origin/<default>` is current.
fn look_local(plan: &mut Plan, dir: &Path, merged: &HashMap<String, u64>) -> Result<()> {
    let origin = localrepo::origin_repo(dir).context("lokaler Klon ohne GitHub-origin")?;
    if !origin.eq_ignore_ascii_case(&plan.repo) {
        bail!("lokaler Klon gehört zu {origin}");
    }
    plan.clone = Some(dir.display().to_string());
    output("git", &["fetch", "--prune", "--quiet", "origin"], dir).context("git fetch im lokalen Klon")?;
    let base = format!("refs/remotes/origin/{}", plan.default_branch);
    output("git", &["rev-parse", "--verify", "--quiet", &base], dir).with_context(|| format!("origin/{} fehlt im lokalen Klon", plan.default_branch))?;
    let current = output("git", &["branch", "--show-current"], dir).unwrap_or_default();
    for line in output("git", &["for-each-ref", "--format=%(refname:short)%09%(objectname)", "refs/heads"], dir)?.lines() {
        let Some((name, sha)) = line.split_once('\t') else { continue };
        if name == plan.default_branch || name == current.trim() {
            continue;
        }
        let reason = match merged.get(sha) {
            Some(number) => format!("PR #{number} gemergt"),
            None if output("git", &["merge-base", "--is-ancestor", sha, &base], dir).is_ok() => format!("in {} enthalten", plan.default_branch),
            None => continue,
        };
        plan.local_branches.push(Branch { name: name.to_owned(), sha: sha.to_owned(), reason, after_merge: None });
    }
    Ok(())
}

/// Turns Claude's verdicts into actions. The app has the last word: it never merges what GitHub
/// does not report ready or what Claude rates red, and without a verdict a pull request stays open.
pub fn apply_review(plan: &mut Plan, verdicts: &[Verdict]) {
    for pr in &mut plan.prs {
        let Some(verdict) = verdicts.iter().find(|v| v.number == pr.number) else {
            pr.action = Action::Keep;
            pr.reason = if pr.ready { "von Claude nicht bewertet".into() } else { pr.gate.clone() };
            continue;
        };
        pr.risk = Some(verdict.risk.clone()).filter(|r| matches!(r.as_str(), "green" | "yellow" | "red"));
        pr.bump = Some(verdict.bump.trim().to_owned()).filter(|b| !b.is_empty());
        let reason = verdict.reason.trim().to_owned();
        (pr.action, pr.reason) = match verdict.action.as_str() {
            "close" => (Action::Close, reason),
            "merge" if verdict.risk == "red" => (Action::Keep, format!("Risiko rot: {reason}")),
            "merge" if pr.ready => (Action::Merge, reason),
            "merge" if pr.conflict && pr.dependabot => (Action::Rebase, format!("{}; danach mergbar: {reason}", pr.gate)),
            "merge" => (Action::Keep, format!("{}; {reason}", pr.gate)),
            _ => (Action::Keep, reason),
        };
    }
    // Low risk first, as in the dependabot-pr skill.
    let order = |pr: &PullRequest| match pr.risk.as_deref() {
        Some("green") => 0,
        Some("yellow") => 1,
        _ => 2,
    };
    plan.prs.sort_by_key(|pr| (order(pr), pr.number));
}

/// Completes the preview after the review: the tag and the head branches of the merges.
pub fn finish(plan: &mut Plan, merge: bool, cleanup: bool) {
    if plan.error.is_some() {
        return;
    }
    if merge {
        if let Err(e) = plan_tag(plan) {
            plan.tag = None;
            plan.tag_note = Some(format!("{e:#}"));
        }
    }
    if cleanup && !plan.github_deletes_merged {
        let heads: Vec<Branch> = plan
            .prs
            .iter()
            .filter(|pr| pr.action == Action::Merge)
            .filter_map(|pr| {
                let name = pr.head_branch.clone().filter(|b| *b != plan.default_branch)?;
                Some(Branch { name, sha: pr.head_sha.clone(), reason: format!("nach dem Merge von PR #{}", pr.number), after_merge: Some(pr.number) })
            })
            .collect();
        plan.branches.splice(0..0, heads);
    }
}

fn plan_tag(plan: &mut Plan) -> Result<()> {
    let tags: Vec<String> =
        get_pages(&format!("repos/{}/tags", plan.repo), 3)?.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect();
    let Some(last) = latest_release_tag(&tags, &plan.tag_prefix) else {
        plan.tag_note = Some(format!("noch kein Tag im Format {}x.y.z; das erste Release von Hand taggen", plan.tag_prefix));
        return Ok(());
    };
    let next = next_patch(&last, &plan.tag_prefix).context("Tag-Version unlesbar")?;
    plan.last_tag = Some(last.clone());
    if tags.contains(&next) {
        plan.tag_note = Some(format!("{next} gibt es schon"));
    } else if plan.count(Action::Merge) == 0 && commits_since(&plan.repo, &last, &plan.default_branch)? == 0 {
        plan.tag_note = Some(format!("{} unverändert seit {last}", plan.default_branch));
    } else {
        plan.tag = Some(next);
    }
    Ok(())
}

// MARK: Run

/// Merges a planned pull request if it is still open, unchanged and ready. A Dependabot pull
/// request that an earlier merge left in conflict gets `@dependabot rebase` instead.
pub fn merge(plan: &Plan, pr: &PullRequest) -> Result<Done> {
    let current = pull(&plan.repo, pr.number)?;
    if current["state"].as_str() != Some("open") {
        return Ok(Done::Skipped("nicht mehr offen".into()));
    }
    if current["head"]["sha"].as_str() != Some(pr.head_sha.as_str()) {
        return Ok(Done::Skipped("hat seit der Vorschau neue Commits".into()));
    }
    if let Some(reason) = gate(&plan.repo, &current)? {
        if pr.dependabot && matches!(current["mergeable_state"].as_str(), Some("dirty" | "behind")) {
            comment(plan, pr.number, "@dependabot rebase")?;
            return Ok(Done::Skipped(format!("{reason}; @dependabot rebase angestoßen, der nächste Lauf kann ihn mergen")));
        }
        return Ok(Done::Skipped(reason));
    }
    let body = json!({ "merge_method": plan.merge_method, "sha": pr.head_sha });
    match send("PUT", &format!("repos/{}/pulls/{}/merge", plan.repo, pr.number), Some(&body))? {
        (200, _) => Ok(Done::Ok(format!("gemergt ({})", plan.merge_method))),
        (_, body) => bail!("GitHub lehnt den Merge ab: {}", message(&body)),
    }
}

/// Closes a superfluous pull request with the reason as a comment, if it has not changed.
pub fn close(plan: &Plan, pr: &PullRequest) -> Result<Done> {
    let current = pull(&plan.repo, pr.number)?;
    if current["state"].as_str() != Some("open") {
        return Ok(Done::Ok("war schon geschlossen".into()));
    }
    if current["head"]["sha"].as_str() != Some(pr.head_sha.as_str()) {
        return Ok(Done::Skipped("hat seit der Vorschau neue Commits".into()));
    }
    comment(plan, pr.number, &format!("Geschlossen von OpenReleasePilot: {}", pr.reason))?;
    match send("PATCH", &format!("repos/{}/pulls/{}", plan.repo, pr.number), Some(&json!({ "state": "closed" })))? {
        (200, _) => Ok(Done::Ok("geschlossen".into())),
        (_, body) => bail!("GitHub lehnt das Schließen ab: {}", message(&body)),
    }
}

/// Asks Dependabot to rebase a conflicting pull request.
pub fn rebase(plan: &Plan, pr: &PullRequest) -> Result<Done> {
    let current = pull(&plan.repo, pr.number)?;
    if current["state"].as_str() != Some("open") {
        return Ok(Done::Skipped("nicht mehr offen".into()));
    }
    comment(plan, pr.number, "@dependabot rebase")?;
    Ok(Done::Ok("@dependabot rebase angestoßen".into()))
}

fn comment(plan: &Plan, number: u64, body: &str) -> Result<()> {
    match send("POST", &format!("repos/{}/issues/{number}/comments", plan.repo), Some(&json!({ "body": body })))? {
        (201, _) => Ok(()),
        (_, body) => bail!("GitHub lehnt den Kommentar ab: {}", message(&body)),
    }
}

/// Head commit of the default branch.
pub fn head(plan: &Plan) -> Result<String> {
    let branch = get(&format!("repos/{}/branches/{}", plan.repo, encode_ref(&plan.default_branch)))?.context("Zielbranch nicht gefunden")?;
    Ok(branch["commit"]["sha"].as_str().context("Zielbranch ohne Commit")?.to_owned())
}

/// Sets the planned tag on `sha` (the default branch after the merges), but only if it moved since
/// the last tag and its checks pass: waits for them, so a broken state never becomes a release.
pub fn tag(plan: &Plan, sha: &str, runner: &Runner, progress: &dyn Fn(&str)) -> Result<Done> {
    let Some(tag) = &plan.tag else { return Ok(Done::Skipped("kein Tag geplant".into())) };
    if version(tag, &plan.tag_prefix).is_none() {
        bail!("{tag} ist keine Version im Format {}x.y.z", plan.tag_prefix);
    }
    if let Some(last) = &plan.last_tag {
        if commits_since(&plan.repo, last, sha)? == 0 {
            return Ok(Done::Skipped(format!("{} unverändert seit {last}", plan.default_branch)));
        }
    }
    if get(&format!("repos/{}/git/ref/tags/{}", plan.repo, encode_ref(tag)))?.is_some() {
        return Ok(Done::Skipped(format!("{tag} gibt es schon")));
    }
    let ci = match wait_for_checks(plan, sha, runner, progress)? {
        Checks::Passed => format!("CI auf {} grün", plan.default_branch),
        Checks::None => format!("{} hat keine Checks", plan.default_branch),
        Checks::Failed(name) => return Ok(Done::Skipped(format!("CI auf {} rot ({name}); kein Tag", plan.default_branch))),
        Checks::TimedOut => return Ok(Done::Skipped(format!("CI auf {} läuft nach 30 Minuten noch; Tag später setzen", plan.default_branch))),
    };
    let body = json!({ "ref": format!("refs/tags/{tag}"), "sha": sha });
    match send("POST", &format!("repos/{}/git/refs", plan.repo), Some(&body))? {
        (201, _) => Ok(Done::Ok(format!("gesetzt auf {} ({ci})", short(sha)))),
        (_, body) => bail!("GitHub lehnt den Tag ab: {}", message(&body)),
    }
}

enum Checks {
    Passed,
    /// No check showed up within three minutes: the branch has no CI of its own.
    None,
    Failed(String),
    TimedOut,
}

fn wait_for_checks(plan: &Plan, sha: &str, runner: &Runner, progress: &dyn Fn(&str)) -> Result<Checks> {
    let start = Instant::now();
    let mut reported = false;
    loop {
        runner.check()?;
        let runs = get(&format!("repos/{}/commits/{sha}/check-runs?per_page=100", plan.repo))?.unwrap_or_default();
        let runs = runs["check_runs"].as_array().cloned().unwrap_or_default();
        let status = get(&format!("repos/{}/commits/{sha}/status", plan.repo))?.unwrap_or_default();
        let statuses = status["statuses"].as_array().cloned().unwrap_or_default();
        if let Some(failed) = runs.iter().find(|r| {
            matches!(r["conclusion"].as_str(), Some("failure" | "cancelled" | "timed_out" | "action_required" | "startup_failure"))
        }) {
            return Ok(Checks::Failed(failed["name"].as_str().unwrap_or("?").to_owned()));
        }
        if let Some(failed) = statuses.iter().find(|s| matches!(s["state"].as_str(), Some("failure" | "error"))) {
            return Ok(Checks::Failed(failed["context"].as_str().unwrap_or("?").to_owned()));
        }
        let pending = runs.iter().any(|r| r["status"].as_str() != Some("completed")) || statuses.iter().any(|s| s["state"].as_str() == Some("pending"));
        let any = !runs.is_empty() || !statuses.is_empty();
        if any && !pending {
            return Ok(Checks::Passed);
        }
        let waited = start.elapsed();
        if !any && waited > Duration::from_secs(180) {
            return Ok(Checks::None);
        }
        if waited > Duration::from_secs(1800) {
            return Ok(Checks::TimedOut);
        }
        if !reported {
            progress(&format!("warte auf die CI auf {} ({}) …", plan.default_branch, short(sha)));
            reported = true;
        }
        for _ in 0..15 {
            runner.check()?;
            thread::sleep(Duration::from_secs(1));
        }
    }
}

/// Deletes a planned branch on GitHub while it still points where the plan saw it.
pub fn delete_branch(plan: &Plan, branch: &Branch) -> Result<Done> {
    let Some(current) = get(&format!("repos/{}/branches/{}", plan.repo, encode_ref(&branch.name)))? else {
        return Ok(Done::Ok("war schon gelöscht".into()));
    };
    if current["commit"]["sha"].as_str() != Some(branch.sha.as_str()) {
        return Ok(Done::Skipped("hat seit der Vorschau neue Commits".into()));
    }
    if current["protected"].as_bool() == Some(true) {
        return Ok(Done::Skipped("ist geschützt".into()));
    }
    match send("DELETE", &format!("repos/{}/git/refs/heads/{}", plan.repo, encode_ref(&branch.name)), None)? {
        (204, _) => Ok(Done::Ok("gelöscht".into())),
        (_, body) => bail!("GitHub lehnt das Löschen ab: {}", message(&body)),
    }
}

/// Deletes a planned branch of the local clone while it still points where the plan saw it.
pub fn delete_local_branch(plan: &Plan, branch: &Branch) -> Result<Done> {
    let dir = Path::new(plan.clone.as_deref().context("kein lokaler Klon")?);
    if output("git", &["branch", "--show-current"], dir).unwrap_or_default().trim() == branch.name {
        return Ok(Done::Skipped("ist ausgecheckt".into()));
    }
    match output("git", &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", branch.name)], dir) {
        Err(_) => return Ok(Done::Ok("war schon gelöscht".into())),
        Ok(sha) if sha.trim() != branch.sha => return Ok(Done::Skipped("hat seit der Vorschau neue Commits".into())),
        Ok(_) => {}
    }
    // -D, because a squash-merged branch is not an ancestor; the plan checked it is merged.
    output("git", &["branch", "-D", &branch.name], dir)?;
    Ok(Done::Ok("gelöscht".into()))
}

// MARK: GitHub facts

/// The pull request with `mergeable` computed; GitHub fills it in the background after a change.
fn pull(repo: &str, number: u64) -> Result<Value> {
    let path = format!("repos/{repo}/pulls/{number}");
    for _ in 0..5 {
        let pr = get(&path)?.with_context(|| format!("PR #{number} nicht gefunden"))?;
        if !pr["mergeable"].is_null() {
            return Ok(pr);
        }
        thread::sleep(Duration::from_secs(2));
    }
    get(&path)?.with_context(|| format!("PR #{number} nicht gefunden"))
}

/// `None` when GitHub reports the pull request mergeable and it has checks, all green; else why not.
fn gate(repo: &str, pr: &Value) -> Result<Option<String>> {
    if pr["draft"].as_bool() == Some(true) {
        return Ok(Some("Entwurf".into()));
    }
    if let Some(reason) = blocked_reason(pr["mergeable_state"].as_str().unwrap_or("unknown")) {
        return Ok(Some(reason.into()));
    }
    let sha = pr["head"]["sha"].as_str().context("PR ohne head.sha")?;
    if !has_checks(repo, sha)? {
        return Ok(Some("keine CI-Checks, also nichts, was grün sein könnte".into()));
    }
    Ok(None)
}

/// Why GitHub's `mergeable_state` keeps a pull request open; `None` when it can be merged. `clean`
/// means mergeable with every check finished and passed (a pending or failed one makes it `unstable`).
fn blocked_reason(state: &str) -> Option<&'static str> {
    Some(match state {
        "clean" | "has_hooks" => return None,
        "unstable" => "CI nicht grün (fehlgeschlagen oder läuft noch)",
        "blocked" => "blockiert (Review oder Pflicht-Checks fehlen)",
        "behind" => "nicht auf dem Stand des Zielbranches",
        "dirty" => "Merge-Konflikt",
        "draft" => "Entwurf",
        _ => "GitHub hat die Mergebarkeit noch nicht bestimmt",
    })
}

/// A commit has at least one check run or commit status; without any, "green" means nothing.
fn has_checks(repo: &str, sha: &str) -> Result<bool> {
    let runs = get(&format!("repos/{repo}/commits/{sha}/check-runs?per_page=1"))?.unwrap_or_default();
    if runs["total_count"].as_u64().unwrap_or(0) > 0 {
        return Ok(true);
    }
    let statuses = get(&format!("repos/{repo}/commits/{sha}/status"))?.unwrap_or_default();
    Ok(statuses["total_count"].as_u64().unwrap_or(0) > 0)
}

/// Merge commit if the repository allows it, else squash, else rebase.
fn merge_method(info: &Value) -> &'static str {
    let allowed = |key: &str| info[key].as_bool() != Some(false);
    if allowed("allow_merge_commit") {
        "merge"
    } else if allowed("allow_squash_merge") {
        "squash"
    } else {
        "rebase"
    }
}

fn pull_request(pr: &Value, repo: &str) -> PullRequest {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    let author = text(&pr["user"]["login"]);
    PullRequest {
        number: pr["number"].as_u64().unwrap_or(0),
        title: text(&pr["title"]),
        dependabot: author.starts_with("dependabot"),
        author,
        head_sha: text(&pr["head"]["sha"]),
        head_branch: same_repo(pr, repo).then(|| text(&pr["head"]["ref"])),
        url: text(&pr["html_url"]),
        size: format!(
            "+{} −{} in {} Dateien",
            pr["additions"].as_u64().unwrap_or(0),
            pr["deletions"].as_u64().unwrap_or(0),
            pr["changed_files"].as_u64().unwrap_or(0)
        ),
        ..PullRequest::default()
    }
}

fn same_repo(pr: &Value, repo: &str) -> bool {
    pr["head"]["repo"]["full_name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(repo))
}

/// Head commit → number of the recently merged pull requests (squash merges leave the branch
/// outside the default branch, but its head is still the merged one).
fn merged_heads(repo: &str) -> Result<HashMap<String, u64>> {
    Ok(get_pages(&format!("repos/{repo}/pulls?state=closed&sort=updated&direction=desc"), 1)?
        .iter()
        .filter(|pr| !pr["merged_at"].is_null())
        .filter_map(|pr| Some((pr["head"]["sha"].as_str()?.to_owned(), pr["number"].as_u64()?)))
        .collect())
}

/// Every commit of `branch` is in `base`.
fn contained(repo: &str, base: &str, branch: &str) -> Result<bool> {
    let compare = get(&format!("repos/{repo}/compare/{}...{}", encode_ref(base), encode_ref(branch)))?;
    Ok(compare.is_some_and(|c| matches!(c["status"].as_str(), Some("identical" | "behind"))))
}

/// Commits in `head` that `tag` does not have.
fn commits_since(repo: &str, tag: &str, head: &str) -> Result<u64> {
    let compare = get(&format!("repos/{repo}/compare/{}...{}", encode_ref(tag), encode_ref(head)))?.with_context(|| format!("{tag} und {head} nicht vergleichbar"))?;
    Ok(compare["ahead_by"].as_u64().unwrap_or(0))
}

/// `(1, 2, 3)` for `v1.2.3` with prefix `v`; only plain releases, no `-beta.1`.
fn version(tag: &str, prefix: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix(prefix)?.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// The highest release tag with the prefix.
fn latest_release_tag(tags: &[String], prefix: &str) -> Option<String> {
    tags.iter().filter_map(|tag| Some((version(tag, prefix)?, tag))).max_by_key(|(version, _)| *version).map(|(_, tag)| tag.clone())
}

fn next_patch(tag: &str, prefix: &str) -> Option<String> {
    let (major, minor, patch) = version(tag, prefix)?;
    Some(format!("{prefix}{major}.{minor}.{}", patch + 1))
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_latest_release_and_the_next_patch() {
        let tags: Vec<String> = ["0.1.9", "0.1.10", "0.2.0-beta.1", "v9.0.0", "0.1.2"].map(String::from).to_vec();
        assert_eq!(latest_release_tag(&tags, "").as_deref(), Some("0.1.10"));
        assert_eq!(next_patch("0.1.10", "").as_deref(), Some("0.1.11"));
        assert_eq!(latest_release_tag(&tags, "v").as_deref(), Some("v9.0.0"));
        assert_eq!(next_patch("v9.0.0", "v").as_deref(), Some("v9.0.1"));
        assert_eq!(latest_release_tag(&["beta".into(), "1.2".into(), "1.2.3.4".into()], ""), None);
    }

    #[test]
    fn only_clean_pull_requests_are_ready() {
        assert_eq!(blocked_reason("clean"), None);
        assert_eq!(blocked_reason("has_hooks"), None);
        for state in ["unstable", "blocked", "behind", "dirty", "draft", "unknown"] {
            assert!(blocked_reason(state).is_some(), "{state}");
        }
    }

    #[test]
    fn prefers_merge_commits() {
        assert_eq!(merge_method(&json!({})), "merge");
        assert_eq!(merge_method(&json!({ "allow_merge_commit": false })), "squash");
        assert_eq!(merge_method(&json!({ "allow_merge_commit": false, "allow_squash_merge": false })), "rebase");
    }

    #[test]
    fn the_app_has_the_last_word_on_merges() {
        let pr = |number, ready, conflict, dependabot| PullRequest {
            number,
            ready,
            conflict,
            dependabot,
            gate: if ready { String::new() } else { "Merge-Konflikt".into() },
            ..PullRequest::default()
        };
        let verdict = |number, action: &str, risk: &str| Verdict { number, action: action.into(), risk: risk.into(), bump: String::new(), reason: "ok".into() };
        let mut plan = Plan { prs: vec![pr(1, true, false, true), pr(2, true, false, true), pr(3, false, true, true), pr(4, false, true, false), pr(5, true, false, true), pr(6, true, false, true)], ..Plan::default() };
        apply_review(
            &mut plan,
            &[verdict(1, "merge", "yellow"), verdict(2, "merge", "red"), verdict(3, "merge", "green"), verdict(4, "merge", "green"), verdict(5, "close", "green"), verdict(6, "merge", "green")],
        );
        let action = |n| plan.prs.iter().find(|pr| pr.number == n).unwrap().action;
        assert_eq!(action(1), Action::Merge);
        assert_eq!(action(2), Action::Keep, "red is never merged");
        assert_eq!(action(3), Action::Rebase, "Dependabot rebases a conflict");
        assert_eq!(action(4), Action::Keep, "not ready, not Dependabot");
        assert_eq!(action(5), Action::Close);
        assert_eq!(plan.prs.iter().map(|pr| pr.number).collect::<Vec<_>>(), [3, 4, 5, 6, 1, 2], "green, then yellow, then red");

        let mut unreviewed = Plan { prs: vec![pr(7, true, false, true)], ..Plan::default() };
        apply_review(&mut unreviewed, &[]);
        assert_eq!(unreviewed.prs[0].action, Action::Keep, "no verdict, no merge");
    }

    /// `cargo test -p openreleasepilot-core scans_a_real_repository -- --ignored --nocapture`:
    /// the facts for this app's own repository; reads only, no local clone, no Claude.
    #[test]
    #[ignore = "network"]
    fn scans_a_real_repository() {
        let mut plan = scan("OpenReleasePilot", crate::update::REPOSITORY, "", None, true, true);
        finish(&mut plan, true, true);
        println!("{}", serde_json::to_string_pretty(&plan).unwrap());
        assert!(plan.error.is_none(), "{:?}", plan.error);
        assert_eq!(plan.default_branch, "main");
    }

    /// `cargo test -p openreleasepilot-core reviews_a_real_repository -- --ignored --nocapture`:
    /// scan plus Claude's review (Haiku, read-only) of this app's own repository; costs a few cents.
    #[test]
    #[ignore = "network, Claude"]
    fn reviews_a_real_repository() {
        let mut plan = scan("OpenReleasePilot", crate::update::REPOSITORY, "", None, true, false);
        assert!(plan.error.is_none(), "{:?}", plan.error);
        let args = crate::claude::review_args(&crate::claude::review_prompt(&plan), Some("haiku"), None, Some(1.0));
        let mut outcome = None;
        crate::shell::stream("claude", &args, &std::env::temp_dir(), &HashMap::new(), &Runner::default(), |line| {
            match crate::claude::parse(line) {
                crate::claude::Event::Line(text) => println!("{text}"),
                crate::claude::Event::Done(done) => outcome = Some(done),
                crate::claude::Event::Ignore => {}
            }
        })
        .unwrap();
        let outcome = outcome.expect("a result");
        println!("cost {:?}, denials {:?}", outcome.cost_usd, outcome.denials);
        let review: Review = serde_json::from_value(outcome.structured.expect("structured output")).unwrap();
        apply_review(&mut plan, &review.prs);
        finish(&mut plan, true, false);
        for pr in &plan.prs {
            println!("#{} {:?} {:?} {:?}: {}", pr.number, pr.action, pr.risk, pr.bump, pr.reason);
        }
        assert_eq!(review.prs.len(), plan.prs.len());
    }

    #[test]
    fn encodes_refs_for_paths() {
        assert_eq!(encode_ref("dependabot/cargo/sha2-0.11.0"), "dependabot/cargo/sha2-0.11.0");
        assert_eq!(encode_ref("fix#1 a"), "fix%231%20a");
    }
}
