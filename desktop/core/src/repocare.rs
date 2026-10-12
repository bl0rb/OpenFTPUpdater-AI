//! Care for the products' repositories on GitHub: merge open pull requests that GitHub reports
//! mergeable with green checks and Claude judges safe, close superfluous ones, ask Dependabot to
//! rebase conflicting ones, tag the next patch version once the CI of exactly that commit is green,
//! delete branches whose commits are all in the default branch (on GitHub and in the local clone).
//!
//! `scan` only reads: no GitHub write, no fetch, no checkout. Claude's review (`apply_review`) and
//! `finish` complete the preview. The run checks every item again right before it writes, so a
//! preview that has aged cannot merge, close, tag or delete something that changed since. Where the
//! GitHub API has no precondition (closing, deleting a branch), a short window between that check
//! and the write remains.

use crate::github::{encode_ref, get, get_list, message, send};
use crate::localrepo;
use crate::repos::{self, ci_for, CiState, LocalCopy, RepoTarget};
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
    /// Comment `@dependabot rebase`; a later preview can merge it.
    Rebase,
}

/// What the run will do in one repository, as shown in the preview.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// The products, e.g. `App, App Pro`.
    pub name: String,
    /// Identity, see `repos::RepoTarget::key`.
    pub key: String,
    /// `owner/name`
    pub repo: String,
    pub default_branch: String,
    /// Head of the default branch when the preview looked; the run expects it there.
    pub base_head: String,
    /// `merge`, `squash` or `rebase`, whichever the repository allows first.
    pub merge_method: String,
    pub tag_prefix: String,
    /// Open pull requests into the default branch; merges in this order.
    pub prs: Vec<PullRequest>,
    /// What Claude's review cost.
    pub review_cost: Option<f64>,
    /// Why there is no (complete) review; without a verdict a pull request stays open.
    pub review_note: Option<String>,
    /// Newest release tag `x.y.z` before the run.
    pub last_tag: Option<String>,
    /// Next patch version, if the default branch moved since `last_tag` (or will by the merges).
    pub tag: Option<String>,
    /// Why there is no tag.
    pub tag_note: Option<String>,
    /// Branches on GitHub to delete.
    pub branches: Vec<Branch>,
    /// Why branches on GitHub were not (all) looked at.
    pub cleanup_note: Option<String>,
    /// Absolute path of the local clone, if it exists and belongs to the repository.
    pub clone: Option<String>,
    pub local_branches: Vec<Branch>,
    /// The run updates the clone (`git fetch --prune origin`) before it deletes local branches; why.
    pub local_sync: Option<String>,
    /// Why the local clone was left out.
    pub clone_note: Option<String>,
    /// The repository could not be looked at; nothing happens there.
    pub error: Option<String>,
    /// GitHub deletes head branches after a merge itself.
    pub github_deletes_merged: bool,
    /// The products disagree on the tag prefix: no tag.
    #[serde(skip)]
    pub tag_blocked: bool,
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
    /// Target branch: the default branch.
    pub base: String,
    pub url: String,
    /// GitHub reports it mergeable, and its CI passed.
    pub ready: bool,
    /// Why GitHub keeps it from merging; empty when ready.
    pub gate: String,
    /// Merge conflict or behind the target branch.
    pub conflict: bool,
    pub action: Action,
    /// Claude gave one valid verdict for it.
    pub reviewed: bool,
    /// `green`, `yellow` or `red`, as Claude judged it.
    pub risk: Option<String>,
    /// e.g. `18.0.14 → 18.1.0`.
    pub bump: Option<String>,
    /// For a close: the newer pull request that replaces it.
    pub superseded_by: Option<u64>,
    /// Why the action: Claude's reason, or the gate.
    pub reason: String,
    /// e.g. `+12 −3 in 2 Dateien`, for the prompt.
    #[serde(skip)]
    pub size: String,
    #[serde(skip)]
    pub files: Vec<String>,
}

