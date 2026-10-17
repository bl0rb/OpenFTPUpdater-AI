//! Care jobs ("Aufträge"): what the user started for which repositories and targets, what the app
//! did, and what still needs a decision. A job is a list of steps with dependencies:
//!
//! - `care`: read the repositories, let Claude judge the pull requests, then the plan waits for
//!   approval; after it, the approved items run one by one (pull requests, then branches).
//! - `release:<repo>`: the planned patch tag once the CI of its commit passed, then (if the release
//!   rule says so) the wait for the published GitHub release.
//! - `target:<id>`: a README/documentation or wiki change, prepared, shown, approved, published.
//! - `website`: the website's update, shown as its commit, approved, uploaded and verified.
//!
//! Each job is a file of its own, written atomically after every change, so a restart forgets no
//! result and starts nothing by itself. A write that had begun when the app stopped is marked
//! (`writing`) and resolved from the remote state before anything else happens.

use crate::config::Config;
use crate::content::{self, Basis, Proposal, TargetKind};
use crate::paths;
use crate::repocare::{Action, PlanSet, RunState};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

/// Finished jobs kept for the history; older ones are removed.
pub const KEEP_FINISHED: usize = 50;
const MAX_LOG_LINES: usize = 20_000;

// MARK: Model

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct JobOptions {
    /// Review and merge/close/rebase pull requests.
    pub prs: bool,
    /// Delete merged branches.
    pub cleanup: bool,
    /// Patch tag (and published release) where a release rule exists.
    pub release: bool,
    /// Content targets, each with the basis chosen for this job.
    pub targets: Vec<TargetChoice>,
    /// Update, publish and verify the website as the last step.
    pub website: bool,
    /// Model alias for Claude's runs; empty = the app's default.
    pub model: String,
    /// Extra instruction for the content and website runs.
    pub instructions: String,
}

