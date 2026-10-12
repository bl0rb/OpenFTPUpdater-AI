//! One pass of a care job: from where it stands, step by step, until it needs a decision, waits for
//! something outside the app, or is finished. Every change is stored right away (see
//! `openreleasepilot_core::jobs`), so a restart neither forgets a result nor repeats a write: a
//! write that had begun is first looked up on GitHub (or the wiki, or the clone).

use crate::steps::{self, Ctx};
use crate::{changed, with_model, Status};
use anyhow::{anyhow, bail, Context, Result};
use openreleasepilot_core::claude::{self, ContentSource, Event};
use openreleasepilot_core::content::{self, Basis, Conflict, Delivery, Proposal, PullRequestRef, TargetKind};
use openreleasepilot_core::jobs::{self, CareItem, ItemKind, ItemOutcome, Job, JobState, Link, Resolution, StepKind, StepState, WebsiteProposal};
use openreleasepilot_core::repocare::{self, Action, CiWait, Done, Plan, PlanSet, ReleaseWait, RunState};
use openreleasepilot_core::repos::RepoTarget;
use openreleasepilot_core::{git, github, shell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn now() -> u64 {
    steps::now()
}

/// Stores the job (its file, then the model) and tells the window. A job that cannot be stored
/// stops: going on would risk results a restart does not know about.
fn save(ctx: &Ctx, job: &mut Job) -> Result<()> {
    job.updated_at = now();
    jobs::save(job).context("Auftrag konnte nicht gespeichert werden; angehalten")?;
    let copy = job.clone();
    with_model(&ctx.app, |m| {
        if let Some(slot) = m.jobs.iter_mut().find(|j| j.id == copy.id) {
            *slot = copy;
        }
    });
    changed(&ctx.app);
    Ok(())
}

/// The managed repositories as they are configured now.
fn current_targets(ctx: &Ctx) -> Vec<RepoTarget> {
    let (config, site) = with_model(&ctx.app, |m| (m.config.clone(), m.site.clone()));
    crate::repo_targets(&config, &site)
}

/// Model, effort and cost limit for a Claude run of a job: the choice, the app's default, the website's.
fn claude_settings(ctx: &Ctx, chosen: &[&str]) -> (Option<String>, Option<String>, Option<f64>) {
    let (config, site) = with_model(&ctx.app, |m| (m.config.clone(), m.site.clone().ok()));
    let model = chosen
        .iter()
        .copied()
        .chain([config.claude_model.as_str()])
        .map(str::trim)
        .find(|m| !m.is_empty())
        .map(str::to_owned)
        .or_else(|| site.as_ref().and_then(|s| s.claude.model.clone()));
    let effort = site.as_ref().and_then(|s| s.claude.effort.clone());
    let budget = config.max_budget_usd.or_else(|| site.as_ref().and_then(|s| s.claude.max_budget_usd));
    (model, effort, budget)
}

pub fn pass(ctx: &Ctx, id: &str) -> Result<(Status, String)> {
    let mut job = with_model(&ctx.app, |m| m.jobs.iter().find(|j| j.id == id).cloned()).ok_or_else(|| anyhow!("Auftrag {id} nicht gefunden"))?;
    if job.ended.is_some() {
        bail!("Der Auftrag ist beendet");
    }
    job.resume();
    if job.interrupted {
        ctx.info("Der letzte Lauf wurde unterbrochen: zuerst den Stand bei GitHub prüfen …");
        resolve_interrupted(ctx, &mut job)?;
        job.interrupted = false;
        save(ctx, &mut job)?;
    }
    while !ctx.runner.is_cancelled() {
        if job.propagate(now()) {
            save(ctx, &mut job)?;
        }
        let Some(i) = job.next_runnable() else { break };
        let approved = job.steps[i].state == StepState::Approved;
        let kind = job.steps[i].kind;
        ctx.cmd(format!("━━ {} ━━", job.steps[i].label));
        job.steps[i].state = StepState::Running;
        save(ctx, &mut job)?;
        let result = match kind {
            StepKind::Care if approved => care_run(ctx, &mut job, i),
            StepKind::Care => care_scan(ctx, &mut job, i),
            StepKind::Release => release(ctx, &mut job, i),
            StepKind::Docs | StepKind::Wiki if approved => content_publish(ctx, &mut job, i),
            StepKind::Docs | StepKind::Wiki => content_prepare(ctx, &mut job, i),
            StepKind::Website if approved => website_publish(ctx, &mut job, i),
            StepKind::Website => website_prepare(ctx, &mut job, i),
        };
        if let Err(e) = result {
            let message = format!("{e:#}");
            if ctx.runner.is_cancelled() {
                job.steps[i].set(StepState::Cancelled, format!("abgebrochen ({message})"), now());
            } else if e.is::<Conflict>() {
                ctx.warn(format!("{}: {message}", job.steps[i].label));
                job.steps[i].set(StepState::Blocked, message, now());
            } else {
                ctx.log("error", format!("{}: {message}", job.steps[i].label));
                job.steps[i].set(StepState::Failed, message, now());
            }
        }
        if job.steps[i].state == StepState::Running {
            job.steps[i].set(StepState::Failed, "ohne Ergebnis beendet", now());
        }
        save(ctx, &mut job)?;
    }
    if ctx.runner.is_cancelled() {
        jobs::end(&mut job, "abgebrochen", now());
        save(ctx, &mut job)?;
    }
    let status = job.status(false);
    if status.state.finished() {
        content::remove_work(&job.id);
    }
    ctx.info(format!("Auftrag: {}", status.reason));
    let kind = match status.state {
        JobState::Succeeded => Status::Ok,
        JobState::Failed => Status::Error,
        JobState::Partial | JobState::Blocked | JobState::Cancelled => Status::Warning,
        _ => Status::Ok,
    };
    Ok((kind, status.reason))
}

// MARK: Interrupted writes

/// Looks up every write that had begun when the app stopped, before anything else runs.
fn resolve_interrupted(ctx: &Ctx, job: &mut Job) -> Result<()> {
    if let Some(set) = job.plan.clone() {
        for n in 0..job.items.len() {
            if !job.items[n].writing {
                continue;
            }
            let item = job.items[n].clone();
            let Some(plan) = set.plans.iter().find(|p| p.key == item.repo) else { continue };
            let state = job.run_states.entry(item.repo.clone()).or_insert_with(|| RunState::new(plan));
            let resolution = jobs::resolve_item(&item, plan, job.created_at, state)?;
            let entry = &mut job.items[n];
            entry.writing = false;
            match resolution {
                Resolution::Done(text) => {
                    ctx.ok(format!("  {}: {text}", item.label));
                    entry.outcome = Some(ItemOutcome::Done);
                    entry.detail = text;
                }
                Resolution::Commented => {
                    ctx.info(format!("  {}: Kommentar war gepostet, das Schließen fehlt noch", item.label));
                    entry.commented = true;
                }
                Resolution::NotDone => ctx.info(format!("  {}: nicht ausgeführt; wird erneut geprüft", item.label)),
            }
        }
    }
    let config = ctx.config();
    for i in 0..job.steps.len() {
        let Some(what) = job.steps[i].writing.clone() else { continue };
        let step = &job.steps[i];
        match step.kind {
            StepKind::Release => {
                let key = step.subject.clone();
                let repo = config.repository(&key).map(|r| r.repo.clone()).unwrap_or(key.clone());
                let planned = job.plan.as_ref().and_then(|s| s.plans.iter().find(|p| p.key == key)).and_then(|p| p.tag.clone());
                let expected = job.run_states.get(&key).and_then(|s| s.expected_head.clone()).or_else(|| job.plan.as_ref().and_then(|s| s.plans.iter().find(|p| p.key == key)).map(|p| p.base_head.clone()));
                let step = &mut job.steps[i];
                step.writing = None;
                if let Some(tag) = planned {
                    match content::resolve_tag(&repo, &tag) {
                        Ok(sha) if Some(&sha) == expected.as_ref() => {
                            ctx.ok(format!("  {tag} war gesetzt (auf {})", content::short(&sha)));
                            step.links.push(Link { label: format!("Tag {tag}"), url: format!("https://github.com/{repo}/releases/tag/{tag}") });
                            step.tag = Some(tag);
                            step.commit = Some(sha);
                        }
                        Ok(sha) => step.set(StepState::Failed, format!("{tag} zeigt auf {}, nicht auf den erwarteten Commit", content::short(&sha)), now()),
                        Err(_) => ctx.info(format!("  {tag} ist nicht gesetzt; der Tag wird erneut geprüft")),
                    }
                }
            }
            StepKind::Docs => {
                if what == "Merge" {
                    let target = config.content_target(&step.subject).cloned();
                    let pr = step.pr;
                    let step = &mut job.steps[i];
                    step.writing = None;
                    if let (Some(target), Some(number)) = (target, pr) {
                        if content::pull_request_state(&target.repo, number)? == content::PrState::Merged {
                            ctx.ok(format!("  PR #{number} war gemergt"));
                            step.set(StepState::Done, format!("PR #{number} erstellt und gemergt (nach der Unterbrechung bei GitHub nachgelesen)"), now());
                            update_target_state(ctx, &target.id, |s| s.published = s.delivered.clone().map(|d| Delivery { pr: None, ..d }));
                        }
                    }
                } else {
                    // Creating the branch and pull request picks up what an earlier attempt left.
                    job.steps[i].writing = None;
                }
            }
            StepKind::Wiki => {
                let target = config.content_target(&step.subject).cloned();
                let base = step.content.as_ref().map(|p| (p.base_branch.clone(), p.base_commit.clone()));
                let commit = step.commit.clone();
                let step = &mut job.steps[i];
                step.writing = None;
                if let (Some(target), Some((branch, base)), Some(commit)) = (target, base, commit) {
                    match content::wiki_head(&content::wiki_remote(&target.repo), &branch)? {
                        Some(head) if head == commit => {
                            ctx.ok(format!("  Wiki-Commit {} war veröffentlicht", content::short(&commit)));
                            step.links.push(Link { label: "Wiki".into(), url: format!("https://github.com/{}/wiki", target.repo) });
                            step.set(StepState::Done, format!("Wiki-Commit {} veröffentlicht (nach der Unterbrechung nachgelesen)", content::short(&commit)), now());
                        }
                        Some(head) if head == base => step.commit = None,
                        _ => step.set(StepState::Blocked, "Das Wiki wurde inzwischen geändert; neu vorbereiten", now()),
                    }
                }
            }
            // An upload compares with what is on the server; doing it again is safe.
            StepKind::Website | StepKind::Care => job.steps[i].writing = None,
        }
    }
    Ok(())
}

// MARK: Care

/// The care plan: GitHub's state of the repositories (read only), then Claude judges the open pull
/// requests. Ends with a plan to approve, or nothing to do.
fn care_scan(ctx: &Ctx, job: &mut Job, i: usize) -> Result<()> {
    if github::token().is_none() {
        bail!("Für die Pflege die GitHub-CLI anmelden: gh auth login");
    }
    let options = job.options.clone();
    let current = current_targets(ctx);
    let targets: Vec<RepoTarget> = job.repos.iter().filter_map(|key| current.iter().find(|t| &t.key == key).cloned()).collect();
    let names: Vec<&str> = targets.iter().map(|t| t.repo.as_str()).collect();
    ctx.info(format!("Lese den Stand auf GitHub (nur lesend): {}", names.join(", ")));
    let mut plans: Vec<Plan> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|target| {
                scope.spawn(move || {
                    if !target.enabled {
                        return Plan { name: target.name(), key: target.key.clone(), repo: target.repo.clone(), error: Some("deaktiviert".into()), ..Plan::default() };
                    }
                    repocare::scan(target, options.prs, options.cleanup)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("scan thread panicked")).collect()
    });
    for key in job.repos.iter().filter(|k| !targets.iter().any(|t| &t.key == *k)) {
        plans.push(Plan { name: key.clone(), key: key.clone(), repo: key.clone(), error: Some("wird nicht mehr verwaltet".into()), ..Plan::default() });
    }
    if options.prs && plans.iter().any(|p| !p.prs.is_empty()) {
        steps::update_claude(ctx, &std::env::temp_dir())?;
    }
    let (model, effort, budget) = claude_settings(ctx, &[options.model.as_str()]);
    for plan in &mut plans {
        ctx.runner.check()?;
        ctx.cmd(format!("{} · {}", plan.name, plan.repo));
        if let Some(error) = &plan.error {
            ctx.warn(format!("  {error}"));
            continue;
        }
        if options.prs && !plan.prs.is_empty() {
            match review(ctx, plan, model.as_deref(), effort.as_deref(), budget) {
                Ok(()) => {}
                Err(e) if ctx.runner.is_cancelled() => return Err(e),
                Err(e) => {
                    ctx.warn(format!("  Claude-Prüfung fehlgeschlagen: {e:#}"));
                    plan.review_note = Some(format!("Claude-Prüfung fehlgeschlagen ({e:#}); alle PRs bleiben offen."));
                    repocare::apply_review(plan, &[]);
                }
            }
            for pr in &plan.prs {
                let (kind, what) = match pr.action {
                    Action::Merge => ("ok", "mergen"),
                    Action::Close => ("warn", "schließen"),
                    Action::Rebase => ("info", "Rebase anstoßen"),
                    Action::Keep => ("info", "bleibt offen"),
                };
                ctx.log(kind, format!("  PR #{} {}: {what} – {}", pr.number, pr.title, pr.reason));
            }
        }
        repocare::finish(plan, options.cleanup, options.release);
        if options.release {
            match (&plan.tag, &plan.tag_note) {
                (Some(tag), _) => ctx.ok(format!("  Tag {tag} geplant (nach {})", plan.last_tag.as_deref().unwrap_or("?"))),
                (None, Some(note)) => ctx.info(format!("  kein Tag: {note}")),
                _ => {}
            }
        }
    }
    let set = PlanSet {
        id: crate::next_token(),
        created_at: now(),
        prs: options.prs,
        cleanup: options.cleanup,
        release: options.release,
        model: options.model.clone(),
        targets: targets.iter().map(|t| (t.key.clone(), t.fingerprint())).collect(),
        plans,
    };
    job.items = jobs::care_items(&set);
    let summary = plan_summary(&set.plans);
    let unreadable = set.plans.iter().filter(|p| p.error.is_some()).count();
    let all_unreadable = unreadable == set.plans.len();
    job.plan = Some(set);
    let note = if unreadable > 0 { format!("; {unreadable} Repository/ies nicht lesbar") } else { String::new() };
    let step = &mut job.steps[i];
    if all_unreadable {
        step.set(StepState::Failed, "Kein Repository lesbar; Details im Plan", now());
    } else if job.items.is_empty() {
        step.set(StepState::NothingToDo, format!("Nichts zu tun{note}"), now());
    } else {
        step.set(StepState::NeedsApproval, format!("Plan: {summary}{note}"), now());
    }
    Ok(())
}

/// `3 PRs mergen, 1 Tag, 5 Branches löschen`, or `nichts zu tun`.
pub fn plan_summary(plans: &[Plan]) -> String {
    let sum = |f: &dyn Fn(&Plan) -> usize| plans.iter().map(f).sum::<usize>();
    let parts: Vec<String> = [
        (sum(&|p| p.count(Action::Merge)), "PR mergen", "PRs mergen"),
        (sum(&|p| p.count(Action::Close)), "PR schließen", "PRs schließen"),
        (sum(&|p| p.count(Action::Rebase)), "Rebase", "Rebases"),
        (sum(&|p| p.tag.is_some() as usize), "Tag", "Tags"),
        (sum(&|p| p.branches.len() + p.local_branches.len()), "Branch löschen", "Branches löschen"),
        (sum(&|p| p.prs.iter().filter(|pr| pr.optional).count()), "PR zur Wahl", "PRs zur Wahl"),
    ]
    .into_iter()
    .filter(|(n, ..)| *n > 0)
    .map(|(n, one, many)| format!("{n} {}", if n == 1 { one } else { many }))
    .collect();
    if parts.is_empty() {
        "nichts zu tun".into()
    } else {
        parts.join(", ")
    }
}

/// Claude judges the open pull requests of one repository, read-only, in its local clone if there is one.
fn review(ctx: &Ctx, plan: &mut Plan, model: Option<&str>, effort: Option<&str>, budget: Option<f64>) -> Result<()> {
    let dir = match &plan.clone {
        Some(dir) => PathBuf::from(dir),
        None => {
            let dir = std::env::temp_dir().join("OpenReleasePilot-Review");
            std::fs::create_dir_all(&dir).context("Arbeitsordner für die Prüfung anlegen")?;
            dir
        }
    };
    let prompt = claude::review_prompt(plan);
    let args = claude::review_args(&prompt, model, effort, budget);
    ctx.cmd(format!("  $ claude -p … --model {} (prüft {} PRs, nur lesend)", model.unwrap_or("Standard"), plan.prs.len()));
    let mut outcome = None;
    // No GITHUB_TOKEN: gh reads with its own login; the allowlist keeps Claude to reading.
    let code = shell::stream("claude", &args, &dir, &HashMap::new(), &ctx.runner, |line| match claude::parse(line) {
        Event::Line(text) => ctx.log("claude", format!("  {text}")),
        Event::Done(done) => outcome = Some(done),
        Event::Ignore => {}
    })?;
    let outcome = outcome.ok_or_else(|| anyhow!("Claude Code hat ohne Ergebnis beendet (Exit-Code {code})"))?;
    plan.review_cost = outcome.cost_usd;
    if !outcome.success {
        bail!("{}", outcome.text);
    }
    let review: repocare::Review = serde_json::from_value(outcome.structured.context("Claude hat keine strukturierte Antwort geliefert")?)
        .context("Claudes Antwort passt nicht zum Schema")?;
    let problems = repocare::apply_review(plan, &review.prs);
    for problem in &problems {
        ctx.warn(format!("  Bewertung verworfen: {problem}"));
    }
    if !problems.is_empty() {
        plan.review_note = Some(format!("Nicht verwendbare Bewertungen, die betroffenen PRs bleiben offen: {}", problems.join("; ")));
    }
    Ok(())
}

/// Carries out the approved items one by one, each checked again right before it writes (see
/// `repocare`). Every outcome is stored before the next item starts.
fn care_run(ctx: &Ctx, job: &mut Job, i: usize) -> Result<()> {
    if github::token().is_none() {
        bail!("Für die Pflege die GitHub-CLI anmelden: gh auth login");
    }
    let set = job.plan.clone().context("Kein Pflegeplan vorhanden")?;
    if let Err(why) = set.check_binding(&current_targets(ctx)) {
        job.steps[i].set(StepState::Blocked, why, now());
        return Ok(());
    }
    let mut last_repo = String::new();
    for n in 0..job.items.len() {
        if job.items[n].outcome.is_some() || job.items[n].kind == ItemKind::Tag {
            continue;
        }
        if ctx.runner.is_cancelled() {
            break;
        }
        let item = job.items[n].clone();
        let Some(plan) = set.plans.iter().find(|p| p.key == item.repo) else {
            job.items[n].outcome = Some(ItemOutcome::Skipped);
            job.items[n].detail = "nicht im Plan".into();
            continue;
        };
        if item.repo != last_repo {
            ctx.cmd(format!("{} · {}", plan.name, plan.repo));
            last_repo = item.repo.clone();
        }
        job.items[n].writing = true;
        save(ctx, job)?;
        let state = job.run_states.entry(item.repo.clone()).or_insert_with(|| RunState::new(plan));
        let result = execute(plan, &item, state);
        let (outcome, text) = match result {
            Ok(Done::Ok(text)) => (ItemOutcome::Done, text),
            Ok(Done::Skipped(text) | Done::Pending(text) | Done::Nothing(text)) => (ItemOutcome::Skipped, text),
            Err(e) if ctx.runner.is_cancelled() => (ItemOutcome::Cancelled, format!("abgebrochen ({e:#})")),
            Err(e) => (ItemOutcome::Failed, format!("{e:#}")),
        };
        if item.kind == ItemKind::Merge && outcome != ItemOutcome::Done {
            state.missed.push(item.reference.parse().unwrap_or(0));
        }
        let context = if item.context.is_empty() { String::new() } else { format!("{} – ", item.context) };
        ctx.log(
            match outcome {
                ItemOutcome::Done => "ok",
                ItemOutcome::Failed => "error",
                _ => "warn",
            },
            format!("  {}: {context}{text}", item.label),
        );
        let entry = &mut job.items[n];
        entry.outcome = Some(outcome);
        entry.detail = text;
        entry.writing = false;
        save(ctx, job)?;
    }
    if ctx.runner.is_cancelled() {
        job.steps[i].set(StepState::Cancelled, "abgebrochen; Erledigtes bleibt", now());
        return Ok(());
    }
    let counted: Vec<&CareItem> = job.items.iter().filter(|i| i.kind != ItemKind::Tag && i.outcome != Some(ItemOutcome::Excluded)).collect();
    let count = |o: ItemOutcome| counted.iter().filter(|i| i.outcome == Some(o)).count();
    let (done, skipped, failed) = (count(ItemOutcome::Done), count(ItemOutcome::Skipped), count(ItemOutcome::Failed));
    let mut parts = Vec::new();
    for (kind, one, many) in [
        (ItemKind::Merge, "PR gemergt", "PRs gemergt"),
        (ItemKind::Close, "PR geschlossen", "PRs geschlossen"),
        (ItemKind::Rebase, "Rebase angestoßen", "Rebases angestoßen"),
        (ItemKind::Branch, "Branch gelöscht", "Branches gelöscht"),
        (ItemKind::LocalBranch, "lokaler Branch gelöscht", "lokale Branches gelöscht"),
    ] {
        let n = counted.iter().filter(|i| i.kind == kind && i.outcome == Some(ItemOutcome::Done)).count();
        if n > 0 {
            parts.push(format!("{n} {}", if n == 1 { one } else { many }));
        }
    }
    for (n, what) in [(skipped, "übersprungen"), (failed, "fehlgeschlagen")] {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    }
    let summary = if parts.is_empty() { "nichts ausgeführt".to_owned() } else { parts.join(", ") };
    let state = if counted.is_empty() {
        StepState::NothingToDo
    } else if failed > 0 && done == 0 {
        StepState::Failed
    } else if failed + skipped > 0 {
        StepState::Partial
    } else {
        StepState::Done
    };
    job.steps[i].set(state, summary, now());
    crate::refresh_status(&ctx.app, &job.repos);
    Ok(())
}

fn execute(plan: &Plan, item: &CareItem, state: &mut RunState) -> Result<Done> {
    let pr = || item.reference.parse::<u64>().ok().and_then(|n| plan.prs.iter().find(|p| p.number == n)).context("PR nicht im Plan");
    match item.kind {
        ItemKind::Close => repocare::close(plan, pr()?, item.commented),
        ItemKind::Merge => repocare::merge(plan, pr()?, state),
        ItemKind::Rebase => repocare::rebase(plan, pr()?),
        ItemKind::Branch => repocare::delete_branch(plan, plan.branches.iter().find(|b| b.name == item.reference).context("Branch nicht im Plan")?, state),
        ItemKind::LocalSync => repocare::sync_local(plan, state),
        ItemKind::LocalBranch => repocare::delete_local_branch(plan, plan.local_branches.iter().find(|b| b.name == item.reference).context("Branch nicht im Plan")?, state),
        ItemKind::Tag => Ok(Done::Skipped("im Release-Schritt".into())),
    }
}

// MARK: Release

/// The planned tag once the CI of exactly its commit passed, then (if the release rule says the
/// repository publishes releases) the wait for that release.
fn release(ctx: &Ctx, job: &mut Job, i: usize) -> Result<()> {
    let key = job.steps[i].subject.clone();
    let config = ctx.config();
    let Some(managed) = config.repository(&key).cloned() else {
        job.steps[i].set(StepState::Failed, "wird nicht mehr verwaltet", now());
        return Ok(());
    };
    let Some(rule) = managed.release.clone() else {
        job.steps[i].set(StepState::Failed, "keine Release-Regel mehr eingerichtet", now());
        return Ok(());
    };
    let plan = job.plan.as_ref().and_then(|s| s.plans.iter().find(|p| p.key == key)).cloned();
    if job.steps[i].tag.is_none() {
        let Some(plan) = plan else {
            job.steps[i].set(StepState::NothingToDo, "kein Pflegeplan, also kein Tag", now());
            return Ok(());
        };
        if let Some(error) = &plan.error {
            job.steps[i].set(StepState::Skipped, format!("Repository nicht lesbar: {error}"), now());
            return Ok(());
        }
        let Some(tag) = plan.tag.clone() else {
            let why = plan.tag_note.clone().unwrap_or_else(|| "kein Tag geplant".into());
            job.steps[i].set(StepState::NothingToDo, format!("kein Tag: {why}"), now());
            return Ok(());
        };
        if let Err(why) = job.plan.as_ref().expect("plan exists").check_binding(&current_targets(ctx)) {
            job.steps[i].set(StepState::Blocked, why, now());
            return Ok(());
        }
        let state = job.run_states.get(&key).cloned().unwrap_or_else(|| RunState::new(&plan));
        job.steps[i].writing = Some(format!("Tag {tag}"));
        save(ctx, job)?;
        let progress = |text: &str| ctx.info(format!("  {}: {text}", plan.name));
        let result = repocare::tag(&plan, &state, &ctx.runner, &CiWait::default(), &progress);
        job.steps[i].writing = None;
        let tag_item = job.items.iter().position(|it| it.kind == ItemKind::Tag && it.repo == key);
        let mut mark = |outcome: ItemOutcome, text: &str| {
            if let Some(n) = tag_item {
                job.items[n].outcome = Some(outcome);
                job.items[n].detail = text.to_owned();
            }
        };
        match result {
            Ok(Done::Ok(text)) => {
                mark(ItemOutcome::Done, &text);
                ctx.ok(format!("  Tag {tag}: {text}"));
                let step = &mut job.steps[i];
                step.links.push(Link { label: format!("Tag {tag}"), url: format!("https://github.com/{}/releases/tag/{tag}", managed.repo) });
                step.tag = Some(tag);
                step.commit = state.expected_head.clone();
                step.detail = text;
                save(ctx, job)?;
            }
            Ok(Done::Pending(text)) => {
                ctx.warn(format!("  {text}"));
                job.steps[i].set(StepState::Waiting, text, now());
                return Ok(());
            }
            Ok(Done::Nothing(text)) => {
                mark(ItemOutcome::Done, &text);
                ctx.info(format!("  Tag {tag}: {text}"));
                job.steps[i].set(StepState::NothingToDo, format!("kein Tag: {text}"), now());
                return Ok(());
            }
            Ok(Done::Skipped(text)) => {
                mark(ItemOutcome::Skipped, &text);
                ctx.warn(format!("  Tag {tag}: {text}"));
                job.steps[i].set(StepState::Skipped, format!("kein Tag: {text}"), now());
                return Ok(());
            }
            Err(e) => {
                mark(ItemOutcome::Failed, &format!("{e:#}"));
                return Err(e);
            }
        }
    }
    let tag = job.steps[i].tag.clone().expect("set above");
    let Some(commit) = job.steps[i].commit.clone() else {
        job.steps[i].set(StepState::Failed, format!("Commit von {tag} unbekannt"), now());
        return Ok(());
    };
    if !rule.publishes_release {
        job.steps[i].set(StepState::Done, format!("Tag {tag} gesetzt; laut Release-Regel veröffentlicht das Repository kein GitHub-Release"), now());
        return Ok(());
    }
    let progress = |text: &str| ctx.info(format!("  {text}"));
    match repocare::wait_for_release(&managed.repo, &tag, &commit, &ctx.runner, &CiWait::default(), &progress)? {
        ReleaseWait::Published { url } => {
            ctx.ok(format!("  Release {tag} veröffentlicht"));
            job.steps[i].links.push(Link { label: format!("Release {tag}"), url });
            job.steps[i].set(StepState::Done, format!("Tag {tag} gesetzt, Release veröffentlicht"), now());
        }
        ReleaseWait::Failed(why) => job.steps[i].set(StepState::Failed, format!("Tag {tag} gesetzt, aber kein Release: {why}"), now()),
        ReleaseWait::WorkflowFailed(why) => job.steps[i].set(
            StepState::Waiting,
            format!("Tag {tag} gesetzt, Release noch nicht veröffentlicht: {why}. Workflow auf GitHub erneut starten, dann erneut prüfen"),
            now(),
        ),
        ReleaseWait::Pending(why) => job.steps[i].set(StepState::Waiting, format!("Tag {tag} gesetzt; {why}. Release-Workflow prüfen, dann erneut prüfen"), now()),
    }
    Ok(())
}

// MARK: Content targets

fn update_target_state(ctx: &Ctx, id: &str, change: impl FnOnce(&mut content::TargetState)) {
    let states = with_model(&ctx.app, |m| {
        change(m.content_states.entry(id.to_owned()).or_default());
        m.content_states.clone()
    });
    if let Err(e) = content::save_states(&states) {
        ctx.warn(format!("Stand der Inhaltsziele nicht gespeichert: {e:#}"));
    }
}

/// Prepares a README/docs or wiki change in the app's work folder: sources pinned, Claude edits,
/// the app checks what changed. Ends with a proposal to approve, nothing to do, or why not.
fn content_prepare(ctx: &Ctx, job: &mut Job, i: usize) -> Result<()> {
    let config = ctx.config();
    let target = config.content_target(&job.steps[i].subject).cloned().context("Das Ziel gibt es nicht mehr")?;
    let managed: Vec<String> = config.repositories.iter().map(|r| r.key()).collect();
    content::validate(&target, &managed).map_err(|why| anyhow!("{}: {why}", target.name))?;
    let basis = job.steps[i].basis.unwrap_or(target.basis);
    ctx.info(format!("Quellen festhalten ({}):", if basis == Basis::Release { "neuestes Release" } else { "aktueller Stand" }));
    let mut sources = Vec::new();
    for key in &target.sources {
        let repo = config.repository(key).map(|r| r.repo.clone()).with_context(|| format!("{key} wird nicht verwaltet"))?;
        let rev = content::pin_source(key, &repo, basis)?;
        ctx.info(format!("  {} ({})", rev.describe(), content::short(&rev.rev)));
        sources.push(rev);
    }
    let work = content::work_dir(&job.id, &target.id);
    let (remote, branch) = match target.kind {
        TargetKind::Docs => (content::github_remote(&target.repo), Some(target.branch.trim()).filter(|b| !b.is_empty())),
        TargetKind::Wiki => (content::wiki_remote(&target.repo), None),
    };
    ctx.cmd(format!("$ git clone {remote} (Arbeitsordner der App, nicht deine lokale Kopie)"));
    let prepared = content::prepare(&work, &remote, branch, &sources, &|s| content::github_remote(&s.repo)).map_err(|e| {
        if target.kind == TargetKind::Wiki {
            e.context(format!("Wiki von {} nicht lesbar; gibt es schon eine erste Seite?", target.repo))
        } else {
            e
        }
    })?;
    ctx.info(format!("Ausgangsstand: {} {}", prepared.base_branch, content::short(&prepared.base_commit)));
    let previous = with_model(&ctx.app, |m| m.content_states.get(&target.id).and_then(|s| s.published.clone().or_else(|| s.delivered.clone())));
    let infos: Vec<ContentSource> = prepared
        .sources
        .iter()
        .map(|(rev, dir)| {
            let before = previous.as_ref().and_then(|d| d.sources.iter().find(|s| s.key == rev.key)).cloned();
            let commits = match &before {
                Some(b) if b.rev != rev.rev => content::commits_between(&rev.repo, &b.rev, &rev.rev),
                _ => Vec::new(),
            };
            ContentSource { rev, dir, previous: before.map(|b| b.label), commits }
        })
        .collect();
    let prompt = claude::content_prompt(&target, &prepared, &infos, &job.options.instructions);
    let (model, effort, budget) = claude_settings(ctx, &[target.model.as_str(), job.options.model.as_str()]);
    steps::update_claude(ctx, &prepared.dir)?;
    let dirs: Vec<&Path> = prepared.sources.iter().map(|(_, d)| d.as_path()).collect();
    let args = claude::content_args(&prompt, &dirs, model.as_deref(), effort.as_deref(), budget);
    ctx.cmd(format!("$ claude -p … --model {} (ändert nur im Arbeitsordner; Prompt unten)", model.as_deref().unwrap_or("Standard")));
    for line in prompt.lines() {
        ctx.log("claude", format!("  {line}"));
    }
    let mut outcome = None;
    let code = shell::stream("claude", &args, &prepared.dir, &HashMap::new(), &ctx.runner, |line| match claude::parse(line) {
        Event::Line(text) => ctx.log("claude", text),
        Event::Done(done) => outcome = Some(done),
        Event::Ignore => {}
    })?;
    let outcome = outcome.ok_or_else(|| anyhow!("Claude Code hat ohne Ergebnis beendet (Exit-Code {code})"))?;
    for denial in &outcome.denials {
        ctx.warn(format!("Nicht freigegeben: {denial}"));
    }
    if let Some(cost) = outcome.cost_usd {
        ctx.info(format!("Claude: {} Runden, {:.2} USD", outcome.turns.unwrap_or(0), cost));
    }
    if !outcome.success {
        bail!("Claude hat nicht erfolgreich abgeschlossen: {}", outcome.text.lines().next().unwrap_or("ohne Meldung"));
    }
    let revisions: Vec<content::SourceRev> = prepared.sources.iter().map(|(s, _)| s.clone()).collect();
    update_target_state(ctx, &target.id, |s| {
        s.prepared = Some(Delivery { job: job.id.clone(), at: now(), sources: revisions.clone(), what: String::new(), url: String::new(), pr: None })
    });
    let changes = match content::collect(&prepared, &target.paths)? {
        Err(problems) => {
            for problem in &problems {
                ctx.warn(format!("  verworfen: {problem}"));
            }
            job.steps[i].set(StepState::Failed, format!("Änderung verworfen: {}", problems.join("; ")), now());
            return Ok(());
        }
        Ok(changes) if changes.files.is_empty() => {
            let first = outcome.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_owned();
            job.steps[i].set(StepState::NothingToDo, format!("Claude hat nichts geändert{}", if first.is_empty() { String::new() } else { format!(": {first}") }), now());
            return Ok(());
        }
        Ok(changes) => changes,
    };
    let mut checks = content::checks(&prepared, &changes, target.kind);
    if !target.check.trim().is_empty() {
        ctx.cmd(format!("$ {}", target.check.trim()));
        checks.push(content::run_check(&prepared, target.check.trim(), &ctx.runner)?);
    }
    for check in &checks {
        ctx.log(if check.ok { "ok" } else { "warn" }, format!("  Prüfung {}: {}", check.name, check.detail));
    }
    let branch = if target.kind == TargetKind::Docs { content::branch_name(&job.id, &target.id, &prepared, &changes) } else { String::new() };
    let files = changes.files.len();
    let proposal = Proposal::new(&target, &prepared, changes, checks, outcome.text.clone(), outcome.cost_usd, branch);
    let passed = proposal.checks_passed();
    let failed: Vec<String> = proposal.checks.iter().filter(|c| !c.ok).map(|c| format!("{}: {}", c.name, c.detail)).collect();
    let step = &mut job.steps[i];
    step.content = Some(proposal);
    if passed {
        step.set(StepState::NeedsApproval, format!("{files} Datei(en) geändert; Freigabe nötig"), now());
    } else {
        step.set(StepState::Failed, format!("Prüfung nicht bestanden: {}", failed.join("; ")), now());
    }
    Ok(())
}

/// Publishes an approved proposal: docs as a branch and pull request (merged once green, if the
/// target says so), a wiki by a push without force. Checks first that nothing it rests on changed.
fn content_publish(ctx: &Ctx, job: &mut Job, i: usize) -> Result<()> {
    let config = ctx.config();
    let target = config.content_target(&job.steps[i].subject).cloned().context("Das Ziel gibt es nicht mehr")?;
    let proposal = job.steps[i].content.clone().context("Kein Vorschlag vorhanden")?;
    if job.steps[i].approved.as_deref() != Some(proposal.fingerprint.as_str()) {
        job.steps[i].set(StepState::Blocked, "Die Freigabe passt nicht zum Vorschlag; neu vorbereiten", now());
        return Ok(());
    }
    if let Some(why) = proposal.stale(&target) {
        job.steps[i].set(StepState::Blocked, format!("{why}; neu vorbereiten"), now());
        return Ok(());
    }
    if target.kind == TargetKind::Docs && job.steps[i].pr.is_none() {
        // An earlier, interrupted attempt may have opened the pull request already: take it, do not make another.
        if let Some(pr) = content::find_docs_delivery(&target.repo, &proposal)? {
            ctx.info(format!("PR #{} gab es schon (früherer Versuch); wird übernommen", pr.number));
            record_pull_request(ctx, job, i, &target, &proposal, pr)?;
        }
    }
    if job.steps[i].pr.is_none() {
        for source in proposal.sources.iter().filter(|s| s.basis == Basis::Release) {
            if let Some(latest) = github::latest_release(&source.repo)? {
                if latest != source.label {
                    let why = format!("{} hat inzwischen das Release {latest}, der Vorschlag beschreibt {}; neu vorbereiten", source.repo, source.label);
                    job.steps[i].set(StepState::Blocked, why, now());
                    return Ok(());
                }
            }
        }
    }
    let work = content::work_dir(&job.id, &target.id);
    let dir = work.join("target");
    match target.kind {
        TargetKind::Docs => {
            if job.steps[i].pr.is_none() {
                let (title, body) = content::pull_request_text(&target, &proposal, &job.id);
                ctx.info(format!("Branch {} und Pull Request in {} anlegen …", proposal.branch, target.repo));
                job.steps[i].writing = Some("Pull Request".into());
                save(ctx, job)?;
                let result = content::publish_docs(&target.repo, &dir, &proposal, &title, &body);
                job.steps[i].writing = None;
                let pr = result?;
                ctx.ok(format!("PR #{} erstellt: {}", pr.number, pr.url));
                record_pull_request(ctx, job, i, &target, &proposal, pr)?;
            }
            let number = job.steps[i].pr.expect("set above");
            if !target.merge_when_green {
                job.steps[i].set(StepState::Done, format!("PR #{number} erstellt; gemergt wird über die PR-Pflege oder auf GitHub"), now());
                return Ok(());
            }
            let pr = PullRequestRef { number, url: String::new(), commit: job.steps[i].commit.clone().context("Commit des PRs unbekannt")? };
            job.steps[i].writing = Some("Merge".into());
            save(ctx, job)?;
            let progress = |text: &str| ctx.info(format!("  {text}"));
            let result = content::merge_when_green(&target.repo, &pr, &ctx.runner, &CiWait::default(), &progress);
            job.steps[i].writing = None;
            match result? {
                Ok(text) => {
                    ctx.ok(format!("PR #{number} {text}"));
                    job.steps[i].set(StepState::Done, format!("PR #{number} erstellt und {text}"), now());
                    update_target_state(ctx, &target.id, |s| s.published = s.delivered.clone().map(|d| Delivery { pr: None, ..d }));
                }
                Err(why) => {
                    ctx.warn(format!("PR #{number} nicht gemergt: {why}"));
                    job.steps[i].set(StepState::Partial, format!("PR #{number} erstellt, nicht gemergt: {why}"), now());
                }
            }
        }
        TargetKind::Wiki => {
            let identity = content::commit_identity()?;
            let sources: Vec<String> = proposal.sources.iter().map(|s| s.describe()).collect();
            let message = format!("{}: aktualisiert für {}\n\nVon OpenReleasePilot (Auftrag {}), in der App geprüft und freigegeben.", target.name, sources.join(", "), job.id);
            ctx.info(format!("Wiki von {} frisch klonen, Stand prüfen, pushen (ohne Force) …", target.repo));
            job.steps[i].writing = Some("Wiki-Push".into());
            save(ctx, job)?;
            let remote = content::wiki_remote(&target.repo);
            let result = content::publish_wiki(&work, &remote, &dir, &proposal, &message, identity.as_ref(), &mut |commit| {
                job.steps[i].commit = Some(commit.to_owned());
                save(ctx, job)
            });
            job.steps[i].writing = None;
            let commit = result?;
            let url = format!("https://github.com/{}/wiki", target.repo);
            ctx.ok(format!("Wiki-Commit {} veröffentlicht", content::short(&commit)));
            job.steps[i].links.push(Link { label: "Wiki".into(), url: url.clone() });
            job.steps[i].set(StepState::Done, format!("Wiki-Commit {} veröffentlicht", content::short(&commit)), now());
            let delivery = Delivery { job: job.id.clone(), at: now(), sources: proposal.sources.clone(), what: format!("Wiki-Commit {}", content::short(&commit)), url, pr: None };
            update_target_state(ctx, &target.id, |s| {
                s.delivered = Some(delivery.clone());
                s.published = Some(delivery);
            });
        }
    }
    Ok(())
}

/// Notes the delivered pull request in the step and in the target's state.
fn record_pull_request(ctx: &Ctx, job: &mut Job, i: usize, target: &content::ContentTarget, proposal: &Proposal, pr: PullRequestRef) -> Result<()> {
    let step = &mut job.steps[i];
    step.pr = Some(pr.number);
    step.commit = Some(pr.commit.clone());
    step.links.push(Link { label: format!("PR #{}", pr.number), url: pr.url.clone() });
    save(ctx, job)?;
    let delivery = Delivery { job: job.id.clone(), at: now(), sources: proposal.sources.clone(), what: format!("PR #{}", pr.number), url: pr.url, pr: Some(pr.number) };
    update_target_state(ctx, &target.id, |s| s.delivered = Some(delivery));
    Ok(())
}

// MARK: Website

/// The website's update for the products of this job's repositories: Claude, build, a local commit.
/// Ends with that commit as the proposal.
fn website_prepare(ctx: &Ctx, job: &mut Job, i: usize) -> Result<()> {
    let project = ctx.project()?;
    let before = git::head(&project).unwrap_or_default();
    let update = steps::update_site(ctx, &job.options.instructions, Some(&job.repos))?;
    let Some((short_sha, files)) = update.commit else {
        job.steps[i].set(StepState::NothingToDo, update.summary, now());
        return Ok(());
    };
    let commit = git::head(&project)?;
    let stat = shell::output("git", &["show", "--stat", "--format=%s", &commit], &project).unwrap_or_default();
    let mut diff = shell::output("git", &["show", "--no-color", "--format=", &commit], &project).unwrap_or_default();
    const MAX: usize = 300 * 1024;
    if diff.len() > MAX {
        let mut cut = MAX;
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
        diff.push_str(&format!("\n… gekürzt; vollständig: git -C \"{}\" show {short_sha}", project.display()));
    }
    let step = &mut job.steps[i];
    step.website = Some(WebsiteProposal { base: before, commit: commit.clone(), stat, diff, summary: update.summary, products: update.products });
    step.set(StepState::NeedsApproval, format!("Commit {short_sha}: {files} Dateien; Freigabe zum Veröffentlichen nötig"), now());
    Ok(())
}

/// Publishes the approved website commit: push, upload, verify (the website's own steps).
fn website_publish(ctx: &Ctx, job: &mut Job, i: usize) -> Result<()> {
    let proposal = job.steps[i].website.clone().context("Kein Vorschlag vorhanden")?;
    if job.steps[i].approved.as_deref() != Some(proposal.commit.as_str()) {
        job.steps[i].set(StepState::Blocked, "Die Freigabe passt nicht zum Vorschlag; neu vorbereiten", now());
        return Ok(());
    }
    let project = ctx.project()?;
    let head = git::head(&project)?;
    if head != proposal.commit {
        let why = format!(
            "Im Website-Projekt steht HEAD auf {} statt auf dem freigegebenen {}; neu vorbereiten oder im Bereich Website veröffentlichen",
            content::short(&head),
            content::short(&proposal.commit)
        );
        job.steps[i].set(StepState::Blocked, why, now());
        return Ok(());
    }
    job.steps[i].writing = Some("Upload".into());
    save(ctx, job)?;
    let uploaded = steps::upload(ctx);
    job.steps[i].writing = None;
    let (_, upload_summary) = uploaded?;
    let site = ctx.site()?;
    job.steps[i].links.push(Link { label: site.url.replace("https://", "").replace("http://", ""), url: site.url.clone() });
    save(ctx, job)?;
    let (verified, verify_summary) = steps::verify(ctx)?;
    let state = if verified == Status::Error { StepState::Partial } else { StepState::Done };
    job.steps[i].set(state, format!("{upload_summary} · Verify: {verify_summary}"), now());
    Ok(())
}