/// Why a branch may go; checked again before it is deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "pr", rename_all = "camelCase")]
pub enum Proof {
    /// Every commit is in the default branch.
    Contained,
    /// Head of this merged pull request (a squash merge leaves the branch outside the default branch).
    MergedPr(u64),
    /// Head of a pull request this run merges: deleted only if that merge succeeds.
    AfterMerge(u64),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Branch {
    pub name: String,
    /// Commit the plan saw; the branch is only deleted while it still points there.
    pub sha: String,
    /// e.g. `in main enthalten`, `PR #12 gemergt`.
    pub reason: String,
    pub proof: Proof,
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
    #[serde(default, rename = "supersededBy")]
    pub superseded_by: Option<u64>,
}

/// What happened with one item of the plan.
#[derive(Debug)]
pub enum Done {
    Ok(String),
    Skipped(String),
}

// MARK: Preview

/// The facts of one repository: open pull requests with GitHub's verdict, branches to delete.
/// Reads only. Errors end up in `Plan::error`, so one unreachable repository does not hide the others.
pub fn scan(target: &RepoTarget, merge: bool, cleanup: bool) -> Plan {
    let mut plan = Plan {
        name: target.name(),
        key: target.key.clone(),
        repo: target.repo.clone(),
        tag_prefix: target.tag_prefix.clone().unwrap_or_default(),
        ..Plan::default()
    };
    if let Some(conflict) = &target.tag_conflict {
        plan.tag_blocked = true;
        plan.tag_note = Some(format!("{conflict}; kein Tag, bis site.json eindeutig ist"));
    }
    if let Err(e) = look(&mut plan, target, merge, cleanup) {
        plan.prs.clear();
        plan.branches.clear();
        plan.local_branches.clear();
        plan.local_sync = None;
        plan.error = Some(format!("{e:#}"));
    }
    plan
}

fn look(plan: &mut Plan, target: &RepoTarget, merge: bool, cleanup: bool) -> Result<()> {
    let repo = plan.repo.clone();
    let info = get(&format!("repos/{repo}"))?.context("nicht gefunden oder kein Zugriff (GitHub meldet 404)")?;
    if info["archived"].as_bool() == Some(true) {
        bail!("archiviert: Pflege gesperrt");
    }
    if info["permissions"]["push"].as_bool() == Some(false) {
        bail!("nur lesbar: kein Schreibzugriff, Pflege gesperrt");
    }
    let default = info["default_branch"].as_str().context("Antwort ohne default_branch")?.to_owned();
    plan.default_branch = default.clone();
    plan.merge_method = merge_method(&info).to_owned();
    plan.github_deletes_merged = info["delete_branch_on_merge"].as_bool() == Some(true);
    plan.base_head = head(plan)?;

    let open = get_list(&format!("repos/{repo}/pulls?state=open"), 3)?;
    if merge {
        if !open.complete {
            plan.review_note = Some("mehr als 300 offene PRs; geprüft werden die ersten 300".into());
        }
        for listed in open.items.iter().filter(|pr| pr["base"]["ref"].as_str() == Some(default.as_str())) {
            let number = listed["number"].as_u64().unwrap_or(0);
            let detail = pull(&repo, number)?;
            let mut pr = pull_request(&detail, &repo);
            let gate = gate(&repo, &detail)?;
            pr.ready = gate.is_none();
            pr.conflict = matches!(detail["mergeable_state"].as_str(), Some("dirty" | "behind"));
            pr.gate = gate.unwrap_or_default();
            pr.reason = if pr.ready { "von Claude noch nicht bewertet".into() } else { pr.gate.clone() };
            let files = get_list(&format!("repos/{repo}/pulls/{number}/files"), 1)?;
            pr.files = files.items.iter().filter_map(|f| f["filename"].as_str().map(str::to_owned)).collect();
            if !files.complete {
                pr.files.push("…".into());
            }
            plan.prs.push(pr);
        }
    }
    if cleanup {
        if !open.complete {
            // Which branches open pull requests still need is not known for sure.
            plan.cleanup_note = Some("mehr als 300 offene PRs: nicht sicher, welche Branches noch gebraucht werden; kein Aufräumen".into());
            return Ok(());
        }
        let merged = merged_heads(&repo)?;
        let heads: HashSet<&str> = open.items.iter().filter(|pr| same_repo(pr, &repo)).filter_map(|pr| pr["head"]["ref"].as_str()).collect();
        let bases: HashSet<&str> = open.items.iter().filter_map(|pr| pr["base"]["ref"].as_str()).collect();
        let branches = get_list(&format!("repos/{repo}/branches"), 5)?;
        if !branches.complete {
            plan.cleanup_note = Some("mehr als 500 Branches; geprüft wurden die ersten 500".into());
        }
        for branch in &branches.items {
            let (Some(name), Some(sha)) = (branch["name"].as_str(), branch["commit"]["sha"].as_str()) else { continue };
            if name == default || branch["protected"].as_bool() == Some(true) || heads.contains(name) || bases.contains(name) {
                continue;
            }
            let (proof, reason) = match merged.get(sha) {
                Some(&number) => (Proof::MergedPr(number), format!("PR #{number} gemergt")),
                None if contained(&repo, &default, name)? => (Proof::Contained, format!("in {default} enthalten")),
                None => continue,
            };
            plan.branches.push(Branch { name: name.to_owned(), sha: sha.to_owned(), reason, proof });
        }
        match &target.local {
            LocalCopy::Found { path } => {
                if let Err(e) = look_local(plan, Path::new(path), &merged) {
                    plan.local_branches.clear();
                    plan.local_sync = None;
                    plan.clone_note = Some(format!("{e:#}"));
                }
            }
            LocalCopy::NotConfigured => {}
            other => plan.clone_note = repos::local_problem(other),
        }
    }
    Ok(())
}

/// Branches of the local clone that are merged, judged by what the clone already knows: no fetch.
/// When its `origin/<default>` differs from GitHub, the run fetches first (`local_sync`).
fn look_local(plan: &mut Plan, dir: &Path, merged: &HashMap<String, u64>) -> Result<()> {
    let origin = localrepo::origin_repo(dir).context("lokale Kopie ohne GitHub-origin")?;
    if !origin.eq_ignore_ascii_case(&plan.repo) {
        bail!("lokale Kopie gehört zu {origin}");
    }
    plan.clone = Some(dir.display().to_string());
    let base = format!("refs/remotes/origin/{}", plan.default_branch);
    let local_head = output("git", &["rev-parse", "--verify", "--quiet", &base], dir)
        .with_context(|| format!("origin/{} fehlt in der lokalen Kopie; einmal git fetch ausführen und neu prüfen", plan.default_branch))?;
    let checked_out = worktree_branches(dir)?;
    for line in output("git", &["for-each-ref", "--format=%(refname:short)%09%(objectname)", "refs/heads"], dir)?.lines() {
        let Some((name, sha)) = line.split_once('\t') else { continue };
        if name == plan.default_branch || checked_out.contains(name) {
            continue;
        }
        let (proof, reason) = match merged.get(sha) {
            Some(&number) => (Proof::MergedPr(number), format!("PR #{number} gemergt")),
            None if output("git", &["merge-base", "--is-ancestor", sha, &base], dir).is_ok() => {
                (Proof::Contained, format!("in origin/{} enthalten", plan.default_branch))
            }
            None => continue,
        };
        plan.local_branches.push(Branch { name: name.to_owned(), sha: sha.to_owned(), reason, proof });
    }
    if !plan.local_branches.is_empty() && local_head.trim() != plan.base_head {
        plan.local_sync = Some(format!(
            "origin/{} steht in der lokalen Kopie auf {}, auf GitHub auf {}",
            plan.default_branch,
            short(local_head.trim()),
            short(&plan.base_head)
        ));
    }
    Ok(())
}

/// Branches checked out in any worktree of the clone, the main one included.
fn worktree_branches(dir: &Path) -> Result<HashSet<String>> {
    let list = output("git", &["worktree", "list", "--porcelain"], dir)?;
    Ok(list.lines().filter_map(|line| line.strip_prefix("branch refs/heads/")).map(str::to_owned).collect())
}

/// Turns Claude's verdicts into actions; returns what was wrong with them. The app has the last
/// word: a pull request without exactly one valid verdict stays open, red is never merged, and a
/// merge needs GitHub's go (ready) and an explicit green or yellow.
pub fn apply_review(plan: &mut Plan, verdicts: &[Verdict]) -> Vec<String> {
    let mut problems: Vec<String> = verdicts
        .iter()
        .filter(|v| !plan.prs.iter().any(|pr| pr.number == v.number))
        .map(|v| format!("Bewertung für PR #{}, der nicht in der Liste steht, ignoriert", v.number))
        .collect();
    for pr in &mut plan.prs {
        pr.reviewed = false;
        pr.risk = None;
        pr.bump = None;
        pr.superseded_by = None;
        let matching: Vec<&Verdict> = verdicts.iter().filter(|v| v.number == pr.number).collect();
        let verdict = match matching.as_slice() {
            [] => {
                pr.action = Action::Keep;
                pr.reason = if pr.ready { "von Claude nicht bewertet".into() } else { pr.gate.clone() };
                continue;
            }
            [verdict] => *verdict,
            _ => {
                pr.action = Action::Keep;
                pr.reason = "von Claude mehrfach bewertet; keine Aktion".into();
                problems.push(format!("PR #{} mehrfach bewertet", pr.number));
                continue;
            }
        };
        let reason = verdict.reason.trim().to_owned();
        let invalid = if !matches!(verdict.risk.as_str(), "green" | "yellow" | "red") {
            Some(format!("ungültiges Risiko „{}“", verdict.risk))
        } else if !matches!(verdict.action.as_str(), "merge" | "keep" | "close") {
            Some(format!("ungültige Aktion „{}“", verdict.action))
        } else if reason.is_empty() {
            Some("Bewertung ohne Begründung".to_owned())
        } else if verdict.action == "close" && verdict.superseded_by == Some(pr.number) {
            Some("PR soll sich selbst ersetzen".to_owned())
        } else {
            None
        };
        if let Some(invalid) = invalid {
            pr.action = Action::Keep;
            pr.reason = format!("{invalid}; keine Aktion");
            problems.push(format!("PR #{}: {invalid}", pr.number));
            continue;
        }
        pr.reviewed = true;
        pr.risk = Some(verdict.risk.clone());
        pr.bump = Some(verdict.bump.trim().to_owned()).filter(|b| !b.is_empty());
        let safe = matches!(verdict.risk.as_str(), "green" | "yellow");
        (pr.action, pr.reason) = match verdict.action.as_str() {
            "close" => {
                pr.superseded_by = verdict.superseded_by;
                (Action::Close, reason)
            }
            "merge" if !safe => (Action::Keep, format!("Risiko rot: {reason}")),
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
    problems
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
    if cleanup && !plan.github_deletes_merged && plan.cleanup_note.is_none() {
        let heads: Vec<Branch> = plan
            .prs
            .iter()
            .filter(|pr| pr.action == Action::Merge)
            .filter_map(|pr| {
                let name = pr.head_branch.clone().filter(|b| *b != plan.default_branch)?;
                Some(Branch { name, sha: pr.head_sha.clone(), reason: format!("nach dem Merge von PR #{}", pr.number), proof: Proof::AfterMerge(pr.number) })
            })
            .collect();
        plan.branches.splice(0..0, heads);
    }
}

fn plan_tag(plan: &mut Plan) -> Result<()> {
    if plan.tag_blocked {
        return Ok(());
    }
    let list = get_list(&format!("repos/{}/tags", plan.repo), 3)?;
    if !list.complete {
        plan.tag_note = Some("über 300 Tags: der höchste ist nicht sicher bestimmbar; Tag von Hand setzen".into());
        return Ok(());
    }
    let tags: Vec<String> = list.items.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect();
    let Some(last) = latest_release_tag(&tags, &plan.tag_prefix) else {
        plan.tag_note = Some(format!("noch kein Tag im Format {}x.y.z; das erste Release von Hand taggen", plan.tag_prefix));
        return Ok(());
    };
    let next = next_patch(&last, &plan.tag_prefix).context("Tag-Version unlesbar")?;
    plan.last_tag = Some(last.clone());
    if tags.contains(&next) {
        plan.tag_note = Some(format!("{next} gibt es schon"));
        return Ok(());
    }
    match repos::commits_since(&plan.repo, &last, &plan.base_head) {
        Err(e) => plan.tag_note = Some(format!("Änderungen seit {last} {e:#}")),
        Ok(0) if plan.count(Action::Merge) == 0 => plan.tag_note = Some(format!("{} unverändert seit {last}", plan.default_branch)),
        Ok(_) => plan.tag = Some(next),
    }
    Ok(())
}

// MARK: Preview lifecycle

/// A preview waiting for the user, bound to what it was made for: the project, the options and each
/// repository's configuration. The window confirms it only by its id, never with actions of its own.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanSet {
    pub id: u64,
    #[serde(skip)]
    pub project: String,
    pub created_at: u64,
    pub merge: bool,
    pub cleanup: bool,
    pub model: String,
    /// Key and `RepoTarget::fingerprint` of each repository.
    #[serde(skip)]
    pub targets: Vec<(String, String)>,
    pub plans: Vec<Plan>,
}

impl PlanSet {
    /// `Ok` while project and configuration are as the preview saw them; else why the run must not use it.
    pub fn check_binding(&self, project: &str, current: &[RepoTarget]) -> Result<(), String> {
        if project != self.project {
            return Err("Das Projekt wurde gewechselt; die Vorschau gilt nicht mehr. Neu prüfen.".into());
        }
        for (key, fingerprint) in &self.targets {
            match current.iter().find(|t| &t.key == key) {
                Some(target) if &target.fingerprint() == fingerprint => {}
                Some(target) => return Err(format!("{}: tagPrefix oder lokale Kopie in site.json haben sich seit der Vorschau geändert. Neu prüfen.", target.repo)),
                None => return Err(format!("{key} gehört zu keinem Produkt mehr. Neu prüfen.")),
            }
        }
        Ok(())
    }
}

/// Takes the preview with this id for its run, so a second confirmation (a double click) finds none;
/// another preview stays where it is.
pub fn take_plan(slot: &mut Option<PlanSet>, id: u64) -> Result<PlanSet, String> {
    match slot.take() {
        Some(set) if set.id == id => Ok(set),
        Some(other) => {
            *slot = Some(other);
            Err("Diese Vorschau gilt nicht mehr; es gibt eine neuere".into())
        }
        None => Err("Keine Vorschau vorhanden; erst prüfen".into()),
    }
}

/// Stores a finished preview, unless the project changed while it was made: a late answer is dropped.
pub fn store_plan(slot: &mut Option<PlanSet>, project: &str, set: PlanSet) -> Result<(), String> {
    if set.project != project {
        return Err("Das Projekt wurde während der Prüfung gewechselt; die Vorschau ist verworfen.".into());
    }
    *slot = Some(set);
    Ok(())
}

// MARK: Run

/// What the run did so far in one repository; decides whether the tag may follow.
#[derive(Debug, Default)]
pub struct RunState {
    /// Pull requests this run merged.
    pub merged: HashSet<u64>,
    /// Planned merges that did not happen (skipped, failed, cancelled): then no tag.
    pub missed: Vec<u64>,
    /// Where the default branch should stand: the preview's head, then each merge's commit.
    pub expected_head: Option<String>,
    /// The default branch moved without this run.
    pub foreign: Option<String>,
    /// The local clone was brought up to date (`sync_local`) in this run.
    pub local_synced: bool,
}

impl RunState {
    pub fn new(plan: &Plan) -> Self {
        Self { expected_head: Some(plan.base_head.clone()).filter(|h| !h.is_empty()), ..Self::default() }
    }
}

/// Merges a planned pull request after checking it again: still open, same head, same target, a
/// valid green or yellow verdict, mergeable, CI of its head passed. GitHub gets the expected head
/// commit, so a push in between makes it refuse. A Dependabot pull request that this run's own
/// merges left in conflict gets `@dependabot rebase` instead, as the preview says.
pub fn merge(plan: &Plan, pr: &PullRequest, state: &mut RunState) -> Result<Done> {
    if pr.action != Action::Merge || !pr.reviewed || !matches!(pr.risk.as_deref(), Some("green" | "yellow")) {
        return Ok(Done::Skipped("keine gültige Freigabe (grün oder gelb) für diesen Stand".into()));
    }
    let current = pull(&plan.repo, pr.number)?;
    if let Some(why) = changed(plan, pr, &current) {
        return Ok(Done::Skipped(why));
    }
    if current["draft"].as_bool() == Some(true) {
        return Ok(Done::Skipped("ist inzwischen ein Entwurf".into()));
    }
    note_head(plan, state)?;
    let mergeable = current["mergeable_state"].as_str().unwrap_or("unknown");
    if let Some(reason) = blocked_reason(mergeable) {
        if pr.dependabot && matches!(mergeable, "dirty" | "behind") && !state.merged.is_empty() {
            comment(plan, pr.number, "@dependabot rebase")?;
            return Ok(Done::Skipped(format!("{reason} nach den Merges dieses Laufs; @dependabot rebase angestoßen, eine neue Vorschau kann ihn mergen")));
        }
        return Ok(Done::Skipped(reason.into()));
    }
    let ci = ci_for(&plan.repo, &pr.head_sha)?;
    if ci.state != CiState::Passed {
        return Ok(Done::Skipped(format!("CI des PR-Commits nicht grün ({})", ci.detail)));
    }
    let body = json!({ "merge_method": plan.merge_method, "sha": pr.head_sha });
    match send("PUT", &format!("repos/{}/pulls/{}/merge", plan.repo, pr.number), Some(&body)) {
        Ok((200, answer)) => {
            state.merged.insert(pr.number);
            state.expected_head = answer["sha"].as_str().map(str::to_owned);
            Ok(Done::Ok(format!("gemergt ({})", plan.merge_method)))
        }
        Ok((_, answer)) => bail!("GitHub lehnt den Merge ab: {}", message(&answer)),
        Err(e) => {
            // The answer got lost: read what happened before anything else writes.
            let now = get(&format!("repos/{}/pulls/{}", plan.repo, pr.number))?.unwrap_or_default();
            if now["merged"].as_bool() == Some(true) && now["head"]["sha"].as_str() == Some(pr.head_sha.as_str()) {
                state.merged.insert(pr.number);
                state.expected_head = now["merge_commit_sha"].as_str().map(str::to_owned);
                return Ok(Done::Ok("gemergt (Antwort verloren, Zustand bei GitHub nachgelesen)".into()));
            }
            Err(e.context("Ergebnis des Merges unklar; laut GitHub ist der PR nicht gemergt"))
        }
    }
}

/// Closes a superfluous pull request with the reason as a comment, after checking it again: still
/// open, same head and target, and a named replacement is still open or merged.
pub fn close(plan: &Plan, pr: &PullRequest) -> Result<Done> {
    let current = pull(&plan.repo, pr.number)?;
    if current["state"].as_str() != Some("open") {
        return Ok(Done::Skipped("war schon geschlossen".into()));
    }
    if let Some(why) = changed(plan, pr, &current) {
        return Ok(Done::Skipped(why));
    }
    if let Some(number) = pr.superseded_by {
        match get(&format!("repos/{}/pulls/{number}", plan.repo))? {
            None => return Ok(Done::Skipped(format!("Ersatz-PR #{number} nicht gefunden; die Begründung trägt nicht mehr"))),
            Some(other) if other["state"].as_str() == Some("closed") && other["merged"].as_bool() != Some(true) => {
                return Ok(Done::Skipped(format!("Ersatz-PR #{number} wurde ohne Merge geschlossen; die Begründung trägt nicht mehr")));
            }
            Some(_) => {}
        }
    }
    comment(plan, pr.number, &format!("Geschlossen von OpenReleasePilot: {}", pr.reason)).context("Kommentar nicht gepostet; der PR bleibt offen")?;
    match send("PATCH", &format!("repos/{}/pulls/{}", plan.repo, pr.number), Some(&json!({ "state": "closed" }))) {
        Ok((200, _)) => Ok(Done::Ok("Kommentar gepostet, geschlossen".into())),
        Ok((_, body)) => bail!("Kommentar gepostet, aber GitHub lehnt das Schließen ab: {}", message(&body)),
        Err(e) => {
            let now = get(&format!("repos/{}/pulls/{}", plan.repo, pr.number))?.unwrap_or_default();
            if now["state"].as_str() == Some("closed") {
                return Ok(Done::Ok("Kommentar gepostet, geschlossen (Zustand bei GitHub nachgelesen)".into()));
            }
            Err(e.context("Kommentar gepostet, Ergebnis des Schließens unklar; laut GitHub ist der PR offen"))
        }
    }
}

/// Asks Dependabot to rebase a conflicting pull request, after checking it again: still open,
/// really Dependabot's, same head and target, still in conflict or behind.
pub fn rebase(plan: &Plan, pr: &PullRequest) -> Result<Done> {
    let current = pull(&plan.repo, pr.number)?;
    if let Some(why) = changed(plan, pr, &current) {
        return Ok(Done::Skipped(why));
    }
    if !current["user"]["login"].as_str().is_some_and(|login| login.starts_with("dependabot")) {
        return Ok(Done::Skipped("kein Dependabot-PR".into()));
    }
    let state = if current["mergeable"].is_null() { None } else { current["mergeable_state"].as_str() };
    match state {
        Some("dirty" | "behind") => {}
        Some(other) => return Ok(Done::Skipped(format!("kein Konflikt mehr ({})", repos::mergeable_text(other)))),
        None => return Ok(Done::Skipped("Mergebarkeit von GitHub noch nicht bestimmt".into())),
    }
    comment(plan, pr.number, "@dependabot rebase")?;
    Ok(Done::Ok("@dependabot rebase angestoßen".into()))
}

/// Why the pull request is no longer the one the preview judged; `None` if it still is.
fn changed(plan: &Plan, pr: &PullRequest, current: &Value) -> Option<String> {
    if current["state"].as_str() != Some("open") {
        return Some(if current["merged"].as_bool() == Some(true) { "schon gemergt" } else { "nicht mehr offen" }.into());
    }
    if current["head"]["sha"].as_str() != Some(pr.head_sha.as_str()) {
        return Some("hat seit der Vorschau neue Commits".into());
    }
    let base = current["base"]["ref"].as_str().unwrap_or("?");
    if base != plan.default_branch {
        return Some(format!("zielt jetzt auf {base} statt auf {}", plan.default_branch));
    }
    if !current["base"]["repo"]["full_name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(&plan.repo)) {
        return Some("zielt auf ein anderes Repository".into());
    }
    None
}

/// Notes when the default branch is not where the run expects it (a push from elsewhere).
fn note_head(plan: &Plan, state: &mut RunState) -> Result<()> {
    if state.foreign.is_some() {
        return Ok(());
    }
    let Some(expected) = state.expected_head.clone() else { return Ok(()) };
    let head = head(plan)?;
    if head != expected {
        state.foreign = Some(format!("{} steht auf {} statt auf {}; die Änderung kam nicht aus diesem Lauf", plan.default_branch, short(&head), short(&expected)));
    }
    Ok(())
}

fn comment(plan: &Plan, number: u64, body: &str) -> Result<()> {
    match send("POST", &format!("repos/{}/issues/{number}/comments", plan.repo), Some(&json!({ "body": body })))? {
        (201, _) => Ok(()),
        (_, body) => bail!("GitHub lehnt den Kommentar ab: {}", message(&body)),
    }
}

/// Head commit of the default branch.
pub fn head(plan: &Plan) -> Result<String> {
    let branch = get(&format!("repos/{}/branches/{}", plan.repo, encode_ref(&plan.default_branch)))?.context("Default-Branch nicht gefunden")?;
    Ok(branch["commit"]["sha"].as_str().context("Default-Branch ohne Commit")?.to_owned())
}

/// How long the tag waits for CI.
pub struct CiWait {
    /// Without any check by then, the commit has none: no tag.
    pub no_checks_after: Duration,
    pub timeout: Duration,
    pub poll: Duration,
}

impl Default for CiWait {
    fn default() -> Self {
        Self { no_checks_after: Duration::from_secs(180), timeout: Duration::from_secs(1800), poll: Duration::from_secs(15) }
    }
}

/// Sets the planned tag, but only on the commit this run expects (the preview's head plus its own
/// merges, every planned merge done), with changes since the last tag, the release line unchanged,
/// and CI of exactly that commit positively passed. Never moves or replaces a tag.
pub fn tag(plan: &Plan, state: &RunState, runner: &Runner, wait: &CiWait, progress: &dyn Fn(&str)) -> Result<Done> {
    let Some(tag) = &plan.tag else { return Ok(Done::Skipped("kein Tag geplant".into())) };
    let Some(last) = &plan.last_tag else { return Ok(Done::Skipped("keine Ausgangsversion".into())) };
    if version(tag, &plan.tag_prefix).is_none() {
        bail!("{tag} ist keine Version im Format {}x.y.z", plan.tag_prefix);
    }
    if !state.missed.is_empty() {
        let missed: Vec<String> = state.missed.iter().map(|n| format!("#{n}")).collect();
        return Ok(Done::Skipped(format!("nicht alle geplanten Merges sind gelungen ({}); kein Tag", missed.join(", "))));
    }
    if let Some(foreign) = &state.foreign {
        return Ok(Done::Skipped(format!("{foreign}; kein Tag ohne neue Vorschau")));
    }
    let Some(target) = state.expected_head.clone() else {
        return Ok(Done::Skipped("Stand nach den Merges unbekannt; kein Tag".into()));
    };
    let current = head(plan)?;
    if current != target {
        return Ok(Done::Skipped(format!(
            "{} steht auf {} statt auf {}; die Änderung kam nicht aus diesem Lauf, kein Tag",
            plan.default_branch,
            short(&current),
            short(&target)
        )));
    }
    if let Some(why) = release_line_changed(plan, last, tag)? {
        return Ok(Done::Skipped(why));
    }
    match repos::commits_since(&plan.repo, last, &target) {
        Ok(0) => return Ok(Done::Skipped(format!("{} unverändert seit {last}", plan.default_branch))),
        Err(e) => return Ok(Done::Skipped(format!("Änderungen seit {last} {e:#}; kein Tag"))),
        Ok(_) => {}
    }
    let ci = wait_for_checks(&plan.repo, &target, runner, wait, &|text| progress(text))?;
    let commit = short(&target);
    match ci.state {
        CiState::Passed => {}
        CiState::Failed => return Ok(Done::Skipped(format!("CI von {commit} rot ({}); kein Tag", ci.detail))),
        CiState::Pending => return Ok(Done::Skipped(format!("CI von {commit} läuft nach {} Minuten noch; kein Tag", wait.timeout.as_secs() / 60))),
        CiState::NoChecks => return Ok(Done::Skipped(format!("keine CI-Checks für {commit} gefunden; ohne grüne CI kein automatischer Tag"))),
        CiState::Unknown => return Ok(Done::Skipped(format!("CI von {commit} unklar ({}); kein Tag", ci.detail))),
    }
    // Waiting may take long: look again right before writing. A newer tag or a moved base makes the
    // planned version wrong even when the default branch stayed where it was.
    if head(plan)? != target {
        return Ok(Done::Skipped(format!("{} hat sich während des Wartens bewegt; kein Tag", plan.default_branch)));
    }
    if get(&format!("repos/{}/git/ref/tags/{}", plan.repo, encode_ref(tag)))?.is_some() {
        return Ok(Done::Skipped(format!("{tag} gibt es inzwischen")));
    }
    if let Some(why) = release_line_changed(plan, last, tag)? {
        return Ok(Done::Skipped(format!("{why} (nach dem Warten auf die CI)")));
    }
    match repos::commits_since(&plan.repo, last, &target) {
        Ok(0) => return Ok(Done::Skipped(format!("{} unverändert seit {last} (nach dem Warten auf die CI)", plan.default_branch))),
        Err(e) => return Ok(Done::Skipped(format!("Änderungen seit {last} {e:#} (nach dem Warten auf die CI); kein Tag"))),
        Ok(_) => {}
    }
    let body = json!({ "ref": format!("refs/tags/{tag}"), "sha": target });
    match send("POST", &format!("repos/{}/git/refs", plan.repo), Some(&body)) {
        Ok((201, _)) => Ok(Done::Ok(format!("gesetzt auf {commit} ({}); ein Release-Workflow startet gegebenenfalls", ci.detail))),
        // 422: the ref exists after all; POST never moves it.
        Ok((422, body)) => Ok(Done::Skipped(format!("GitHub lehnt ab: {}; bestehende Tags bleiben unverändert", message(&body)))),
        Ok((_, body)) => bail!("GitHub lehnt den Tag ab: {}", message(&body)),
        Err(e) => {
            let now = get(&format!("repos/{}/git/ref/tags/{}", plan.repo, encode_ref(tag)))?;
            if now.is_some_and(|r| r["object"]["sha"].as_str() == Some(target.as_str())) {
                return Ok(Done::Ok(format!("gesetzt auf {commit} (Antwort verloren, Zustand bei GitHub nachgelesen)")));
            }
            Err(e.context("Ergebnis unklar; laut GitHub gibt es den Tag nicht"))
        }
    }
}

/// Why the release line is no longer the preview's: a newer tag, the planned one, an unreadable list.
fn release_line_changed(plan: &Plan, last: &str, tag: &str) -> Result<Option<String>> {
    let list = get_list(&format!("repos/{}/tags", plan.repo), 3)?;
    if !list.complete {
        return Ok(Some("über 300 Tags; die Version ist nicht sicher prüfbar, kein Tag".into()));
    }
    let names: Vec<String> = list.items.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect();
    if names.iter().any(|name| name == tag) {
        return Ok(Some(format!("{tag} gibt es schon")));
    }
    Ok(match latest_release_tag(&names, &plan.tag_prefix) {
        Some(now) if now == last => None,
        Some(now) => Some(format!("inzwischen gibt es {now}; neue Vorschau nötig")),
        None => Some(format!("{last} nicht mehr gefunden; kein Tag")),
    })
}

/// Waits until the CI of `sha` has a result: passed, failed, unknown, no checks after
/// `no_checks_after`, or still pending after `timeout`.
fn wait_for_checks(repo: &str, sha: &str, runner: &Runner, wait: &CiWait, progress: &dyn Fn(&str)) -> Result<repos::Ci> {
    let start = Instant::now();
    let mut reported = false;
    loop {
        runner.check()?;
        let ci = ci_for(repo, sha)?;
        let waited = start.elapsed();
        match ci.state {
            CiState::Passed | CiState::Failed | CiState::Unknown => return Ok(ci),
            CiState::NoChecks if waited >= wait.no_checks_after => return Ok(ci),
            _ if waited >= wait.timeout => return Ok(ci),
            _ => {}
        }
        if !reported {
            progress(&format!("warte auf die CI von {} …", short(sha)));
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

/// Deletes a planned branch on GitHub after checking it again: same commit, not protected, not the
/// default branch, no open pull request from or into it, the proof still holds. The delete itself
/// has no precondition on the commit at GitHub; a short window remains.
pub fn delete_branch(plan: &Plan, branch: &Branch, state: &RunState) -> Result<Done> {
    if let Proof::AfterMerge(number) = branch.proof {
        if !state.merged.contains(&number) {
            return Ok(Done::Skipped(format!("PR #{number} wurde nicht gemergt")));
        }
    }
    let path = format!("repos/{}/branches/{}", plan.repo, encode_ref(&branch.name));
    let Some(current) = get(&path)? else {
        return Ok(Done::Skipped("war schon gelöscht".into()));
    };
    if current["commit"]["sha"].as_str() != Some(branch.sha.as_str()) {
        return Ok(Done::Skipped("hat seit der Vorschau neue Commits".into()));
    }
    if current["protected"].as_bool() == Some(true) {
        return Ok(Done::Skipped("ist geschützt".into()));
    }
    let info = get(&format!("repos/{}", plan.repo))?.context("Repository nicht gefunden")?;
    if info["default_branch"].as_str() == Some(branch.name.as_str()) {
        return Ok(Done::Skipped("ist jetzt der Default-Branch".into()));
    }
    let owner = plan.repo.split('/').next().unwrap_or_default();
    let name = encode_ref(&branch.name);
    for (query, what) in [(format!("head={owner}:{name}"), "kommt aus ihm"), (format!("base={name}"), "zielt auf ihn")] {
        let open = get_list(&format!("repos/{}/pulls?state=open&{query}", plan.repo), 1)?;
        if let Some(pr) = open.items.first() {
            return Ok(Done::Skipped(format!("offener PR #{} {what}", pr["number"].as_u64().unwrap_or(0))));
        }
    }
    if let Some(why) = proof_gone(plan, branch, state, false)? {
        return Ok(Done::Skipped(why));
    }
    match send("DELETE", &format!("repos/{}/git/refs/heads/{name}", plan.repo), None) {
        Ok((204, _)) => Ok(Done::Ok("gelöscht".into())),
        Ok((_, body)) => bail!("GitHub lehnt das Löschen ab: {}", message(&body)),
        Err(e) => {
            if get(&path)?.is_none() {
                return Ok(Done::Ok("gelöscht (Antwort verloren, Zustand bei GitHub nachgelesen)".into()));
            }
            Err(e.context("Ergebnis unklar; laut GitHub gibt es den Branch noch"))
        }
    }
}

/// Updates the clone's view of GitHub (`git fetch --prune origin`), as the preview announced.
pub fn sync_local(plan: &Plan, state: &mut RunState) -> Result<Done> {
    let dir = Path::new(plan.clone.as_deref().context("keine lokale Kopie")?);
    if let Some(why) = foreign_clone(plan, dir) {
        return Ok(Done::Skipped(why));
    }
    output("git", &["fetch", "--prune", "--quiet", "origin"], dir).context("git fetch --prune in der lokalen Kopie")?;
    state.local_synced = true;
    Ok(Done::Ok("git fetch --prune origin".into()))
}

/// Deletes a planned branch of the local clone after checking it again: the clone still belongs to
/// the repository, an announced update of the clone succeeded, the branch is not the default branch,
/// not checked out in any worktree, still on the same commit, and the proof still holds on GitHub.
pub fn delete_local_branch(plan: &Plan, branch: &Branch, state: &RunState) -> Result<Done> {
    let dir = Path::new(plan.clone.as_deref().context("keine lokale Kopie")?);
    if let Some(why) = foreign_clone(plan, dir) {
        return Ok(Done::Skipped(why));
    }
    if plan.local_sync.is_some() && !state.local_synced {
        return Ok(Done::Skipped("die lokale Kopie wurde nicht abgeglichen; ohne aktuellen Stand wird nichts gelöscht".into()));
    }
    if branch.name == plan.default_branch {
        return Ok(Done::Skipped("ist der Default-Branch".into()));
    }
    if worktree_branches(dir)?.contains(&branch.name) {
        return Ok(Done::Skipped("ist in einem Worktree ausgecheckt".into()));
    }
    match output("git", &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", branch.name)], dir) {
        Err(_) => return Ok(Done::Skipped("war schon gelöscht".into())),
        Ok(sha) if sha.trim() != branch.sha => return Ok(Done::Skipped("hat seit der Vorschau neue Commits".into())),
        Ok(_) => {}
    }
    if let Some(why) = proof_gone(plan, branch, state, true)? {
        return Ok(Done::Skipped(why));
    }
    // -D, because a squash-merged branch is not an ancestor; the checks above proved it merged.
    output("git", &["branch", "-D", &branch.name], dir)?;
    Ok(Done::Ok("gelöscht".into()))
}

fn foreign_clone(plan: &Plan, dir: &Path) -> Option<String> {
    match localrepo::origin_repo(dir) {
        Some(origin) if origin.eq_ignore_ascii_case(&plan.repo) => None,
        origin => Some(format!("die lokale Kopie gehört nicht mehr zu {} ({})", plan.repo, origin.as_deref().unwrap_or("ohne GitHub-origin"))),
    }
}

/// Why the branch's proof no longer holds, judged by GitHub's current state. A local branch is
/// compared by its commit: the clone's `origin/<default>` may be older than GitHub.
fn proof_gone(plan: &Plan, branch: &Branch, state: &RunState, local: bool) -> Result<Option<String>> {
    Ok(match branch.proof {
        Proof::AfterMerge(number) => (!state.merged.contains(&number)).then(|| format!("PR #{number} wurde nicht gemergt")),
        Proof::MergedPr(number) => {
            let pr = get(&format!("repos/{}/pulls/{number}", plan.repo))?.unwrap_or_default();
            let holds = pr["merged"].as_bool() == Some(true) && pr["head"]["sha"].as_str() == Some(branch.sha.as_str());
            (!holds).then(|| format!("PR #{number} belegt den Merge nicht mehr"))
        }
        Proof::Contained => {
            let head = if local { &branch.sha } else { &branch.name };
            let holds = contained(&plan.repo, &plan.default_branch, head)?;
            (!holds).then(|| format!("nicht mehr vollständig in {} auf GitHub enthalten", plan.default_branch))
        }
    })
}

// MARK: GitHub facts

/// The pull request with `mergeable` computed; GitHub fills it in the background after a change.
fn pull(repo: &str, number: u64) -> Result<Value> {
    let path = format!("repos/{repo}/pulls/{number}");
    for _ in 0..5 {
        let pr = get(&path)?.with_context(|| format!("PR #{number} nicht gefunden"))?;
        if !pr["mergeable"].is_null() || pr["state"].as_str() != Some("open") {
            return Ok(pr);
        }
        thread::sleep(Duration::from_secs(2));
    }
    get(&path)?.with_context(|| format!("PR #{number} nicht gefunden"))
}

/// `None` when GitHub reports the pull request mergeable and the CI of its head passed; else why not.
fn gate(repo: &str, pr: &Value) -> Result<Option<String>> {
    if pr["draft"].as_bool() == Some(true) {
        return Ok(Some("Entwurf".into()));
    }
    if let Some(reason) = blocked_reason(pr["mergeable_state"].as_str().unwrap_or("unknown")) {
        return Ok(Some(reason.into()));
    }
    let sha = pr["head"]["sha"].as_str().context("PR ohne head.sha")?;
    let ci = ci_for(repo, sha)?;
    if ci.state != CiState::Passed {
        return Ok(Some(match ci.state {
            CiState::NoChecks => "keine CI-Checks, also nichts, was grün sein könnte".into(),
            _ => format!("CI nicht grün ({})", ci.detail),
        }));
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
        base: text(&pr["base"]["ref"]),
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
    Ok(get_list(&format!("repos/{repo}/pulls?state=closed&sort=updated&direction=desc"), 1)?
        .items
        .iter()
        .filter(|pr| !pr["merged_at"].is_null())
        .filter_map(|pr| Some((pr["head"]["sha"].as_str()?.to_owned(), pr["number"].as_u64()?)))
        .collect())
}

/// Every commit of `branch` (a branch name or a commit) is in `base` on GitHub; a commit GitHub
/// does not know is not contained.
fn contained(repo: &str, base: &str, branch: &str) -> Result<bool> {
    let compare = get(&format!("repos/{repo}/compare/{}...{}", encode_ref(base), encode_ref(branch)))?;
    Ok(compare.is_some_and(|c| matches!(c["status"].as_str(), Some("identical" | "behind"))))
}

/// `(1, 2, 3)` for `v1.2.3` with prefix `v`; only plain releases, no `-beta.1`.
pub(crate) fn version(tag: &str, prefix: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix(prefix)?.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// The highest release tag with the prefix, compared as numbers.
pub(crate) fn latest_release_tag(tags: &[String], prefix: &str) -> Option<String> {
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
    use crate::github::mock;
    use crate::repos::ProductLink;

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

    fn pr(number: u64, ready: bool, conflict: bool, dependabot: bool) -> PullRequest {
        PullRequest {
            number,
            ready,
            conflict,
            dependabot,
            head_sha: format!("h{number}"),
            base: "main".into(),
            gate: if ready { String::new() } else { "Merge-Konflikt".into() },
            ..PullRequest::default()
        }
    }

    fn verdict(number: u64, action: &str, risk: &str) -> Verdict {
        Verdict { number, action: action.into(), risk: risk.into(), bump: String::new(), reason: "ok".into(), superseded_by: None }
    }

    #[test]
    fn the_app_has_the_last_word_on_merges() {
        let mut plan = Plan {
            prs: vec![pr(1, true, false, true), pr(2, true, false, true), pr(3, false, true, true), pr(4, false, true, false), pr(5, true, false, true), pr(6, true, false, true)],
            ..Plan::default()
        };
        apply_review(
            &mut plan,
            &[verdict(1, "merge", "yellow"), verdict(2, "merge", "red"), verdict(3, "merge", "green"), verdict(4, "merge", "green"), verdict(5, "close", "green"), verdict(6, "merge", "green")],
        );
        let action = |n| plan.prs.iter().find(|pr| pr.number == n).unwrap().action;
        assert_eq!(action(1), Action::Merge, "yellow stays possible");
        assert_eq!(action(2), Action::Keep, "red is never merged");
        assert_eq!(action(3), Action::Rebase, "Dependabot rebases a conflict");
        assert_eq!(action(4), Action::Keep, "not ready, not Dependabot");
        assert_eq!(action(5), Action::Close);
        assert_eq!(plan.prs.iter().map(|pr| pr.number).collect::<Vec<_>>(), [3, 4, 5, 6, 1, 2], "green, then yellow, then red");

        let mut unreviewed = Plan { prs: vec![pr(7, true, false, true)], ..Plan::default() };
        apply_review(&mut unreviewed, &[]);
        assert_eq!(unreviewed.prs[0].action, Action::Keep, "no verdict, no merge");
    }

    #[test]
    fn invalid_verdicts_never_merge() {
        let mut plan = Plan { prs: (1..=6).map(|n| pr(n, true, false, true)).collect(), ..Plan::default() };
        let mut empty = verdict(4, "merge", "green");
        empty.reason = "  ".into();
        let mut itself = verdict(5, "close", "green");
        itself.superseded_by = Some(5);
        let problems = apply_review(
            &mut plan,
            &[verdict(1, "merge", "green"), verdict(1, "merge", "green"), verdict(2, "merge", "orange"), verdict(3, "ship", "green"), empty, itself, verdict(6, "merge", ""), verdict(99, "merge", "green")],
        );
        for pr in &plan.prs {
            assert_eq!(pr.action, Action::Keep, "#{}: {}", pr.number, pr.reason);
            assert!(!pr.reviewed);
        }
        assert_eq!(problems.len(), 7, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("#99")));
    }

    fn plan() -> Plan {
        Plan {
            key: "o/app".into(),
            repo: "o/app".into(),
            default_branch: "main".into(),
            base_head: "base".into(),
            merge_method: "merge".into(),
            ..Plan::default()
        }
    }

    fn open_pr(number: u64, head: &str, login: &str, mergeable_state: &str) -> Value {
        json!({
            "number": number, "state": "open", "draft": false, "mergeable": true, "mergeable_state": mergeable_state,
            "user": { "login": login }, "head": { "sha": head }, "base": { "ref": "main", "repo": { "full_name": "o/app" } }
        })
    }

    fn green(sha: &str) -> [(String, Option<Value>); 2] {
        [
            (format!("repos/o/app/commits/{sha}/check-runs?per_page=100"), Some(json!({ "total_count": 1, "check_runs": [{ "name": "build", "status": "completed", "conclusion": "success" }] }))),
            (format!("repos/o/app/commits/{sha}/status?per_page=100"), Some(json!({ "total_count": 0, "statuses": [] }))),
        ]
    }

    fn no_checks(sha: &str) -> [(String, Option<Value>); 2] {
        [
            (format!("repos/o/app/commits/{sha}/check-runs?per_page=100"), Some(json!({ "total_count": 0, "check_runs": [] }))),
            (format!("repos/o/app/commits/{sha}/status?per_page=100"), Some(json!({ "total_count": 0, "statuses": [] }))),
        ]
    }

    fn approved(number: u64) -> PullRequest {
        PullRequest { action: Action::Merge, reviewed: true, risk: Some("green".into()), ..pr(number, true, false, true) }
    }

    #[test]
    fn merge_checks_again_right_before_it_writes() {
        let plan = plan();
        let main = ("repos/o/app/branches/main".to_owned(), Some(json!({ "commit": { "sha": "base" } })));
        let mut state = RunState::new(&plan);

        let mut unsure = approved(1);
        unsure.risk = None;
        mock::install([]);
        assert!(matches!(merge(&plan, &unsure, &mut state).unwrap(), Done::Skipped(_)), "no valid risk, nothing read or written");

        for (current, why) in [
            (open_pr(1, "other", "dependabot[bot]", "clean"), "neue Commits"),
            (json!({ "number": 1, "state": "open", "mergeable": true, "mergeable_state": "clean", "head": { "sha": "h1" }, "base": { "ref": "release", "repo": { "full_name": "o/app" } } }), "zielt jetzt auf release"),
            (open_pr(1, "h1", "dependabot[bot]", "blocked"), "blockiert"),
        ] {
            mock::install([("repos/o/app/pulls/1".to_owned(), Some(current)), main.clone()]);
            match merge(&plan, &approved(1), &mut state).unwrap() {
                Done::Skipped(text) => assert!(text.contains(why), "{text}"),
                Done::Ok(text) => panic!("merged: {text}"),
            }
            assert!(mock::writes().is_empty());
        }

        let red_ci = ("repos/o/app/commits/h1/check-runs?per_page=100".to_owned(), Some(json!({ "total_count": 1, "check_runs": [{ "name": "build", "status": "completed", "conclusion": "failure" }] })));
        mock::install([("repos/o/app/pulls/1".to_owned(), Some(open_pr(1, "h1", "dependabot[bot]", "clean"))), main.clone(), red_ci, green("h1")[1].clone()]);
        assert!(matches!(merge(&plan, &approved(1), &mut state).unwrap(), Done::Skipped(_)), "red CI of the PR head");
        assert!(mock::writes().is_empty());

        let [runs, status] = green("h1");
        mock::install([("repos/o/app/pulls/1".to_owned(), Some(open_pr(1, "h1", "dependabot[bot]", "clean"))), main.clone(), runs, status]);
        mock::reply("PUT repos/o/app/pulls/1/merge", 200, json!({ "sha": "m1" }));
        assert!(matches!(merge(&plan, &approved(1), &mut state).unwrap(), Done::Ok(_)));
        assert_eq!(mock::writes(), ["PUT repos/o/app/pulls/1/merge"]);
        assert_eq!(state.expected_head.as_deref(), Some("m1"));
        assert!(state.merged.contains(&1));
    }

    #[test]
    fn rebase_and_close_check_again_too() {
        let plan = plan();
        let rebase_pr = PullRequest { action: Action::Rebase, ..pr(2, false, true, true) };
        for (current, why) in [
            (open_pr(2, "new", "dependabot[bot]", "dirty"), "neue Commits"),
            (open_pr(2, "h2", "someone", "dirty"), "kein Dependabot"),
            (open_pr(2, "h2", "dependabot[bot]", "clean"), "kein Konflikt mehr"),
        ] {
            mock::install([("repos/o/app/pulls/2".to_owned(), Some(current))]);
            match rebase(&plan, &rebase_pr).unwrap() {
                Done::Skipped(text) => assert!(text.contains(why), "{text}"),
                Done::Ok(text) => panic!("commented: {text}"),
            }
            assert!(mock::writes().is_empty());
        }
        mock::install([("repos/o/app/pulls/2".to_owned(), Some(open_pr(2, "h2", "dependabot[bot]", "dirty")))]);
        assert!(matches!(rebase(&plan, &rebase_pr).unwrap(), Done::Ok(_)));
        assert_eq!(mock::writes(), ["POST repos/o/app/issues/2/comments"]);

        let close_pr = PullRequest { action: Action::Close, superseded_by: Some(9), reason: "von #9 überholt".into(), ..pr(3, true, false, true) };
        mock::install([
            ("repos/o/app/pulls/3".to_owned(), Some(open_pr(3, "h3", "dependabot[bot]", "clean"))),
            ("repos/o/app/pulls/9".to_owned(), Some(json!({ "state": "closed", "merged": false }))),
        ]);
        assert!(matches!(close(&plan, &close_pr).unwrap(), Done::Skipped(text) if text.contains("ohne Merge geschlossen")));
        assert!(mock::writes().is_empty(), "the reason no longer holds");

        mock::answer("repos/o/app/pulls/9", Some(json!({ "state": "open", "merged": false })));
        mock::reply("PATCH repos/o/app/pulls/3", 422, json!({ "message": "nope" }));
        let error = close(&plan, &close_pr).unwrap_err();
        assert!(format!("{error:#}").starts_with("Kommentar gepostet, aber"), "both parts are reported: {error:#}");
        assert_eq!(mock::writes(), ["POST repos/o/app/issues/3/comments", "PATCH repos/o/app/pulls/3"]);
    }

    fn tag_plan() -> Plan {
        Plan { tag: Some("1.0.1".into()), last_tag: Some("1.0.0".into()), ..plan() }
    }

    fn quick() -> CiWait {
        CiWait { no_checks_after: Duration::ZERO, timeout: Duration::ZERO, poll: Duration::ZERO }
    }

    fn tag_world(head: &str) -> Vec<(String, Option<Value>)> {
        vec![
            ("repos/o/app/branches/main".to_owned(), Some(json!({ "commit": { "sha": head } }))),
            ("repos/o/app/tags?per_page=100&page=1".to_owned(), Some(json!([{ "name": "1.0.0" }, { "name": "0.9.0" }]))),
            (format!("repos/o/app/compare/1.0.0...{head}"), Some(json!({ "status": "ahead", "ahead_by": 2 }))),
            ("repos/o/app/git/ref/tags/1.0.1".to_owned(), None),
        ]
    }

    #[test]
    fn tags_only_on_green_ci_of_the_expected_commit() {
        let plan = tag_plan();
        let runner = Runner::default();
        let skipped = |state: &RunState, why: &str| match tag(&plan, state, &runner, &quick(), &|_| {}).unwrap() {
            Done::Skipped(text) => assert!(text.contains(why), "{text}"),
            Done::Ok(text) => panic!("tagged: {text}"),
        };
        let mut state = RunState { expected_head: Some("m2".into()), ..RunState::default() };

        mock::install(tag_world("m2"));
        state.missed = vec![4];
        skipped(&state, "nicht alle geplanten Merges");
        state.missed.clear();

        mock::install(tag_world("other"));
        skipped(&state, "kam nicht aus diesem Lauf");

        let mut world = tag_world("m2");
        world.extend(no_checks("m2"));
        mock::install(world);
        skipped(&state, "keine CI-Checks");

        let mut world = tag_world("m2");
        world.extend([
            ("repos/o/app/commits/m2/check-runs?per_page=100".to_owned(), Some(json!({ "total_count": 1, "check_runs": [{ "name": "build", "status": "queued" }] }))),
            ("repos/o/app/commits/m2/status?per_page=100".to_owned(), Some(json!({ "total_count": 0, "statuses": [] }))),
        ]);
        mock::install(world);
        skipped(&state, "läuft nach 0 Minuten noch");

        let mut world = tag_world("m2");
        world[1].1 = Some(json!([{ "name": "1.0.0" }, { "name": "1.0.5" }]));
        mock::install(world);
        skipped(&state, "inzwischen gibt es 1.0.5");

        let mut world = tag_world("m2");
        world[2].1 = Some(json!({ "status": "diverged", "ahead_by": 2 }));
        mock::install(world);
        skipped(&state, "divergiert");
        assert!(mock::writes().is_empty());

        let mut world = tag_world("m2");
        world.extend(green("m2"));
        mock::install(world.clone());
        let cancelled = Runner::default();
        cancelled.cancel();
        assert!(tag(&plan, &state, &cancelled, &quick(), &|_| {}).is_err(), "a cancel stops before the tag");
        assert!(mock::writes().is_empty());

        mock::install(world.clone());
        assert!(matches!(tag(&plan, &state, &runner, &quick(), &|_| {}).unwrap(), Done::Ok(_)));
        assert_eq!(mock::writes(), ["POST repos/o/app/git/refs"]);

        world[3].1 = Some(json!({ "object": { "sha": "elsewhere" } }));
        mock::install(world);
        skipped(&state, "gibt es inzwischen");
        assert!(mock::writes().is_empty(), "an existing tag is never moved");
    }

    #[test]
    fn remote_branches_go_only_while_the_proof_holds() {
        let plan = plan();
        let mut state = RunState::new(&plan);
        let branch = |proof| Branch { name: "feature/x".into(), sha: "f1".into(), reason: String::new(), proof };
        let world = |sha: &str, protected: bool, open: Value| {
            vec![
                ("repos/o/app/branches/feature/x".to_owned(), Some(json!({ "commit": { "sha": sha }, "protected": protected }))),
                ("repos/o/app".to_owned(), Some(json!({ "default_branch": "main" }))),
                ("repos/o/app/pulls?state=open&head=o:feature/x&per_page=100&page=1".to_owned(), Some(open)),
                ("repos/o/app/pulls?state=open&base=feature/x&per_page=100&page=1".to_owned(), Some(json!([]))),
                ("repos/o/app/compare/main...feature/x".to_owned(), Some(json!({ "status": "behind" }))),
                ("repos/o/app/pulls/7".to_owned(), Some(json!({ "merged": true, "head": { "sha": "f1" } }))),
            ]
        };
        let skipped = |b: &Branch, state: &RunState, why: &str| match delete_branch(&plan, b, state).unwrap() {
            Done::Skipped(text) => assert!(text.contains(why), "{text}"),
            Done::Ok(text) => panic!("deleted: {text}"),
        };
        mock::install(world("f1", false, json!([])));
        skipped(&branch(Proof::AfterMerge(5)), &state, "nicht gemergt");
        mock::install(world("f2", false, json!([])));
        skipped(&branch(Proof::Contained), &state, "neue Commits");
        mock::install(world("f1", true, json!([])));
        skipped(&branch(Proof::Contained), &state, "geschützt");
        mock::install(world("f1", false, json!([{ "number": 12 }])));
        skipped(&branch(Proof::Contained), &state, "#12");
        let mut gone = world("f1", false, json!([]));
        gone[4].1 = Some(json!({ "status": "diverged" }));
        mock::install(gone);
        skipped(&branch(Proof::Contained), &state, "nicht mehr vollständig");
        assert!(mock::writes().is_empty());

        mock::install(world("f1", false, json!([])));
        assert!(matches!(delete_branch(&plan, &branch(Proof::MergedPr(7)), &state).unwrap(), Done::Ok(_)));
        state.merged.insert(5);
        assert!(matches!(delete_branch(&plan, &branch(Proof::AfterMerge(5)), &state).unwrap(), Done::Ok(_)));
        assert_eq!(mock::writes(), ["DELETE repos/o/app/git/refs/heads/feature/x", "DELETE repos/o/app/git/refs/heads/feature/x"]);
    }

    /// A local clone of `o/app` with `main`, `origin/main` and a few branches.
    fn clone(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("orp-repocare-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(dir.with_extension("wt"));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| output("git", args, &dir).unwrap();
        git(&["init", "--quiet", "--initial-branch=main"]);
        git(&["remote", "add", "origin", "https://github.com/o/app.git"]);
        let commit = |message: &str| git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "--allow-empty", "-m", message]);
        commit("one");
        git(&["branch", "merged"]);
        git(&["branch", "busy"]);
        commit("two");
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&["checkout", "--quiet", "-b", "open"]);
        commit("three");
        git(&["checkout", "--quiet", "main"]);
        git(&["worktree", "add", "--quiet", &dir.with_extension("wt").display().to_string(), "busy"]);
        dir
    }

    fn target_with(dir: &Path) -> RepoTarget {
        RepoTarget {
            key: "o/app".into(),
            repo: "o/app".into(),
            products: vec![ProductLink { id: "app".into(), name: "App".into(), documented: "1.0.0".into(), tag_prefix: String::new(), local_path: None, enabled: false }],
            tag_prefix: Some(String::new()),
            tag_conflict: None,
            local: LocalCopy::Found { path: dir.display().to_string() },
            website: false,
        }
    }

    #[test]
    fn the_preview_reads_and_leaves_the_clone_alone() {
        let dir = clone("scan");
        mock::install([
            ("repos/o/app".to_owned(), Some(json!({ "default_branch": "main", "permissions": { "push": true } }))),
            ("repos/o/app/branches/main".to_owned(), Some(json!({ "commit": { "sha": "newer-on-github" } }))),
            ("repos/o/app/pulls?state=open&per_page=100&page=1".to_owned(), Some(json!([]))),
            ("repos/o/app/pulls?state=closed&sort=updated&direction=desc&per_page=100&page=1".to_owned(), Some(json!([]))),
            ("repos/o/app/branches?per_page=100&page=1".to_owned(), Some(json!([{ "name": "main", "commit": { "sha": "x" } }]))),
        ]);
        crate::shell::RAN.with(|ran| ran.borrow_mut().clear());
        let plan = scan(&target_with(&dir), false, true);
        assert_eq!(plan.error, None);
        let names: Vec<&str> = plan.local_branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["merged"], "busy is checked out in a worktree, open is not merged");
        assert!(plan.local_sync.is_some(), "origin/main differs from GitHub: the run fetches first");
        let ran = crate::shell::RAN.with(|ran| ran.borrow().clone());
        assert!(ran.iter().all(|c| !c.contains("fetch") && !c.contains("branch -D") && !c.contains("checkout")), "{ran:?}");
        assert!(mock::writes().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(dir.with_extension("wt"));
    }

    /// GitHub's answer whether `sha` is contained in `main`; `None` for a commit GitHub does not know.
    fn on_github(sha: &str, status: Option<&str>) -> (String, Option<Value>) {
        (format!("repos/o/app/compare/main...{sha}"), status.map(|s| json!({ "status": s })))
    }

    #[test]
    fn local_branches_go_only_while_the_proof_holds() {
        let dir = clone("delete");
        let plan = Plan { clone: Some(dir.display().to_string()), ..plan() };
        let state = RunState::new(&plan);
        let sha = |name: &str| output("git", &["rev-parse", name], &dir).unwrap();
        let branch = |name: &str, sha: String| Branch { name: name.into(), sha, reason: String::new(), proof: Proof::Contained };
        mock::install([on_github(&sha("merged"), Some("behind")), on_github(&sha("open"), Some("ahead"))]);
        let busy = branch("busy", sha("busy"));
        assert!(matches!(delete_local_branch(&plan, &busy, &state).unwrap(), Done::Skipped(t) if t.contains("Worktree")));
        let open = branch("open", sha("open"));
        assert!(matches!(delete_local_branch(&plan, &open, &state).unwrap(), Done::Skipped(t) if t.contains("nicht mehr vollständig")));
        let moved = branch("merged", sha("open"));
        assert!(matches!(delete_local_branch(&plan, &moved, &state).unwrap(), Done::Skipped(t) if t.contains("neue Commits")));
        let other = Plan { repo: "o/other".into(), ..plan.clone() };
        let merged = branch("merged", sha("merged"));
        assert!(matches!(delete_local_branch(&other, &merged, &state).unwrap(), Done::Skipped(t) if t.contains("gehört nicht mehr")));
        assert!(matches!(delete_local_branch(&plan, &merged, &state).unwrap(), Done::Ok(_)));
        assert!(output("git", &["rev-parse", "--verify", "--quiet", "refs/heads/merged"], &dir).is_err());
        assert!(matches!(delete_local_branch(&plan, &merged, &state).unwrap(), Done::Skipped(t) if t.contains("schon gelöscht")), "a repeat does nothing");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(dir.with_extension("wt"));
    }

    #[test]
    fn a_stale_local_view_of_github_deletes_nothing() {
        let dir = clone("stale");
        let sha = output("git", &["rev-parse", "merged"], &dir).unwrap();
        let merged = Branch { name: "merged".into(), sha: sha.clone(), reason: String::new(), proof: Proof::Contained };
        let plan = Plan { clone: Some(dir.display().to_string()), ..plan() };
        let state = RunState::new(&plan);
        let skipped = |plan: &Plan, state: &RunState, why: &str| match delete_local_branch(plan, &merged, state).unwrap() {
            Done::Skipped(text) => assert!(text.contains(why), "{text}"),
            Done::Ok(text) => panic!("deleted: {text}"),
        };
        // The clone's origin/main contains the branch, but main on GitHub was rewritten without it.
        mock::install([on_github(&sha, Some("diverged"))]);
        skipped(&plan, &state, "auf GitHub");
        mock::install([on_github(&sha, None)]);
        skipped(&plan, &state, "auf GitHub");

        // The preview announced an update of the clone; it did not happen, so nothing goes.
        let announced = Plan { local_sync: Some("origin/main steht lokal auf a, auf GitHub auf b".into()), ..plan.clone() };
        mock::install([on_github(&sha, Some("behind"))]);
        skipped(&announced, &state, "nicht abgeglichen");
        let synced = RunState { local_synced: true, ..RunState::new(&announced) };
        assert!(matches!(delete_local_branch(&announced, &merged, &synced).unwrap(), Done::Ok(_)));
        assert!(mock::writes().is_empty(), "local deletes never write to GitHub");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(dir.with_extension("wt"));
    }

    #[test]
    fn the_release_line_is_checked_again_after_waiting_for_ci() {
        let plan = tag_plan();
        let state = RunState { expected_head: Some("m2".into()), ..RunState::default() };
        let runner = Runner::default();
        let skipped = |why: &str| match tag(&plan, &state, &runner, &quick(), &|_| {}).unwrap() {
            Done::Skipped(text) => assert!(text.contains(why) && text.contains("nach dem Warten"), "{text}"),
            Done::Ok(text) => panic!("tagged: {text}"),
        };
        let mut world = tag_world("m2");
        world.extend(green("m2"));

        // While the CI ran, someone released 1.0.5: 1.0.1 would go below it.
        mock::install(world.clone());
        mock::sequence("repos/o/app/tags?per_page=100&page=1", vec![Some(json!([{ "name": "1.0.0" }])), Some(json!([{ "name": "1.0.0" }, { "name": "1.0.5" }]))]);
        skipped("inzwischen gibt es 1.0.5");

        // 1.0.0 was moved to another commit while the CI ran: the base is no longer an ancestor.
        mock::install(world);
        mock::sequence("repos/o/app/compare/1.0.0...m2", vec![Some(json!({ "status": "ahead", "ahead_by": 2 })), Some(json!({ "status": "diverged" }))]);
        skipped("divergiert");
        assert!(mock::writes().is_empty());
    }

    #[test]
    fn a_preview_runs_once_and_only_for_what_it_was_made_for() {
        let target = target_with(Path::new("/nowhere/app"));
        let set = |id| PlanSet {
            id,
            project: "~/site".into(),
            created_at: 0,
            merge: true,
            cleanup: true,
            model: String::new(),
            targets: vec![(target.key.clone(), target.fingerprint())],
            plans: vec![plan()],
        };
        assert_eq!(set(1).check_binding("~/site", &[target.clone()]), Ok(()));
        assert!(set(1).check_binding("~/other", &[target.clone()]).is_err(), "project switched");
        let changed = RepoTarget { tag_prefix: Some("v".into()), ..target.clone() };
        assert!(set(1).check_binding("~/site", &[changed]).is_err(), "tag prefix changed in site.json");
        assert!(set(1).check_binding("~/site", &[]).is_err(), "repository no longer a product");

        let mut slot = None;
        assert!(store_plan(&mut slot, "~/other", set(1)).is_err(), "a late answer for another project");
        assert!(slot.is_none());
        store_plan(&mut slot, "~/site", set(2)).unwrap();
        assert!(take_plan(&mut slot, 1).is_err(), "an older preview's id");
        assert!(slot.is_some(), "the newer one stays");
        assert_eq!(take_plan(&mut slot, 2).unwrap().id, 2);
        assert!(take_plan(&mut slot, 2).is_err(), "a second click runs nothing");
    }

    fn own_target() -> RepoTarget {
        RepoTarget { local: LocalCopy::NotConfigured, repo: crate::update::REPOSITORY.into(), key: crate::update::REPOSITORY.to_lowercase(), ..target_with(Path::new("/")) }
    }

    /// `cargo test -p openreleasepilot-core scans_a_real_repository -- --ignored --nocapture`:
    /// the facts for this app's own repository; reads only, no local clone, no Claude.
    #[test]
    #[ignore = "network"]
    fn scans_a_real_repository() {
        let mut plan = scan(&own_target(), true, true);
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
        let mut plan = scan(&own_target(), true, false);
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
        println!("problems {:?}", apply_review(&mut plan, &review.prs));
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