impl JobOptions {
    pub fn care(&self) -> bool {
        self.prs || self.cleanup || self.release
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TargetChoice {
    pub id: String,
    pub basis: Basis,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StepKind {
    Care,
    Release,
    Docs,
    Wiki,
    Website,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StepState {
    /// Not started yet.
    Pending,
    Running,
    /// A proposal waits for the user.
    NeedsApproval,
    /// Approved, not carried out yet.
    Approved,
    /// Waiting for something outside the app (a release); asking again may finish it.
    Waiting,
    Done,
    NothingToDo,
    /// Some of its items done, others skipped or failed.
    Partial,
    /// Not done because a step it depends on did not finish well.
    Skipped,
    Failed,
    /// The user said no.
    Rejected,
    /// Cannot go on without the user: a conflict, an interrupted write, a changed precondition.
    Blocked,
    Cancelled,
}

impl StepState {
    /// Finished in a way that lets dependent steps go on.
    pub fn lets_go_on(self) -> bool {
        matches!(self, StepState::Done | StepState::NothingToDo | StepState::Partial | StepState::Rejected)
    }

    /// Finished in a way that stops dependent steps.
    pub fn stops_dependents(self) -> bool {
        matches!(self, StepState::Failed | StepState::Skipped | StepState::Cancelled)
    }

    pub fn finished(self) -> bool {
        self.lets_go_on() || self.stops_dependents()
    }
}

/// The website's change as the user approves it: the local commit the update made.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WebsiteProposal {
    /// HEAD before the update and the commit it made.
    pub base: String,
    pub commit: String,
    pub stat: String,
    pub diff: String,
    pub summary: String,
    /// The products it covers, e.g. `App 1.2.0 → 1.3.0`.
    pub products: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Link {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStep {
    /// `care`, `release:<repo>`, `target:<id>`, `website`.
    pub id: String,
    pub kind: StepKind,
    pub label: String,
    /// Release: the repository key. Docs/wiki: the target id.
    #[serde(default)]
    pub subject: String,
    /// Steps that must have finished first.
    #[serde(default)]
    pub after: Vec<String>,
    pub state: StepState,
    /// Why it is in its state, or what it did.
    #[serde(default)]
    pub detail: String,
    /// Docs/wiki: the basis chosen for this job.
    #[serde(default)]
    pub basis: Option<Basis>,
    #[serde(default)]
    pub content: Option<Proposal>,
    #[serde(default)]
    pub website: Option<WebsiteProposal>,
    /// Fingerprint of the proposal the user approved.
    #[serde(default)]
    pub approved: Option<String>,
    /// What it produced: pull request, tag, release, commit.
    #[serde(default)]
    pub links: Vec<Link>,
    /// A write had started and its result is not recorded yet.
    #[serde(default)]
    pub writing: Option<String>,
    /// Release: the tag and its commit. Wiki: the commit about to be pushed.
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    /// Docs: the pull request the step created.
    #[serde(default)]
    pub pr: Option<u64>,
    #[serde(default)]
    pub finished_at: Option<u64>,
}

impl JobStep {
    fn new(id: String, kind: StepKind, label: String, subject: String, after: Vec<String>) -> Self {
        Self {
            id,
            kind,
            label,
            subject,
            after,
            state: StepState::Pending,
            detail: String::new(),
            basis: None,
            content: None,
            website: None,
            approved: None,
            links: Vec::new(),
            writing: None,
            tag: None,
            commit: None,
            pr: None,
            finished_at: None,
        }
    }

    pub fn set(&mut self, state: StepState, detail: impl Into<String>, now: u64) {
        self.state = state;
        self.detail = detail.into();
        if state.finished() {
            self.finished_at = Some(now);
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ItemKind {
    Close,
    Merge,
    Rebase,
    Branch,
    LocalSync,
    LocalBranch,
    /// Only for leaving a planned tag out; the tag itself is the release step.
    Tag,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ItemOutcome {
    Done,
    Skipped,
    Failed,
    Cancelled,
    /// Left out by the user before approving.
    Excluded,
    /// Not carried out because GitHub or the clone already is what it would make it (a branch that
    /// was deleted already). The goal is reached; nothing is left to do.
    AlreadyDone,
}

/// One action of the care plan and what became of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CareItem {
    /// `<repo>|merge|12`, `<repo>|branch|feature/x`, …
    pub id: String,
    pub repo: String,
    pub kind: ItemKind,
    /// PR number or branch name.
    pub reference: String,
    pub label: String,
    #[serde(default)]
    pub context: String,
    #[serde(default)]
    pub outcome: Option<ItemOutcome>,
    #[serde(default)]
    pub detail: String,
    /// The write had started; resolved from GitHub's state before anything else.
    #[serde(default)]
    pub writing: bool,
    /// Close: the comment is posted already, only the close is missing.
    #[serde(default)]
    pub commented: bool,
    /// Offered for the user to choose (a merge Claude advised against, see `PullRequest::optional`);
    /// it only runs when the approval names it.
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub created_at: u64,
    pub updated_at: u64,
    /// Keys of the repositories, each once.
    pub repos: Vec<String>,
    pub options: JobOptions,
    pub steps: Vec<JobStep>,
    #[serde(default)]
    pub plan: Option<PlanSet>,
    #[serde(default)]
    pub items: Vec<CareItem>,
    /// Per repository: the merges of this job and where the default branch should be.
    #[serde(default)]
    pub run_states: BTreeMap<String, RunState>,
    /// Ended by the user; nothing runs any more.
    #[serde(default)]
    pub ended: Option<String>,
    /// A pass was running when the app stopped.
    #[serde(default)]
    pub interrupted: bool,
}

/// The state of a job as a whole, derived from its steps.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum JobState {
    Planned,
    Preparing,
    NeedsApproval,
    Running,
    Waiting,
    Succeeded,
    Partial,
    Blocked,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn finished(self) -> bool {
        matches!(self, JobState::Succeeded | JobState::Partial | JobState::Failed | JobState::Cancelled)
    }
}

/// State, the reason for it, and the next sensible action.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub state: JobState,
    pub reason: String,
    pub next: String,
}

// MARK: Making a job

/// What a job can be made of: the repositories with their release rules, the targets, and which
/// repositories feed the website.
pub struct Scope<'a> {
    pub config: &'a Config,
    /// Keys of repositories the website's selected products come from; `None` without a website.
    pub website_sources: Option<Vec<String>>,
}

/// The steps for the options, each target and the website once; dependencies point backwards only,
/// so nothing can wait for itself.
pub fn make(id: &str, now: u64, repos: &[String], options: &JobOptions, scope: &Scope) -> Result<Job, String> {
    let mut keys: Vec<String> = Vec::new();
    for key in repos.iter().map(|k| k.trim().to_lowercase()) {
        let managed = scope.config.repository(&key).ok_or_else(|| format!("{key} wird nicht verwaltet"))?;
        if !managed.enabled {
            return Err(format!("{} ist deaktiviert", managed.repo));
        }
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if keys.is_empty() {
        return Err("Kein Repository gewählt".into());
    }
    let mut steps = Vec::new();
    if options.care() {
        steps.push(JobStep::new("care".into(), StepKind::Care, "Pflegeplan".into(), String::new(), Vec::new()));
    }
    let care: Vec<String> = steps.iter().map(|s| s.id.clone()).collect();
    let mut releases = Vec::new();
    if options.release {
        for key in &keys {
            let managed = scope.config.repository(key).expect("checked above");
            if managed.release.is_some() {
                let id = format!("release:{key}");
                steps.push(JobStep::new(id.clone(), StepKind::Release, format!("Release {}", managed.repo), key.clone(), care.clone()));
                releases.push((key.clone(), id));
            }
        }
        if releases.is_empty() {
            return Err("Keins der Repositories hat eine Release-Regel; Release abwählen oder eine Regel einrichten".into());
        }
    }
    let after_sources = |sources: &[String]| -> Vec<String> {
        let mut after = care.clone();
        after.extend(releases.iter().filter(|(key, _)| sources.contains(key)).map(|(_, id)| id.clone()));
        after
    };
    let managed_keys: Vec<String> = scope.config.repositories.iter().map(|r| r.key()).collect();
    let mut seen = Vec::new();
    for choice in &options.targets {
        if seen.contains(&choice.id) {
            continue;
        }
        seen.push(choice.id.clone());
        let target = scope.config.content_target(&choice.id).ok_or_else(|| format!("Ziel {} gibt es nicht mehr", choice.id))?;
        content::validate(target, &managed_keys).map_err(|why| format!("{}: {why}", target.name))?;
        if !target.sources.iter().any(|s| keys.contains(s)) {
            return Err(format!("{}: keine seiner Quellen ist ausgewählt", target.name));
        }
        let kind = match target.kind {
            TargetKind::Docs => StepKind::Docs,
            TargetKind::Wiki => StepKind::Wiki,
        };
        let mut step = JobStep::new(format!("target:{}", target.id), kind, format!("{}: {}", target.kind_name(), target.name), target.id.clone(), after_sources(&target.sources));
        step.basis = Some(choice.basis);
        steps.push(step);
    }
    if options.website {
        let sources = scope.website_sources.as_ref().ok_or("Keine Website eingerichtet")?;
        if !sources.iter().any(|s| keys.contains(s)) {
            return Err("Keins der ausgewählten Repositories speist die Website (ausgewählte Produkte in site.json)".into());
        }
        steps.push(JobStep::new("website".into(), StepKind::Website, "Website".into(), String::new(), after_sources(sources)));
    }
    if steps.is_empty() {
        return Err("Keine Tätigkeit gewählt".into());
    }
    Ok(Job {
        id: id.to_owned(),
        created_at: now,
        updated_at: now,
        repos: keys,
        options: options.clone(),
        steps,
        plan: None,
        items: Vec::new(),
        run_states: BTreeMap::new(),
        ended: None,
        interrupted: false,
    })
}

/// A new job may not touch what an open job holds: the same repository, target or website.
pub fn overlap(open: &[&Job], repos: &[String], options: &JobOptions) -> Option<String> {
    for job in open {
        if let Some(repo) = repos.iter().map(|r| r.to_lowercase()).find(|r| job.repos.contains(r)) {
            return Some(format!("{repo} gehört schon zum offenen Auftrag vom {}; erst abschließen oder beenden", job.id));
        }
        if let Some(target) = options.targets.iter().find(|t| job.options.targets.iter().any(|o| o.id == t.id)) {
            return Some(format!("Ziel {} gehört schon zu einem offenen Auftrag", target.id));
        }
        if options.website && job.options.website {
            return Some("Die Website gehört schon zu einem offenen Auftrag".into());
        }
    }
    None
}

// MARK: Progress

impl Job {
    pub fn step(&self, id: &str) -> Option<&JobStep> {
        self.steps.iter().find(|s| s.id == id)
    }

    pub fn step_mut(&mut self, id: &str) -> Option<&mut JobStep> {
        self.steps.iter_mut().find(|s| s.id == id)
    }

    fn state_of(&self, id: &str) -> Option<StepState> {
        self.step(id).map(|s| s.state)
    }

    /// Steps whose dependency stopped become skipped, with the reason. Returns whether any changed.
    pub fn propagate(&mut self, now: u64) -> bool {
        let mut changed = false;
        loop {
            let mut skip = None;
            for (i, step) in self.steps.iter().enumerate() {
                if step.state != StepState::Pending {
                    continue;
                }
                if let Some(dep) = step.after.iter().find(|d| self.state_of(d).is_some_and(StepState::stops_dependents)) {
                    let label = self.step(dep).map(|s| s.label.clone()).unwrap_or_default();
                    skip = Some((i, format!("nicht ausgeführt: {label} ist nicht gelungen")));
                    break;
                }
            }
            match skip {
                Some((i, why)) => {
                    self.steps[i].set(StepState::Skipped, why, now);
                    changed = true;
                }
                None => return changed,
            }
        }
    }

    /// The next step that can run now: pending or approved, all its dependencies finished well.
    pub fn next_runnable(&self) -> Option<usize> {
        if self.ended.is_some() {
            return None;
        }
        self.steps.iter().position(|step| {
            matches!(step.state, StepState::Pending | StepState::Approved) && step.after.iter().all(|d| self.state_of(d).is_some_and(StepState::lets_go_on))
        })
    }

    /// Before a pass that the user started: what waits for the outside is asked again.
    pub fn resume(&mut self) {
        for step in &mut self.steps {
            if step.state == StepState::Waiting {
                step.state = StepState::Approved;
            }
        }
    }

    /// After a restart: a step that was running had stopped half-way. Its write, if one had begun,
    /// stays marked for resolution; otherwise it simply starts again.
    pub fn mark_interrupted(&mut self) -> bool {
        let mut any = false;
        for step in &mut self.steps {
            if step.state == StepState::Running {
                any = true;
                step.state = if step.approved.is_some() || (step.kind == StepKind::Care && self.plan.is_some()) || step.kind == StepKind::Release {
                    StepState::Approved
                } else {
                    StepState::Pending
                };
                step.detail = "unterbrochen: die App wurde während des Schritts beendet".into();
            }
        }
        if any || self.items.iter().any(|i| i.writing) || self.steps.iter().any(|s| s.writing.is_some()) {
            self.interrupted = true;
        }
        self.interrupted
    }

    pub fn status(&self, running: bool) -> Status {
        let status = |state, reason: &str, next: &str| Status { state, reason: reason.to_owned(), next: next.to_owned() };
        if let Some(why) = &self.ended {
            let done = self.steps.iter().filter(|s| matches!(s.state, StepState::Done | StepState::Partial)).count();
            let what = if done > 0 { format!("{why}; {done} Schritt(e) vorher erledigt, nichts zurückgenommen") } else { why.clone() };
            return status(JobState::Cancelled, &what, "");
        }
        if running {
            let current = self.steps.iter().find(|s| s.state == StepState::Running);
            let preparing = current.is_some_and(|s| s.approved.is_none() && !matches!(s.kind, StepKind::Release) && !(s.kind == StepKind::Care && self.plan.is_some()));
            let label = current.map(|s| s.label.as_str()).unwrap_or("Auftrag");
            return if preparing {
                status(JobState::Preparing, &format!("{label}: wird vorbereitet"), "")
            } else {
                status(JobState::Running, &format!("{label}: läuft"), "")
            };
        }
        let pending: Vec<&JobStep> = self.steps.iter().filter(|s| s.state == StepState::NeedsApproval).collect();
        if !pending.is_empty() {
            let names: Vec<&str> = pending.iter().map(|s| s.label.as_str()).collect();
            return status(JobState::NeedsApproval, &format!("Freigabe nötig: {}", names.join(", ")), "Vorschlag prüfen und freigeben oder verwerfen");
        }
        if self.interrupted {
            return status(JobState::Blocked, "Unterbrochen: die App wurde während des Auftrags beendet", "Fortsetzen: die App prüft zuerst den Stand bei GitHub");
        }
        if let Some(step) = self.steps.iter().find(|s| s.state == StepState::Blocked) {
            return status(JobState::Blocked, &format!("{}: {}", step.label, step.detail), "Ursache beheben, dann neu vorbereiten oder fortsetzen");
        }
        if let Some(step) = self.steps.iter().find(|s| s.state == StepState::Waiting) {
            return status(JobState::Waiting, &format!("{}: {}", step.label, step.detail), "Später erneut prüfen");
        }
        if self.next_runnable().is_some() {
            return status(JobState::Planned, "Bereit zum Fortsetzen", "Fortsetzen");
        }
        let count = |states: &[StepState]| self.steps.iter().filter(|s| states.contains(&s.state)).count();
        let good = count(&[StepState::Done, StepState::NothingToDo]);
        let failed = count(&[StepState::Failed]);
        let partial = count(&[StepState::Partial, StepState::Skipped]);
        let rejected = count(&[StepState::Rejected]);
        let note = if rejected > 0 { format!(" ({rejected} verworfen)") } else { String::new() };
        if failed > 0 && good == 0 && partial == 0 {
            return status(JobState::Failed, &format!("Fehlgeschlagen{note}"), "Log und Ergebnisse ansehen; für das Übrige einen neuen Auftrag starten");
        }
        if failed > 0 || partial > 0 {
            return status(
                JobState::Partial,
                &format!("Teilweise erfolgreich: {good} erledigt, {partial} teilweise oder übersprungen, {failed} fehlgeschlagen{note}"),
                "Ergebnisse ansehen; für das Übrige einen neuen Auftrag starten",
            );
        }
        status(JobState::Succeeded, &format!("Erfolgreich{note}"), "")
    }

    /// Whether it still holds anything: not finished and not ended.
    pub fn open(&self) -> bool {
        self.ended.is_none() && !self.status(false).state.finished()
    }
}

// MARK: The care plan's items

/// The care step's state and summary from what became of its items: what was done, what already
/// was as planned (that needs no repeat), what was skipped or failed (that does).
pub fn care_result(items: &[CareItem]) -> (StepState, String) {
    let counted: Vec<&CareItem> = items.iter().filter(|i| i.kind != ItemKind::Tag && i.outcome != Some(ItemOutcome::Excluded)).collect();
    let count = |o: ItemOutcome| counted.iter().filter(|i| i.outcome == Some(o)).count();
    let (done, already, skipped, failed) =
        (count(ItemOutcome::Done), count(ItemOutcome::AlreadyDone), count(ItemOutcome::Skipped), count(ItemOutcome::Failed));
    let of = |kind: ItemKind, outcome: ItemOutcome| counted.iter().filter(|i| i.kind == kind && i.outcome == Some(outcome)).count();
    // PRs on GitHub, MRs on GitLab; a job with both says both.
    let gitlab = counted.iter().filter(|i| crate::gitlab::project(&i.repo).is_some()).count();
    let word = match gitlab {
        0 => "PR",
        n if n == counted.len() => "MR",
        _ => "PR/MR",
    };
    let mut parts = Vec::new();
    for (kind, one, many) in [
        (ItemKind::Merge, format!("{word} gemergt"), format!("{word}s gemergt")),
        (ItemKind::Close, format!("{word} geschlossen"), format!("{word}s geschlossen")),
        (ItemKind::Rebase, "Rebase angestoßen".to_owned(), "Rebases angestoßen".to_owned()),
        (ItemKind::Branch, "Branch gelöscht".to_owned(), "Branches gelöscht".to_owned()),
        (ItemKind::LocalBranch, "lokaler Branch gelöscht".to_owned(), "lokale Branches gelöscht".to_owned()),
    ] {
        let n = of(kind, ItemOutcome::Done);
        if n > 0 {
            parts.push(format!("{n} {}", if n == 1 { one } else { many }));
        }
    }
    let gone = of(ItemKind::Branch, ItemOutcome::AlreadyDone) + of(ItemKind::LocalBranch, ItemOutcome::AlreadyDone);
    for (n, what) in [(gone, "bereits gelöscht"), (already - gone, "bereits erledigt"), (skipped, "übersprungen"), (failed, "fehlgeschlagen")] {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    }
    let summary = if parts.is_empty() { "nichts ausgeführt".to_owned() } else { parts.join(" · ") };
    let state = if counted.is_empty() {
        StepState::NothingToDo
    } else if failed > 0 && done + already == 0 {
        StepState::Failed
    } else if failed + skipped > 0 {
        StepState::Partial
    } else if done == 0 {
        StepState::NothingToDo
    } else {
        StepState::Done
    };
    (state, summary)
}

/// The actions of a reviewed plan, in the order the run carries them out: per repository the pull
/// requests (close, merge, rebase), then the branches on GitHub, the clone's update and its branches.
pub fn care_items(set: &PlanSet) -> Vec<CareItem> {
    let mut items = Vec::new();
    for plan in set.plans.iter().filter(|p| p.error.is_none()) {
        let item = |kind: ItemKind, reference: String, label: String, context: String| CareItem {
            id: format!("{}|{}|{reference}", plan.key, kind_key(kind)),
            repo: plan.key.clone(),
            kind,
            reference,
            label,
            context,
            outcome: None,
            detail: String::new(),
            writing: false,
            commented: false,
            optional: false,
        };
        for (action, kind) in [(Action::Close, ItemKind::Close), (Action::Merge, ItemKind::Merge), (Action::Rebase, ItemKind::Rebase)] {
            for pr in plan.prs.iter().filter(|pr| pr.action == action) {
                items.push(item(kind, pr.number.to_string(), crate::repocare::request_ref(&plan.repo, pr.number), pr.title.clone()));
            }
        }
        for pr in plan.prs.iter().filter(|pr| pr.action == Action::Keep && pr.optional) {
            items.push(CareItem { optional: true, ..item(ItemKind::Merge, pr.number.to_string(), crate::repocare::request_ref(&plan.repo, pr.number), pr.title.clone()) });
        }
        if let Some(tag) = &plan.tag {
            items.push(item(ItemKind::Tag, tag.clone(), format!("Tag {tag}"), "im Release-Schritt".into()));
        }
        for branch in &plan.branches {
            items.push(item(ItemKind::Branch, branch.name.clone(), format!("Branch {}", branch.name), branch.reason.clone()));
        }
        if !plan.local_branches.is_empty() && plan.local_sync.is_some() {
            items.push(item(ItemKind::LocalSync, String::new(), "Lokale Kopie abgleichen".into(), plan.local_sync.clone().unwrap_or_default()));
        }
        for branch in &plan.local_branches {
            items.push(item(ItemKind::LocalBranch, branch.name.clone(), format!("Lokal {}", branch.name), branch.reason.clone()));
        }
    }
    items
}

fn kind_key(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Close => "close",
        ItemKind::Merge => "merge",
        ItemKind::Rebase => "rebase",
        ItemKind::Branch => "branch",
        ItemKind::LocalSync => "sync",
        ItemKind::LocalBranch => "local",
        ItemKind::Tag => "tag",
    }
}

/// Approves the plan with this id: the items the user left out do not run, and of the merges the
/// plan offered for choice (`CareItem::optional`) only those in `included` do. The window cannot add
/// anything else: every id must be one of the plan's items, every included one an offered one. A
/// left-out tag is removed from the plan itself, and so is a tag that rested on merges none of which
/// remains.
pub fn approve_plan(job: &mut Job, plan_id: u64, excluded: &[String], included: &[String], now: u64) -> Result<(), String> {
    let set = job.plan.as_mut().ok_or("Kein Pflegeplan vorhanden")?;
    if set.id != plan_id {
        return Err("Dieser Plan gilt nicht mehr; es gibt einen neueren".into());
    }
    let step = job.steps.iter().find(|s| s.id == "care").ok_or("Auftrag ohne Pflegeschritt")?;
    if step.state != StepState::NeedsApproval {
        return Err("Der Pflegeplan wartet nicht auf eine Freigabe".into());
    }
    if let Some(unknown) = excluded.iter().find(|id| !job.items.iter().any(|item| &item.id == *id)) {
        return Err(format!("{unknown} gehört nicht zu diesem Plan"));
    }
    if let Some(unknown) = included.iter().find(|id| !job.items.iter().any(|item| &item.id == *id && item.optional)) {
        return Err(format!("{unknown} steht nicht zur Wahl"));
    }
    for item in job.items.iter_mut() {
        let chosen = item.optional && included.contains(&item.id) && !excluded.contains(&item.id);
        if item.optional && !chosen {
            item.outcome = Some(ItemOutcome::Excluded);
            item.detail = "nicht gewählt; bleibt offen, wie Claude riet".into();
        } else if chosen {
            // The user's choice turns the advice into a planned merge; the run checks it like any other.
            let plan = set.plans.iter_mut().find(|p| p.key == item.repo);
            if let Some(pr) = plan.and_then(|p| p.prs.iter_mut().find(|pr| pr.number.to_string() == item.reference)) {
                pr.action = Action::Merge;
                pr.reason = format!("von dir gewählt; Claude riet, offen zu lassen: {}", pr.reason);
            }
        } else if excluded.contains(&item.id) {
            item.outcome = Some(ItemOutcome::Excluded);
            item.detail = "von dir ausgenommen".into();
            if item.kind == ItemKind::Tag {
                if let Some(plan) = set.plans.iter_mut().find(|p| p.key == item.repo) {
                    plan.tag = None;
                    plan.tag_note = Some("von dir ausgenommen".into());
                }
            }
        }
    }
    for plan in set.plans.iter_mut().filter(|p| p.tag.is_some() && p.tag_needs_merge) {
        let merging = job.items.iter().any(|i| i.repo == plan.key && i.kind == ItemKind::Merge && i.outcome.is_none());
        if !merging {
            let why = format!("{} unverändert seit {}, und es wird nichts gemergt; kein Tag", plan.default_branch, plan.last_tag.as_deref().unwrap_or("dem letzten Tag"));
            for item in job.items.iter_mut().filter(|i| i.repo == plan.key && i.kind == ItemKind::Tag && i.outcome.is_none()) {
                item.outcome = Some(ItemOutcome::Excluded);
                item.detail = why.clone();
            }
            plan.tag = None;
            plan.tag_note = Some(why);
        }
    }
    let step = job.step_mut("care").expect("checked above");
    step.approved = Some(format!("{plan_id}"));
    step.set(StepState::Approved, "freigegeben", now);
    job.updated_at = now;
    Ok(())
}

/// The user said no to the plan: nothing of it runs; steps after it go on with the current state.
pub fn reject_plan(job: &mut Job, plan_id: u64, now: u64) -> Result<(), String> {
    if job.plan.as_ref().map(|p| p.id) != Some(plan_id) {
        return Err("Dieser Plan gilt nicht mehr".into());
    }
    for item in job.items.iter_mut().filter(|i| i.outcome.is_none()) {
        item.outcome = Some(ItemOutcome::Excluded);
        item.detail = "Plan verworfen".into();
    }
    if let Some(set) = job.plan.as_mut() {
        for plan in &mut set.plans {
            plan.tag = None;
        }
    }
    let step = job.step_mut("care").ok_or("Auftrag ohne Pflegeschritt")?;
    if step.state != StepState::NeedsApproval {
        return Err("Der Pflegeplan wartet nicht auf eine Freigabe".into());
    }
    step.set(StepState::Rejected, "von dir verworfen; nichts ausgeführt", now);
    job.updated_at = now;
    Ok(())
}

/// Approves the proposal of a docs, wiki or website step, bound to its fingerprint: a proposal
/// that changed since the window showed it is not approved.
pub fn approve_step(job: &mut Job, step_id: &str, fingerprint: &str, now: u64) -> Result<(), String> {
    let step = job.step_mut(step_id).ok_or("Schritt nicht gefunden")?;
    if step.state != StepState::NeedsApproval {
        return Err("Dieser Schritt wartet nicht auf eine Freigabe".into());
    }
    let current = match (&step.content, &step.website) {
        (Some(p), _) => p.fingerprint.clone(),
        (None, Some(w)) => w.commit.clone(),
        _ => return Err("Kein Vorschlag vorhanden".into()),
    };
    if current != fingerprint {
        return Err("Der Vorschlag hat sich geändert; bitte erneut ansehen".into());
    }
    if step.content.as_ref().is_some_and(|p| !p.checks_passed()) {
        return Err("Prüfungen nicht bestanden; neu vorbereiten".into());
    }
    step.approved = Some(current);
    step.set(StepState::Approved, "freigegeben", now);
    job.updated_at = now;
    Ok(())
}

pub fn reject_step(job: &mut Job, step_id: &str, now: u64) -> Result<(), String> {
    let step = job.step_mut(step_id).ok_or("Schritt nicht gefunden")?;
    if !matches!(step.state, StepState::NeedsApproval | StepState::Blocked | StepState::Failed) {
        return Err("Dieser Schritt kann nicht verworfen werden".into());
    }
    let what = if step.kind == StepKind::Website {
        "von dir verworfen; der lokale Commit bleibt unveröffentlicht im Website-Projekt"
    } else {
        "von dir verworfen; nichts veröffentlicht"
    };
    step.approved = None;
    step.set(StepState::Rejected, what, now);
    job.updated_at = now;
    Ok(())
}

/// Starts a step over: a docs, wiki or website step drops its proposal and approval; the care step
/// makes a new plan, as long as nothing of the old one was carried out.
pub fn redo_step(job: &mut Job, step_id: &str, now: u64) -> Result<(), String> {
    if step_id == "care" {
        if job.items.iter().any(|i| matches!(i.outcome, Some(ItemOutcome::Done | ItemOutcome::AlreadyDone | ItemOutcome::Skipped | ItemOutcome::Failed)) || i.writing) {
            return Err("Vom Plan wurde schon etwas ausgeführt; für das Übrige einen neuen Auftrag starten".into());
        }
        let step = job.step_mut("care").ok_or("Auftrag ohne Pflegeschritt")?;
        if !matches!(step.state, StepState::NeedsApproval | StepState::Blocked | StepState::Failed) {
            return Err("Der Pflegeplan lässt sich jetzt nicht neu erstellen".into());
        }
        step.approved = None;
        step.finished_at = None;
        step.state = StepState::Pending;
        step.detail = "wird neu geprüft".into();
        job.plan = None;
        job.items.clear();
        job.run_states.clear();
        job.updated_at = now;
        return Ok(());
    }
    let step = job.step_mut(step_id).ok_or("Schritt nicht gefunden")?;
    if !matches!(step.kind, StepKind::Docs | StepKind::Wiki | StepKind::Website) {
        return Err("Nur Pflegeplan, Inhalts- und Website-Schritte lassen sich neu vorbereiten".into());
    }
    if !matches!(step.state, StepState::NeedsApproval | StepState::Blocked | StepState::Failed | StepState::NothingToDo) {
        return Err("Dieser Schritt lässt sich jetzt nicht neu vorbereiten".into());
    }
    if step.writing.is_some() || step.pr.is_some() {
        return Err("Der Schritt hat schon veröffentlicht; dafür einen neuen Auftrag starten".into());
    }
    step.content = None;
    step.website = None;
    step.approved = None;
    step.finished_at = None;
    step.state = StepState::Pending;
    step.detail = "wird neu vorbereitet".into();
    job.updated_at = now;
    Ok(())
}

/// The user ends the job: what is not done stays undone; nothing done is taken back.
pub fn end(job: &mut Job, why: &str, now: u64) {
    for step in job.steps.iter_mut().filter(|s| !s.state.finished()) {
        step.set(StepState::Cancelled, "nicht ausgeführt: Auftrag beendet", now);
    }
    for item in job.items.iter_mut().filter(|i| i.outcome.is_none()) {
        item.outcome = Some(ItemOutcome::Cancelled);
        item.detail = "nicht ausgeführt: Auftrag beendet".into();
    }
    job.ended = Some(why.to_owned());
    job.updated_at = now;
}

// MARK: Resolving interrupted writes

/// What GitHub (or the clone) says about an item whose write had begun when the app stopped.
#[derive(Debug, PartialEq)]
pub enum Resolution {
    /// It happened; the text says how it was found.
    Done(String),
    /// The close comment is there, the pull request still open: only the close is missing.
    Commented,
    /// It did not happen; the item may run again (it checks everything again first).
    NotDone,
}

/// Looks up whether an interrupted item's write happened. Reads only. A merge found done is
/// recorded in `state`, so a following tag knows where the default branch should be.
pub fn resolve_item(item: &CareItem, plan: &crate::repocare::Plan, since: u64, state: &mut RunState) -> Result<Resolution> {
    use crate::github::{encode_ref, get};
    use crate::repocare::{commented_since, pull, CLOSE_COMMENT};
    let number = || item.reference.parse::<u64>().context("PR-Nummer unlesbar");
    let found = |what: &str| Resolution::Done(format!("{what} (nach der Unterbrechung bei GitHub nachgelesen)"));
    Ok(match item.kind {
        ItemKind::Merge => {
            let pr = pull(&plan.repo, number()?)?;
            let planned = plan.prs.iter().find(|p| p.number == number().unwrap_or(0)).map(|p| p.head_sha.as_str());
            if pr["merged"].as_bool() == Some(true) && pr["head"]["sha"].as_str() == planned {
                state.merged.insert(number()?);
                state.expected_head = pr["merge_commit_sha"].as_str().map(str::to_owned);
                found("gemergt")
            } else {
                Resolution::NotDone
            }
        }
        ItemKind::Close => {
            let pr = pull(&plan.repo, number()?)?;
            if pr["state"].as_str() == Some("closed") {
                found("geschlossen")
            } else if commented_since(&plan.repo, number()?, CLOSE_COMMENT, since)? {
                Resolution::Commented
            } else {
                Resolution::NotDone
            }
        }
        ItemKind::Rebase => {
            if commented_since(&plan.repo, number()?, "@dependabot rebase", since)? {
                found("@dependabot rebase angestoßen")
            } else {
                Resolution::NotDone
            }
        }
        ItemKind::Branch => match get(&format!("repos/{}/branches/{}", plan.repo, encode_ref(&item.reference)))? {
            None => found("gelöscht"),
            Some(_) => Resolution::NotDone,
        },
        ItemKind::LocalBranch => {
            let dir = plan.clone.as_deref().context("keine lokale Kopie")?;
            let reference = format!("refs/heads/{}", item.reference);
            match crate::shell::output("git", &["rev-parse", "--verify", "--quiet", &reference], std::path::Path::new(dir)) {
                Err(_) => found("gelöscht"),
                Ok(_) => Resolution::NotDone,
            }
        }
        ItemKind::LocalSync | ItemKind::Tag => Resolution::NotDone,
    })
}

// MARK: Store

pub fn dir() -> PathBuf {
    paths::data_dir().join("jobs")
}

fn file(id: &str) -> PathBuf {
    dir().join(format!("{id}.json"))
}

fn log_file(id: &str) -> PathBuf {
    dir().join(format!("{id}.log"))
}

/// A new job id: sortable by time, unique within the app's lifetime.
pub fn new_id(now: u64, counter: u64) -> String {
    format!("{now}-{counter:03}")
}

pub fn save(job: &Job) -> Result<()> {
    content::write_atomic(&file(&job.id), &serde_json::to_vec_pretty(job)?)
}

/// All stored jobs, newest first; files that cannot be read are skipped and named.
pub fn load_all() -> (Vec<Job>, Vec<String>) {
    let mut jobs = Vec::new();
    let mut unreadable = Vec::new();
    let Ok(entries) = fs::read_dir(dir()) else { return (jobs, unreadable) };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|text| serde_json::from_str::<Job>(&text).map_err(anyhow::Error::from)) {
            Ok(job) => jobs.push(job),
            Err(e) => unreadable.push(format!("{}: {e:#}", path.display())),
        }
    }
    jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    (jobs, unreadable)
}

/// Removes the oldest finished jobs beyond `KEEP_FINISHED`, with their logs and work folders.
pub fn prune(jobs: &mut Vec<Job>) {
    let finished: Vec<String> = jobs.iter().filter(|j| !j.open()).map(|j| j.id.clone()).collect();
    for id in finished.iter().skip(KEEP_FINISHED) {
        remove(id);
        jobs.retain(|j| &j.id != id);
    }
}

pub fn remove(id: &str) {
    let _ = fs::remove_file(file(id));
    let _ = fs::remove_file(log_file(id));
    content::remove_work(id);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub kind: String,
    pub text: String,
}

pub fn append_log(id: &str, entry: &LogEntry) -> Result<()> {
    fs::create_dir_all(dir())?;
    let mut file = fs::OpenOptions::new().create(true).append(true).open(log_file(id)).context("Log öffnen")?;
    writeln!(file, "{}", serde_json::to_string(entry)?)?;
    Ok(())
}

/// The job's log, at most the newest `MAX_LOG_LINES` lines.
pub fn read_log(id: &str) -> Vec<LogEntry> {
    let text = fs::read_to_string(log_file(id)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(MAX_LOG_LINES);
    lines.iter().skip(skip).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ManagedRepo, ReleaseRule};
    use crate::content::{ContentTarget, TargetKind};
    use crate::repocare::{Branch, Plan, Proof, PullRequest};

    fn config() -> Config {
        let mut config = Config::default();
        config.add_repository(ManagedRepo { release: Some(ReleaseRule { tag_prefix: "v".into(), publishes_release: true }), ..ManagedRepo::new("o/app") });
        config.add_repository(ManagedRepo::new("o/lib"));
        config.add_repository(ManagedRepo::new("o/site"));
        for (id, kind, sources) in [("t1", TargetKind::Docs, vec!["o/app"]), ("t2", TargetKind::Wiki, vec!["o/app", "o/lib"])] {
            config.content_targets.push(ContentTarget {
                id: id.into(),
                kind,
                name: id.to_uppercase(),
                sources: sources.into_iter().map(String::from).collect(),
                repo: "o/app".into(),
                branch: String::new(),
                paths: content::default_paths(kind),
                instructions: String::new(),
                skill: String::new(),
                model: String::new(),
                basis: Basis::Release,
                check: String::new(),
                merge_when_green: false,
            });
        }
        config
    }

    fn outcome(kind: ItemKind, outcome: ItemOutcome) -> CareItem {
        CareItem {
            id: format!("{kind:?}-{outcome:?}"),
            repo: "o/app".into(),
            kind,
            reference: String::new(),
            label: String::new(),
            context: String::new(),
            outcome: Some(outcome),
            detail: String::new(),
            writing: false,
            commented: false,
            optional: false,
        }
    }

    #[test]
    fn branches_deleted_already_need_no_repeat() {
        use ItemKind::{Branch as B, Merge as M};
        use ItemOutcome::{AlreadyDone, Done, Excluded, Failed, Skipped};
        let items = [outcome(M, Done), outcome(M, Done), outcome(M, Done), outcome(B, Done), outcome(B, AlreadyDone), outcome(B, AlreadyDone)];
        assert_eq!(care_result(&items), (StepState::Done, "3 PRs gemergt · 1 Branch gelöscht · 2 bereits gelöscht".to_owned()));
        assert_eq!(care_result(&[outcome(B, AlreadyDone), outcome(M, Excluded)]).0, StepState::NothingToDo, "nothing left, nothing done");
        let (state, summary) = care_result(&[outcome(M, Done), outcome(B, AlreadyDone), outcome(B, Skipped)]);
        assert_eq!(state, StepState::Partial, "a safety stop still counts as open: {summary}");
        assert_eq!(care_result(&[outcome(B, AlreadyDone), outcome(M, Failed)]).0, StepState::Partial);
        assert_eq!(care_result(&[outcome(M, Failed)]).0, StepState::Failed);
    }

    #[test]
    fn older_jobs_without_the_new_outcome_stay_readable() {
        let item: CareItem = serde_json::from_value(serde_json::json!({
            "id": "a", "repo": "o/app", "kind": "branch", "reference": "x", "label": "x", "context": "",
            "outcome": "skipped", "detail": "war schon gelöscht", "writing": false, "commented": false, "optional": false,
        })).unwrap();
        assert_eq!(item.outcome, Some(ItemOutcome::Skipped));
    }

    fn all() -> JobOptions {
        JobOptions {
            prs: true,
            cleanup: true,
            release: true,
            targets: vec![TargetChoice { id: "t1".into(), basis: Basis::Release }, TargetChoice { id: "t2".into(), basis: Basis::Commit }, TargetChoice { id: "t1".into(), basis: Basis::Commit }],
            website: true,
            ..JobOptions::default()
        }
    }

    fn job(options: &JobOptions) -> Job {
        let config = config();
        let scope = Scope { config: &config, website_sources: Some(vec!["o/app".into(), "o/lib".into()]) };
        make("j1", 100, &["o/app".into(), "O/App".into(), "o/lib".into()], options, &scope).unwrap()
    }

    #[test]
    fn a_job_has_each_repository_target_and_website_once() {
        let job = job(&all());
        assert_eq!(job.repos, ["o/app", "o/lib"]);
        let ids: Vec<&str> = job.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["care", "release:o/app", "target:t1", "target:t2", "website"], "o/lib has no release rule; t1 once");
        assert_eq!(job.step("target:t1").unwrap().after, ["care", "release:o/app"]);
        assert_eq!(job.step("target:t1").unwrap().basis, Some(Basis::Release), "the first choice counts");
        assert_eq!(job.step("website").unwrap().after, ["care", "release:o/app"]);
        for (i, step) in job.steps.iter().enumerate() {
            assert!(step.after.iter().all(|d| job.steps[..i].iter().any(|s| &s.id == d)), "{} only waits for earlier steps", step.id);
        }

        let config = config();
        let scope = Scope { config: &config, website_sources: None };
        let only_docs = JobOptions { targets: vec![TargetChoice { id: "t1".into(), basis: Basis::Commit }], ..JobOptions::default() };
        let docs = make("j2", 100, &["o/app".into()], &only_docs, &scope).unwrap();
        assert_eq!(docs.steps.len(), 1, "a README without care, release or website");
        assert!(docs.steps[0].after.is_empty());
        assert!(make("j3", 100, &["o/app".into()], &JobOptions { website: true, ..JobOptions::default() }, &scope).is_err(), "no website configured");
        assert!(make("j4", 100, &["o/lib".into()], &JobOptions { release: true, ..JobOptions::default() }, &scope).is_err(), "no release rule");
        assert!(make("j5", 100, &["o/other".into()], &only_docs, &scope).is_err(), "unmanaged");
        assert!(make("j6", 100, &["o/site".into()], &only_docs, &scope).is_err(), "t1 is not fed by o/site");
    }

    #[test]
    fn open_jobs_do_not_share_targets() {
        let open = job(&all());
        let options = JobOptions { prs: true, ..JobOptions::default() };
        assert!(overlap(&[&open], &["O/APP".into()], &options).is_some());
        assert!(overlap(&[&open], &["o/site".into()], &options).is_none());
        let docs = JobOptions { targets: vec![TargetChoice { id: "t2".into(), basis: Basis::Commit }], ..JobOptions::default() };
        assert!(overlap(&[&open], &["o/site".into()], &docs).is_some(), "same wiki");
        assert!(overlap(&[&open], &["o/site".into()], &JobOptions { website: true, ..JobOptions::default() }).is_some(), "same website");
    }

    fn plan_set() -> PlanSet {
        let pr = |number, action| PullRequest { number, action, title: format!("PR {number}"), ..PullRequest::default() };
        let branch = |name: &str| Branch { name: name.into(), sha: "s".into(), reason: "gemergt".into(), proof: Proof::Contained };
        let plan = Plan {
            key: "o/app".into(),
            repo: "o/app".into(),
            prs: vec![pr(1, Action::Merge), pr(2, Action::Keep), pr(3, Action::Close)],
            tag: Some("v1.0.1".into()),
            branches: vec![branch("old")],
            local_branches: vec![branch("local")],
            local_sync: Some("veraltet".into()),
            ..Plan::default()
        };
        let broken = Plan { key: "o/lib".into(), error: Some("404".into()), ..Plan::default() };
        PlanSet { id: 7, created_at: 0, prs: true, cleanup: true, release: true, model: String::new(), targets: Vec::new(), plans: vec![plan, broken] }
    }

    #[test]
    fn the_window_can_only_narrow_the_plan() {
        let mut job = job(&all());
        let set = plan_set();
        job.items = care_items(&set);
        let ids: Vec<&str> = job.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["o/app|close|3", "o/app|merge|1", "o/app|tag|v1.0.1", "o/app|branch|old", "o/app|sync|", "o/app|local|local"], "nothing for a repository that could not be read");
        job.plan = Some(set);
        assert!(approve_plan(&mut job, 7, &[], &[], 1).is_err(), "not waiting for approval yet");
        job.step_mut("care").unwrap().state = StepState::NeedsApproval;
        assert!(approve_plan(&mut job, 6, &[], &[], 1).is_err(), "an older plan");
        assert!(approve_plan(&mut job, 7, &["o/app|merge|99".into()], &[], 1).is_err(), "an item that is not in the plan");
        assert!(approve_plan(&mut job, 7, &[], &["o/app|merge|1".into()], 1).is_err(), "only offered items can be chosen");
        approve_plan(&mut job, 7, &["o/app|merge|1".into(), "o/app|tag|v1.0.1".into()], &[], 1).unwrap();
        assert_eq!(job.step("care").unwrap().state, StepState::Approved);
        assert_eq!(job.items[1].outcome, Some(ItemOutcome::Excluded));
        assert_eq!(job.plan.as_ref().unwrap().plans[0].tag, None, "a left-out tag is not set");
        assert!(approve_plan(&mut job, 7, &[], &[], 2).is_err(), "approved once");
    }

    /// A plan that only offers a merge Claude advised against (yellow, mergeable), and a tag that
    /// rests on it.
    fn advised_plan() -> PlanSet {
        let pr = PullRequest { number: 20, action: Action::Keep, optional: true, reviewed: true, risk: Some("yellow".into()), reason: "manueller Test offen".into(), ..PullRequest::default() };
        let plan = Plan { key: "o/app".into(), repo: "o/app".into(), default_branch: "main".into(), prs: vec![pr], tag: Some("v1.3.1".into()), last_tag: Some("v1.3.0".into()), tag_needs_merge: true, ..Plan::default() };
        PlanSet { id: 8, created_at: 0, prs: true, cleanup: false, release: true, model: String::new(), targets: Vec::new(), plans: vec![plan] }
    }

    #[test]
    fn a_merge_claude_advised_against_is_the_users_choice() {
        let set = advised_plan();
        let items = care_items(&set);
        assert_eq!(items.iter().map(|i| (i.id.as_str(), i.optional)).collect::<Vec<_>>(), [("o/app|merge|20", true), ("o/app|tag|v1.3.1", false)]);

        // Not chosen: nothing merges, and the tag that needed the merge goes too.
        let mut job = job(&all());
        job.items = items.clone();
        job.plan = Some(set.clone());
        job.step_mut("care").unwrap().state = StepState::NeedsApproval;
        approve_plan(&mut job, 8, &[], &[], 1).unwrap();
        assert!(job.items.iter().all(|i| i.outcome == Some(ItemOutcome::Excluded)), "{:?}", job.items);
        let plan = &job.plan.as_ref().unwrap().plans[0];
        assert_eq!(plan.prs[0].action, Action::Keep);
        assert_eq!(plan.tag, None);
        assert!(plan.tag_note.as_deref().unwrap().contains("nichts gemergt"));

        // Chosen: it becomes a planned merge, checked like any other, and the tag stays.
        let mut job = self::job(&all());
        job.items = items;
        job.plan = Some(set);
        job.step_mut("care").unwrap().state = StepState::NeedsApproval;
        approve_plan(&mut job, 8, &[], &["o/app|merge|20".into()], 1).unwrap();
        assert_eq!(job.items[0].outcome, None, "runs");
        let plan = &job.plan.as_ref().unwrap().plans[0];
        assert_eq!(plan.prs[0].action, Action::Merge);
        assert!(plan.prs[0].reason.starts_with("von dir gewählt"));
        assert_eq!(plan.tag.as_deref(), Some("v1.3.1"));
    }

    fn proposal(fingerprint: &str, checks_ok: bool) -> Proposal {
        Proposal {
            base_branch: "main".into(),
            base_commit: "b".into(),
            sources: Vec::new(),
            files: Vec::new(),
            diff: String::new(),
            stat: String::new(),
            checks: vec![content::CheckResult { name: "x".into(), ok: checks_ok, detail: String::new() }],
            summary: String::new(),
            cost_usd: None,
            branch: "x".into(),
            target: String::new(),
            fingerprint: fingerprint.into(),
        }
    }

    #[test]
    fn approvals_bind_to_the_proposal_that_was_shown() {
        let mut job = job(&all());
        let step = job.step_mut("target:t1").unwrap();
        step.state = StepState::NeedsApproval;
        step.content = Some(proposal("fp1", true));
        assert!(approve_step(&mut job, "target:t1", "fp0", 1).is_err(), "a proposal the window did not show");
        approve_step(&mut job, "target:t1", "fp1", 1).unwrap();
        assert_eq!(job.step("target:t1").unwrap().approved.as_deref(), Some("fp1"));

        let step = job.step_mut("target:t2").unwrap();
        step.state = StepState::NeedsApproval;
        step.content = Some(proposal("fp2", false));
        assert!(approve_step(&mut job, "target:t2", "fp2", 1).is_err(), "failed checks are never approved");
        redo_step(&mut job, "target:t2", 1).unwrap();
        assert_eq!(job.step("target:t2").unwrap().state, StepState::Pending);
        assert!(job.step("target:t2").unwrap().content.is_none());
    }

    #[test]
    fn failures_stop_what_depends_on_them_and_nothing_else() {
        let mut job = job(&all());
        job.steps[0].set(StepState::Done, "", 1);
        assert_eq!(job.next_runnable(), Some(1), "release after care");
        job.steps[1].set(StepState::Failed, "CI rot", 1);
        job.propagate(1);
        assert_eq!(job.step("target:t1").unwrap().state, StepState::Skipped, "t1 depends on the release");
        assert_eq!(job.step("target:t2").unwrap().state, StepState::Skipped, "t2 too: o/app is one of its sources");
        assert_eq!(job.step("website").unwrap().state, StepState::Skipped);

        let mut job = self::job(&JobOptions { prs: true, targets: vec![TargetChoice { id: "t2".into(), basis: Basis::Commit }], ..JobOptions::default() });
        job.steps[0].set(StepState::Rejected, "", 1);
        assert_eq!(job.next_runnable(), Some(1), "a rejected plan lets the wiki go on with the current state");
        job.steps[1].set(StepState::Done, "", 1);
        assert_eq!(job.status(false).state, JobState::Succeeded);
        assert!(job.status(false).reason.contains("1 verworfen"));
    }

    #[test]
    fn a_partial_success_never_reads_as_success() {
        let mut job = job(&all());
        for step in &mut job.steps {
            step.set(StepState::Done, "", 1);
        }
        assert_eq!(job.status(false).state, JobState::Succeeded);
        job.steps[0].set(StepState::Partial, "1 übersprungen", 1);
        assert_eq!(job.status(false).state, JobState::Partial);
        job.steps[2].set(StepState::Failed, "", 1);
        assert_eq!(job.status(false).state, JobState::Partial);
        for step in &mut job.steps {
            step.set(StepState::Failed, "", 1);
        }
        assert_eq!(job.status(false).state, JobState::Failed);

        let mut waiting = self::job(&all());
        waiting.steps[0].set(StepState::Done, "", 1);
        waiting.steps[1].set(StepState::Waiting, "noch kein Release", 1);
        assert_eq!(waiting.status(false).state, JobState::Waiting);
        assert_eq!(waiting.next_runnable(), None, "nothing after it runs meanwhile");
        waiting.resume();
        assert_eq!(waiting.next_runnable(), Some(1), "asked again when the user continues");
    }

    #[test]
    fn a_restart_neither_forgets_nor_repeats() {
        let mut job = job(&all());
        job.steps[0].set(StepState::Done, "", 1);
        job.steps[1].state = StepState::Running;
        job.steps[1].writing = Some("tag v1.0.1".into());
        job.steps[2].state = StepState::Running;
        let stored: Job = serde_json::from_str(&serde_json::to_string(&job).unwrap()).unwrap();
        let mut loaded = stored;
        assert!(loaded.mark_interrupted());
        assert_eq!(loaded.steps[0].state, StepState::Done, "done stays done");
        assert_eq!(loaded.steps[1].state, StepState::Approved);
        assert_eq!(loaded.steps[1].writing.as_deref(), Some("tag v1.0.1"), "the started write is resolved, not forgotten");
        assert_eq!(loaded.steps[2].state, StepState::Pending, "a preparation simply starts again");
        assert_eq!(loaded.status(false).state, JobState::Blocked);
        assert!(loaded.status(false).next.contains("Stand bei GitHub"));
    }

    #[test]
    fn an_interrupted_write_is_looked_up_not_repeated() {
        use crate::github::mock;
        use serde_json::json;
        let set = plan_set();
        let plan = &set.plans[0];
        let items = care_items(&set);
        let item = |id: &str| items.iter().find(|i| i.id == id).unwrap().clone();
        let mut state = RunState::default();
        let pull = |body: serde_json::Value| ("repos/o/app/pulls/1".to_owned(), Some(body));

        let mut merge_plan = plan.clone();
        merge_plan.prs[0].head_sha = "h1".into();
        mock::install([pull(json!({ "state": "closed", "merged": true, "mergeable": true, "head": { "sha": "h1" }, "merge_commit_sha": "m1" }))]);
        assert!(matches!(resolve_item(&item("o/app|merge|1"), &merge_plan, 100, &mut state).unwrap(), Resolution::Done(_)));
        assert!(state.merged.contains(&1) && state.expected_head.as_deref() == Some("m1"), "the tag knows the merge");

        mock::install([pull(json!({ "state": "open", "merged": false, "mergeable": true, "head": { "sha": "h1" } }))]);
        assert_eq!(resolve_item(&item("o/app|merge|1"), &merge_plan, 100, &mut RunState::default()).unwrap(), Resolution::NotDone);

        let comments = |body: &str| ("repos/o/app/issues/3/comments?since=1970-01-01T00:01:40Z&per_page=100&page=1".to_owned(), Some(json!([{ "body": body }])));
        mock::install([
            ("repos/o/app/pulls/3".to_owned(), Some(json!({ "state": "open", "mergeable": true }))),
            comments("Geschlossen von OpenReleasePilot: überholt"),
        ]);
        assert_eq!(resolve_item(&item("o/app|close|3"), plan, 100, &mut state).unwrap(), Resolution::Commented, "only the close is missing, the comment is not posted twice");
        mock::install([("repos/o/app/pulls/3".to_owned(), Some(json!({ "state": "open", "mergeable": true }))), comments("ein anderer Kommentar")]);
        assert_eq!(resolve_item(&item("o/app|close|3"), plan, 100, &mut state).unwrap(), Resolution::NotDone);

        mock::install([("repos/o/app/branches/old".to_owned(), None)]);
        assert!(matches!(resolve_item(&item("o/app|branch|old"), plan, 100, &mut state).unwrap(), Resolution::Done(_)));
        assert!(mock::writes().is_empty(), "resolving only reads");
    }

    #[test]
    fn ending_a_job_keeps_what_was_done() {
        let mut job = job(&all());
        job.steps[0].set(StepState::Done, "3 PRs gemergt", 1);
        end(&mut job, "abgebrochen", 2);
        assert_eq!(job.steps[0].state, StepState::Done);
        assert!(job.steps[1..].iter().all(|s| s.state == StepState::Cancelled));
        let status = job.status(false);
        assert_eq!(status.state, JobState::Cancelled);
        assert!(status.reason.contains("nichts zurückgenommen"));
        assert_eq!(job.next_runnable(), None);
    }
}
