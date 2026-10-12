//! The website's four steps and the passes of care jobs (see `jobrun`). Each runs on its own
//! thread, writes log lines as it goes and ends with a status and a one-line summary.

use crate::{changed, load_site, with_model, Check, ClaudeRun, LogLine, Model, Status, Step, UploadResult};
use anyhow::{anyhow, bail, Context, Result};
use openreleasepilot_core::claude::{self, Event, Outdated, ProductRelease, ReleaseCheck};
use openreleasepilot_core::config::{AuthKind, Config};
use openreleasepilot_core::paths::expand_home;
use openreleasepilot_core::sftp::{self, ConnectError, Connection};
use openreleasepilot_core::shell::{self, Runner};
use openreleasepilot_core::site::{Site, Task};
use openreleasepilot_core::{git, github, jobs, localrepo, manifest, secrets, verify};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

const MAX_LOG_LINES: usize = 5000;

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `: a, b, c …` for the first names of a list.
pub fn preview(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let shown: Vec<&str> = names.iter().take(8).map(String::as_str).collect();
    format!(": {}{}", shown.join(", "), if names.len() > shown.len() { " …" } else { "" })
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub struct Ctx {
    pub(crate) app: AppHandle,
    step: Step,
    pub(crate) runner: Arc<Runner>,
    /// The care job this pass belongs to; its lines go to the job's own log.
    pub(crate) job: Option<String>,
}

impl Ctx {
    pub(crate) fn log(&self, kind: &'static str, text: impl Into<String>) {
        let text = strip_ansi(&text.into());
        let line = LogLine { kind, text };
        if let Some(job) = &self.job {
            let _ = jobs::append_log(job, &jobs::LogEntry { kind: kind.to_owned(), text: line.text.clone() });
            let _ = self.app.emit("log", serde_json::json!({ "step": self.step, "job": job, "line": line }));
            return;
        }
        with_model(&self.app, |m| {
            if let Some(state) = m.steps.get_mut(&self.step) {
                if state.log.len() >= MAX_LOG_LINES {
                    state.log.drain(..MAX_LOG_LINES / 10);
                }
                state.log.push(line.clone());
            }
        });
        let _ = self.app.emit("log", serde_json::json!({ "step": self.step, "line": line }));
    }
    pub(crate) fn info(&self, text: impl Into<String>) {
        self.log("info", text)
    }
    pub(crate) fn ok(&self, text: impl Into<String>) {
        self.log("ok", text)
    }
    pub(crate) fn warn(&self, text: impl Into<String>) {
        self.log("warn", text)
    }
    pub(crate) fn cmd(&self, text: impl Into<String>) {
        self.log("cmd", text)
    }

    pub(crate) fn config(&self) -> Config {
        with_model(&self.app, |m| m.config.clone())
    }

    pub(crate) fn project(&self) -> Result<PathBuf> {
        let dir = self.config().project_dir;
        if dir.trim().is_empty() {
            bail!("Keine Website eingerichtet (Projektordner fehlt)");
        }
        let path = expand_home(&dir);
        if !path.is_dir() {
            bail!("Projektordner {} nicht gefunden", path.display());
        }
        Ok(path)
    }

    /// Reloads site.json, so edits (e.g. a raised `documented`) are always current.
    pub(crate) fn site(&self) -> Result<Site> {
        let config = self.config();
        let site = load_site(&config);
        with_model(&self.app, |m| m.site = site.clone());
        site.map_err(|e| anyhow!(e))
    }
}

/// One unit of work; each shows up in the slot of its step.
pub enum Work {
    Precheck,
    Update(String),
    Upload,
    Verify,
    AddProduct(NewProductInput),
    /// One pass of a care job: runs until it needs a decision, waits or is finished.
    Job(String),
}

impl Work {
    fn step(&self) -> Step {
        match self {
            Work::Precheck => Step::Precheck,
            Work::Update(_) | Work::AddProduct(_) => Step::Update,
            Work::Upload => Step::Upload,
            Work::Verify => Step::Verify,
            Work::Job(_) => Step::Jobs,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewProductInput {
    pub repo: String,
    pub name: String,
    /// Absolute path of the local clone, or empty.
    pub local_path: String,
    pub instructions: String,
    /// Model alias; empty = the site's default.
    #[serde(default)]
    pub model: String,
}

/// One update task with the user's choice applied.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedTask {
    #[serde(skip)]
    pub task: Task,
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub skill: String,
    pub enabled: bool,
    /// The user's model choice; empty = default.
    pub chosen_model: String,
    /// Model without a choice: task, else site; `None` = Claude Code's default.
    pub default_model: Option<String>,
    /// What the run uses.
    pub model: Option<String>,
    pub effort: Option<String>,
}

pub fn resolve_tasks(site: &Site, config: &Config) -> Vec<ResolvedTask> {
    site.update_tasks()
        .into_iter()
        .map(|task| {
            let choice = config.tasks.get(&task.id).cloned().unwrap_or_default();
            let default_model = task.model.clone().or_else(|| site.claude.model.clone());
            let model = Some(choice.model.clone()).filter(|m| !m.is_empty()).or_else(|| default_model.clone());
            let effort = task.effort.clone().or_else(|| site.claude.effort.clone());
            ResolvedTask {
                id: task.id.clone(),
                label: task.label.clone(),
                description: task.description.clone(),
                skill: task.skill.clone(),
                enabled: choice.enabled.unwrap_or(task.default),
                chosen_model: choice.model,
                default_model,
                model,
                effort,
                task,
            }
        })
        .collect()
}

/// One watched product with the user's choice and what the last release lookup found.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductView {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub documented: String,
    pub enabled: bool,
    /// Newest release tag, once a Precheck or Update has looked it up.
    pub latest_tag: Option<String>,
    /// The newest release is newer than the documented one.
    pub outdated: bool,
    /// `localPath` from site.json, if set.
    pub local_path: Option<String>,
    /// The local clone exists.
    pub local: bool,
}

pub fn resolve_products(site: &Site, config: &Config, latest_tags: &std::collections::BTreeMap<String, String>) -> Vec<ProductView> {
    let project = expand_home(&config.project_dir);
    site.products
        .iter()
        .map(|p| {
            let latest_tag = latest_tags.get(&p.id).cloned();
            ProductView {
                id: p.id.clone(),
                name: p.name.clone(),
                repo: p.repo.clone(),
                documented: p.documented.clone(),
                enabled: config.product_enabled(&p.id),
                outdated: latest_tag.as_deref().is_some_and(|tag| github::is_newer(tag, &p.documented)),
                latest_tag,
                local: p.local_path.as_deref().is_some_and(|path| project.join(path).is_dir()),
                local_path: p.local_path.clone(),
            }
        })
        .collect()
}

/// `runner` is set as soon as a start is accepted, `running` only once its thread runs: either means busy.
fn busy(runner: Option<&Arc<Runner>>, running: Option<Step>) -> bool {
    runner.is_some() || running.is_some()
}

/// Runs the work one after another on a background thread; stops at the first error.
pub fn start(app: &AppHandle, work: Vec<Work>) -> Result<(), String> {
    start_with(app, |_| Ok(work))
}

/// Like `start`, but `work` is built under the same lock that marks the app busy: checking a job's
/// state and starting its pass happen together, so a second click finds the app busy.
pub fn start_with(app: &AppHandle, work: impl FnOnce(&mut Model) -> Result<Vec<Work>, String>) -> Result<(), String> {
    let runner = Arc::new(Runner::default());
    let work = with_model(app, |m| {
        if busy(m.runner.as_ref(), m.running) {
            return Err("Es läuft bereits ein Schritt oder Auftrag.".to_owned());
        }
        let work = work(m)?;
        m.runner = Some(runner.clone());
        m.error = None;
        m.notice = None;
        if let Some(Work::Job(id)) = work.first() {
            m.job_running = Some(id.clone());
        }
        Ok(work)
    })?;
    let app = app.clone();
    std::thread::spawn(move || {
        let all = work.len() > 1;
        for unit in work {
            if let (true, Work::Update(instructions)) = (all, &unit) {
                if with_model(&app, |m| ReleaseCheck::nothing_to_do(m.outdated.as_ref(), instructions)) {
                    continue;
                }
            }
            let step = unit.step();
            let job = match &unit {
                Work::Job(id) => Some(id.clone()),
                _ => None,
            };
            let ctx = Ctx { app: app.clone(), step, runner: runner.clone(), job };
            with_model(&app, |m| {
                m.running = Some(step);
                let state = m.steps.entry(step).or_default();
                state.status = Status::Running;
                state.summary.clear();
                state.stale = false;
                state.log.clear();
                // Results of an earlier run must not show up under the new status.
                match step {
                    Step::Precheck => m.checks.clear(),
                    Step::Update => m.claude_runs.clear(),
                    Step::Upload => m.upload = None,
                    Step::Verify => m.verify = None,
                    Step::Jobs => {}
                }
            });
            changed(&app);
            let result = match &unit {
                Work::Precheck => precheck(&ctx),
                Work::Update(instructions) => update(&ctx, instructions),
                Work::Upload => upload(&ctx),
                Work::Verify => verify(&ctx),
                Work::AddProduct(input) => add_product(&ctx, input),
                Work::Job(id) => crate::jobrun::pass(&ctx, id),
            };
            let (status, summary) = match result {
                Ok(done) => done,
                Err(e) => {
                    let message = format!("{e:#}");
                    ctx.log("error", message.clone());
                    (Status::Error, message)
                }
            };
            with_model(&app, |m| {
                let state = m.steps.entry(step).or_default();
                state.status = status;
                state.summary = summary;
                state.finished_at = Some(now());
                if matches!(step, Step::Update | Step::Upload) {
                    // Commits, pushes and uploads change what the precheck saw.
                    if let Some(precheck) = m.steps.get_mut(&Step::Precheck) {
                        precheck.stale = precheck.status != Status::Idle;
                    }
                }
            });
            changed(&app);
            if status == Status::Error || runner.is_cancelled() {
                break;
            }
        }
        with_model(&app, |m| {
            m.running = None;
            m.runner = None;
            m.job_running = None;
        });
        changed(&app);
    });
    Ok(())
}

fn github_env() -> HashMap<String, String> {
    github::token().map(|t| HashMap::from([("GITHUB_TOKEN".to_owned(), t.to_owned())])).unwrap_or_default()
}

/// Runs the build command; lines matching `failOnOutput` fail it even with exit code 0.
fn build(ctx: &Ctx, site: &Site, project: &std::path::Path) -> Result<()> {
    ctx.cmd(format!("$ {}", site.build));
    let mut flagged = Vec::new();
    let code = shell::stream_shell(&site.build, project, &github_env(), &ctx.runner, |line| {
        if site.fail_on_output.iter().any(|pattern| line.contains(pattern.as_str())) {
            flagged.push(line.to_owned());
            ctx.warn(line);
        } else {
            ctx.log("out", line);
        }
    })?;
    if code != 0 {
        bail!("Build fehlgeschlagen (Exit-Code {code})");
    }
    if !flagged.is_empty() {
        bail!("Build-Ausgabe meldet ein Problem: {}", flagged[0]);
    }
    if !site.output_dir(project).is_dir() {
        bail!("Ausgabeordner {} fehlt nach dem Build", site.output);
    }
    ctx.ok("Build erfolgreich");
    Ok(())
}

fn connect(ctx: &Ctx) -> Result<Connection> {
    let config = ctx.config();
    if !config.sftp.is_complete() {
        bail!("SFTP ist nicht vollständig eingerichtet");
    }
    let secret = match config.sftp.auth {
        AuthKind::Password => Some(secrets::get(&config.sftp).context("Passwort nicht im Schlüsselbund gefunden")?),
        AuthKind::Key => secrets::get(&config.sftp),
    };
    match sftp::connect(&config.sftp, secret.as_deref()) {
        Ok(connection) => Ok(connection),
        Err(ConnectError::UnknownHostKey(fingerprint)) => {
            with_model(&ctx.app, |m| m.pending_host_key = Some(fingerprint.clone()));
            changed(&ctx.app);
            bail!("Erste Verbindung: Server-Schlüssel {fingerprint} bestätigen, dann den Schritt erneut starten")
        }
        Err(e) => Err(anyhow!(e.to_string())),
    }
}

/// Looks up the newest release of every selected product in scope; products the user turned off
/// are skipped. A failed lookup lands in `unknown`, never among the current ones.
fn find_outdated(ctx: &Ctx, site: &Site, in_scope: &dyn Fn(&openreleasepilot_core::site::Product) -> bool, mut on_check: impl FnMut(Check)) -> ReleaseCheck {
    let config = ctx.config();
    let mut check = ReleaseCheck::default();
    for product in site.products.iter().filter(|p| in_scope(p)) {
        let label = format!("Release {}", product.name);
        if !config.product_enabled(&product.id) {
            ctx.info(format!("{}: nicht ausgewählt, übersprungen", product.name));
            on_check(Check { label, status: Status::Idle, detail: "nicht ausgewählt".into() });
            continue;
        }
        match ProductRelease::classify(product, github::latest_release(&product.repo)) {
            ProductRelease::Newer(tag) => {
                with_model(&ctx.app, |m| m.latest_tags.insert(product.id.clone(), tag.clone()));
                let o = Outdated::new(product, tag);
                ctx.warn(format!("{}: dokumentiert {}, neuestes Release {}", product.name, o.documented, o.latest_version()));
                on_check(Check { label, status: Status::Warning, detail: format!("{} → {}", o.documented, o.latest_version()) });
                check.outdated.push(o);
            }
            ProductRelease::Current(tag) => {
                with_model(&ctx.app, |m| m.latest_tags.insert(product.id.clone(), tag.clone()));
                ctx.ok(format!("{}: {} ist aktuell", product.name, product.documented));
                on_check(Check { label, status: Status::Ok, detail: format!("{} aktuell", tag.trim_start_matches('v')) });
            }
            ProductRelease::NoRelease => {
                ctx.info(format!("{}: noch kein veröffentlichtes Release", product.name));
                on_check(Check { label, status: Status::Idle, detail: "noch kein veröffentlichtes Release".into() });
            }
            ProductRelease::Unknown(why) => {
                ctx.warn(format!("{}: Release-Stand unbekannt: {why}", product.name));
                on_check(Check { label, status: Status::Warning, detail: format!("unbekannt: {why}") });
                check.unknown.push(format!("{}: {why}", product.name));
            }
        }
    }
    check
}

/// Stops when the release state of a selected product is unknown: an update (and the upload after
/// it) must not rest on a lookup that failed.
fn require_complete(check: &ReleaseCheck) -> Result<()> {
    if !check.complete() {
        bail!(
            "Release-Stand unklar ({}). Update gestoppt, damit nichts auf unvollständiger Grundlage geändert oder veröffentlicht wird; später erneut versuchen.",
            check.unknown.join("; ")
        );
    }
    Ok(())
}

// MARK: Precheck

fn precheck(ctx: &Ctx) -> Result<(Status, String)> {
    let mut checks: Vec<Check> = Vec::new();
    let push = |checks: &mut Vec<Check>, label: &str, status: Status, detail: String| {
        match status {
            Status::Ok => ctx.ok(format!("{label}: {detail}")),
            Status::Warning => ctx.warn(format!("{label}: {detail}")),
            Status::Error => ctx.log("error", format!("{label}: {detail}")),
            _ => ctx.info(format!("{label}: {detail}")),
        }
        checks.push(Check { label: label.to_owned(), status, detail });
    };
    let publish = |checks: &Vec<Check>| {
        with_model(&ctx.app, |m| m.checks = checks.clone());
        changed(&ctx.app);
    };

    let project = ctx.project()?;
    let site = ctx.site()?;
    push(&mut checks, "Projekt", Status::Ok, format!("{} ({})", site.name, site.url));

    for (tool, args) in [("git", "--version"), ("node", "--version"), ("npm", "--version"), ("claude", "--version")] {
        match shell::output(tool, &[args], &project) {
            Ok(version) => push(&mut checks, tool, Status::Ok, version.lines().next().unwrap_or_default().to_owned()),
            Err(_) => push(&mut checks, tool, Status::Error, "nicht gefunden".into()),
        }
    }
    match github::token() {
        Some(_) => push(&mut checks, "GitHub", Status::Ok, "angemeldet über gh (5000 Anfragen/Stunde)".into()),
        None => push(&mut checks, "GitHub", Status::Warning, "ohne gh-Anmeldung nur 60 Anfragen/Stunde – `gh auth login`".into()),
    }
    publish(&checks);
    ctx.runner.check()?;

    let branch = git::branch(&project).unwrap_or_else(|_| "?".into());
    let changes = git::changes(&project)?;
    if changes.is_empty() {
        push(&mut checks, "Arbeitsverzeichnis", Status::Ok, format!("sauber (Branch {branch})"));
    } else {
        let names: Vec<String> = changes.iter().map(|l| l.get(3..).unwrap_or(l).to_owned()).collect();
        push(
            &mut checks,
            "Arbeitsverzeichnis",
            Status::Warning,
            format!("{} offene Änderungen{}; vor Update und Upload committen oder mit git stash -u beiseitelegen", changes.len(), preview(&names)),
        );
    }
    if git::has_upstream(&project) {
        match git::fetch(&project).and_then(|_| git::ahead_behind(&project)) {
            Ok((0, 0)) => push(&mut checks, "Remote", Status::Ok, "auf dem Stand von origin".into()),
            Ok((ahead, 0)) => push(&mut checks, "Remote", Status::Ok, format!("{ahead} Commit(s) noch nicht gepusht – Upload pusht sie")),
            Ok((_, behind)) => push(&mut checks, "Remote", Status::Warning, format!("{behind} Commit(s) hinter origin – Update holt sie per pull")),
            Err(e) => push(&mut checks, "Remote", Status::Warning, format!("{e:#}")),
        }
    } else {
        push(&mut checks, "Remote", Status::Warning, "kein Upstream-Branch, Push nicht möglich".into());
    }
    publish(&checks);
    ctx.runner.check()?;

    let release_check = find_outdated(ctx, &site, &|_| true, |check| checks.push(check));
    with_model(&ctx.app, |m| m.outdated = Some(release_check.clone()));
    publish(&checks);
    ctx.runner.check()?;

    let config = ctx.config();
    if !config.sftp.is_complete() {
        push(&mut checks, "SFTP", Status::Warning, "nicht vollständig eingerichtet".into());
    } else {
        match connect(ctx) {
            Ok(connection) => match connection.list_root() {
                Ok(names) => push(&mut checks, "SFTP", Status::Ok, format!("{}@{} · {} · {} Einträge{}", config.sftp.user, config.sftp.host, config.sftp.remote_dir, names.len(), preview(&names))),
                Err(e) => push(&mut checks, "SFTP", Status::Error, format!("{e:#}")),
            },
            Err(e) => push(&mut checks, "SFTP", Status::Error, format!("{e:#}")),
        }
    }
    publish(&checks);
    ctx.runner.check()?;

    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    match agent.get(&site.url).call() {
        Ok(response) => push(&mut checks, "Live-Seite", Status::Ok, format!("{} antwortet mit HTTP {}", site.url, response.status())),
        Err(ureq::Error::Status(code, _)) => push(&mut checks, "Live-Seite", Status::Warning, format!("{} antwortet mit HTTP {code}", site.url)),
        Err(e) => push(&mut checks, "Live-Seite", Status::Warning, format!("{} nicht erreichbar: {e}", site.url)),
    }
    match manifest::load_last(&config.sftp) {
        Some(last) => {
            let since = last.commit.as_deref().and_then(|c| git::commits_since(&project, c));
            let detail = match since {
                Some(0) => format!("{} Dateien, Stand des aktuellen Commits", last.files.len()),
                Some(n) => format!("{} Dateien, seither {n} neue(r) Commit(s)", last.files.len()),
                None => format!("{} Dateien", last.files.len()),
            };
            push(&mut checks, "Letzter Upload", Status::Ok, detail);
        }
        None => push(&mut checks, "Letzter Upload", Status::Idle, "noch keiner mit dieser App (der erste Upload lädt alles hoch)".into()),
    }
    publish(&checks);

    let worst = checks.iter().map(|c| c.status).max().unwrap_or(Status::Ok).max(Status::Ok);
    let problems = checks.iter().filter(|c| c.status >= Status::Warning && !c.label.starts_with("Release ")).count();
    let outdated = &release_check.outdated;
    let mut summary = if !outdated.is_empty() {
        format!(
            "Neue Releases: {}",
            outdated.iter().map(|o| format!("{} {} → {}", o.name, o.documented, o.latest_version())).collect::<Vec<_>>().join(", ")
        )
    } else if !release_check.complete() {
        format!("Release-Stand unklar für {} Produkt(e)", release_check.unknown.len())
    } else if site.products.iter().all(|p| config.product_enabled(&p.id)) {
        "Alle Produkte auf dem dokumentierten Stand".to_owned()
    } else {
        "Alle ausgewählten Produkte auf dem dokumentierten Stand".to_owned()
    };
    if !outdated.is_empty() && !release_check.complete() {
        summary.push_str(&format!(" · Release-Stand unklar für {} Produkt(e)", release_check.unknown.len()));
    }
    if problems > 0 {
        summary.push_str(&format!(" · {problems} Hinweis(e)"));
    }
    Ok((worst, summary))
}

// MARK: Update

/// Update and Upload need a clean working tree. Lists what is dirty in the log with the commands to
/// look at it, keep it or put it aside; the error names the files and the recommendation.
pub(crate) fn dirty_tree(ctx: &Ctx, project: &std::path::Path, changes: &[String], step: &str, why: &str) -> anyhow::Error {
    // `git status --porcelain`: two status letters, a space, the path.
    let entries: Vec<(&str, &str)> = changes.iter().map(|line| (line.get(..2).unwrap_or(""), line.get(3..).unwrap_or(line))).collect();
    let untracked_only = entries.iter().all(|(code, _)| *code == "??");
    ctx.warn(format!("{} nicht committete Änderung(en) in {}:", entries.len(), openreleasepilot_core::paths::abbreviate_home(project)));
    for (code, path) in entries.iter().take(30) {
        ctx.log("out", format!("  {}: {path}", change_kind(code)));
    }
    if entries.len() > 30 {
        ctx.log("out", format!("  … und {} weitere", entries.len() - 30));
    }
    ctx.info("Empfehlung:");
    ctx.info(if untracked_only {
        "  Das sind neue Dateien, die git noch nicht kennt. Gehören sie zur Website, committen; sonst löschen oder in .gitignore aufnehmen."
    } else {
        "  Häufigste Ursache: ein Update, das nach Claudes Änderungen beim Build oder an site.json abgebrochen ist. Erst ansehen, dann übernehmen oder beiseitelegen."
    });
    if entries.iter().any(|(_, path)| path.ends_with("site.json")) {
        ctx.info("  site.json ist geändert: vor dem Commit prüfen, ob build, output und claude.allowedTools noch stimmen.");
    }
    let dir = project.display();
    ctx.cmd(format!("  ansehen:        git -C \"{dir}\" diff"));
    ctx.cmd(format!("  übernehmen:     git -C \"{dir}\" add -A && git -C \"{dir}\" commit -m \"…\""));
    ctx.cmd(format!("  beiseitelegen:  git -C \"{dir}\" stash -u   (zurückholen mit git -C \"{dir}\" stash pop)"));
    ctx.info(format!("  Danach {step} erneut starten."));
    let names: Vec<String> = entries.iter().map(|(_, path)| path.to_string()).collect();
    let advice = if untracked_only { "committen, wenn sie dazugehören, sonst löschen" } else { "ansehen, dann committen oder mit git stash -u beiseitelegen" };
    anyhow!("{} nicht committete Änderung(en) im Projekt{}. {why} Empfehlung: {advice}; die Befehle stehen im Log.", entries.len(), preview(&names))
}

fn change_kind(code: &str) -> &'static str {
    match code {
        "??" => "neu, nicht versioniert",
        c if c.contains('U') => "Konflikt",
        c if c.contains('D') => "gelöscht",
        c if c.contains('R') => "umbenannt",
        c if c.contains('A') => "neu",
        _ => "geändert",
    }
}

/// Content changes start from a clean tree on the latest remote state.
fn prepare_tree(ctx: &Ctx, project: &std::path::Path) -> Result<()> {
    let changes = git::changes(project)?;
    if !changes.is_empty() {
        return Err(dirty_tree(ctx, project, &changes, "Update", "Update startet nur auf einem sauberen Stand."));
    }
    if git::has_upstream(project) {
        ctx.cmd("$ git pull --ff-only");
        let out = git::pull_ff(project)?;
        ctx.log("out", if out.is_empty() { "Already up to date.".to_owned() } else { out });
    }
    Ok(())
}

/// Brings Claude Code up to date before it runs, at most every six hours: aliases like `opus` or
/// `fable` point to the newest model only in a current CLI, and one that only ever runs headless
/// (as here) does not update itself. A failed update only warns.
pub(crate) fn update_claude(ctx: &Ctx, dir: &std::path::Path) -> Result<()> {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|at| at.elapsed() < Duration::from_secs(6 * 3600)) {
        return Ok(());
    }
    ctx.cmd("$ claude update");
    let mut output = String::new();
    let result = shell::stream("claude", &["update".to_owned()], dir, &HashMap::new(), &ctx.runner, |line| {
        output.push_str(line);
        output.push('\n');
    });
    match result {
        Err(e) if ctx.runner.is_cancelled() => return Err(e),
        Err(e) => ctx.warn(format!("  {e:#}; es läuft die installierte Version")),
        Ok(0) => ctx.info(format!("  {}", claude::update_note(&output))),
        Ok(_) => ctx.warn(format!("  Nicht aktualisiert: {}; es läuft die installierte Version", claude::update_note(&output))),
    }
    *last = Some(Instant::now());
    Ok(())
}

/// Runs Claude Code with the prompt and fails unless it finished successfully.
fn run_claude(ctx: &Ctx, site: &Site, project: &std::path::Path, prompt: &str, label: &str, model: Option<&str>, effort: Option<&str>) -> Result<claude::Outcome> {
    update_claude(ctx, project)?;
    ctx.cmd(format!(
        "$ claude -p … --model {} --effort {} (Prompt unten)",
        model.unwrap_or("Standard"),
        effort.unwrap_or("Standard")
    ));
    for line in prompt.lines() {
        ctx.log("claude", format!("  {line}"));
    }
    let args = claude::args(site, prompt, model, effort);
    let mut outcome = None;
    // No GITHUB_TOKEN here: whatever Claude runs (the build included) must not get the user's token.
    let code = shell::stream("claude", &args, project, &HashMap::new(), &ctx.runner, |line| match claude::parse(line) {
        Event::Line(text) => ctx.log("claude", text),
        Event::Done(done) => outcome = Some(done),
        Event::Ignore => {}
    })?;
    if !openreleasepilot_core::site::load(project).is_ok_and(|after| site.same_permissions(&after)) {
        bail!(
            "Claude hat build, output oder claude.allowedTools/disallowedTools in {} geändert oder die Datei unbrauchbar gemacht. \
             Bitte die Änderung prüfen; sie wird weder gebaut noch hochgeladen.",
            openreleasepilot_core::site::file_name(project)
        );
    }
    let outcome = outcome.ok_or_else(|| anyhow!("Claude Code hat ohne Ergebnis beendet (Exit-Code {code})"))?;
    with_model(&ctx.app, |m| {
        m.claude_runs.push(ClaudeRun { task: label.to_owned(), model: model.unwrap_or("Standard").to_owned(), outcome: outcome.clone() })
    });
    changed(&ctx.app);
    for denial in &outcome.denials {
        ctx.warn(format!("Nicht freigegeben: {denial}"));
    }
    if let Some(cost) = outcome.cost_usd {
        ctx.info(format!("Claude: {} Runden, {:.2} USD", outcome.turns.unwrap_or(0), cost));
    }
    if !outcome.success {
        bail!("Claude hat nicht erfolgreich abgeschlossen: {}", outcome.text.lines().next().unwrap_or("ohne Meldung"));
    }
    Ok(outcome)
}

/// Builds and commits what Claude changed; `None` if nothing changed.
fn build_and_commit(ctx: &Ctx, site: &Site, project: &std::path::Path, subject: &str, summary: &str) -> Result<Option<(String, usize)>> {
    let changes = git::changes(project)?;
    if changes.is_empty() {
        return Ok(None);
    }
    ctx.info(format!("{} geänderte oder neue Dateien:", changes.len()));
    for line in &changes {
        ctx.log("out", format!("  {line}"));
    }
    build(ctx, site, project).context("Die Änderungen bleiben uncommittet im Arbeitsverzeichnis")?;
    let body = format!("{summary}\n\nCo-Authored-By: Claude <noreply@anthropic.com>");
    ctx.cmd(format!("$ git commit -m \"{subject}\""));
    let sha = git::commit_all(project, subject, &body)?;
    ctx.ok(format!("Commit {sha}"));
    Ok(Some((sha, changes.len())))
}

fn update(ctx: &Ctx, instructions: &str) -> Result<(Status, String)> {
    let result = update_site(ctx, instructions, None)?;
    match result.commit {
        Some((sha, files)) => Ok((Status::Ok, format!("Commit {sha}: {files} Dateien geändert"))),
        None if result.nothing => Ok((Status::Ok, result.summary)),
        None => Ok((Status::Warning, result.summary)),
    }
}

/// What a website update did.
pub struct SiteUpdate {
    /// The local commit and the number of files, if anything changed.
    pub commit: Option<(String, usize)>,
    /// Nothing outdated and no instruction: Claude did not run.
    pub nothing: bool,
    pub summary: String,
    /// e.g. `App 1.2.0 → 1.3.0`.
    pub products: Vec<String>,
}

/// The website's Update: Claude runs the selected tasks, then build and a local commit. `only`
/// limits it to products from these repositories (a care job); the website's product selection
/// applies either way.
pub fn update_site(ctx: &Ctx, instructions: &str, only: Option<&[String]>) -> Result<SiteUpdate> {
    let project = ctx.project()?;
    let site = ctx.site()?;
    prepare_tree(ctx, &project)?;

    ctx.info("Neueste Releases abfragen …");
    let in_scope = |p: &openreleasepilot_core::site::Product| only.is_none_or(|keys| keys.contains(&p.repo.trim().to_lowercase()));
    let release_check = find_outdated(ctx, &site, &in_scope, |_| {});
    if only.is_none() {
        with_model(&ctx.app, |m| m.outdated = Some(release_check.clone()));
    }
    require_complete(&release_check)?;
    let outdated = release_check.outdated;
    let config = ctx.config();
    let skipped: Vec<String> = site.products.iter().filter(|p| !config.product_enabled(&p.id) || !in_scope(p)).map(|p| p.name.clone()).collect();
    let products: Vec<String> = outdated.iter().map(|o| format!("{} {} → {}", o.name, o.documented, o.latest_version())).collect();
    if outdated.is_empty() && instructions.trim().is_empty() {
        let what = if skipped.is_empty() { "alle Produkte" } else { "alle ausgewählten Produkte" };
        return Ok(SiteUpdate { commit: None, nothing: true, summary: format!("Nichts zu tun: {what} aktuell"), products });
    }
    let tasks: Vec<ResolvedTask> = resolve_tasks(&site, &config).into_iter().filter(|t| t.enabled).collect();
    if tasks.is_empty() {
        bail!("Keine Website-Aufgabe ausgewählt");
    }

    let mut summaries = Vec::new();
    for (i, task) in tasks.iter().enumerate() {
        ctx.runner.check()?;
        ctx.log("cmd", format!("━━ Aufgabe {}/{}: {} ━━", i + 1, tasks.len(), task.task.label));
        let prompt = claude::task_prompt(&site, &task.task, &outdated, &skipped, instructions);
        let outcome = run_claude(ctx, &site, &project, &prompt, &task.task.label, task.model.as_deref(), task.effort.as_deref())?;
        summaries.push(if tasks.len() > 1 { format!("{}:\n{}", task.task.label, outcome.text) } else { outcome.text });
    }
    let subject = if outdated.is_empty() {
        let first = instructions.lines().next().unwrap_or("").trim();
        format!("Website-Update: {}", first.chars().take(60).collect::<String>())
    } else {
        format!(
            "Website-Update: {}",
            outdated.iter().map(|o| format!("{} {}", o.name, o.latest_version())).collect::<Vec<_>>().join(", ")
        )
    };
    let summary = summaries.join("\n\n");
    match build_and_commit(ctx, &site, &project, &subject, &summary)? {
        Some(commit) => {
            with_model(&ctx.app, |m| m.outdated = None);
            Ok(SiteUpdate { commit: Some(commit), nothing: false, summary, products })
        }
        None => Ok(SiteUpdate { commit: None, nothing: false, summary: "Claude hat keine Dateien geändert".into(), products }),
    }
}

// MARK: Add product

fn add_product(ctx: &Ctx, input: &NewProductInput) -> Result<(Status, String)> {
    let project = ctx.project()?;
    let site = ctx.site()?;
    let repo = input.repo.trim();
    let name = input.name.trim();
    if name.is_empty() {
        bail!("Name des Produkts fehlt");
    }
    if site.products.iter().any(|p| p.repo.eq_ignore_ascii_case(repo)) {
        bail!("{repo} ist bereits als Produkt in site.json eingetragen");
    }
    prepare_tree(ctx, &project)?;

    ctx.info(format!("Repository {repo} prüfen …"));
    let info = github::repo_info(repo)?;
    if info.private {
        ctx.warn("Das Repository ist privat: Besucher können Links darauf nicht öffnen.");
    }
    let latest_tag = github::latest_release(&info.full_name)?;
    match &latest_tag {
        Some(tag) => ctx.ok(format!("Neuestes Release: {tag}")),
        None => ctx.warn("Noch kein veröffentlichtes Release: kein Versions-Badge, der Precheck beobachtet das Produkt erst nach dem ersten Release."),
    }
    let local_path = match input.local_path.trim() {
        "" => None,
        path => {
            let clone = expand_home(path);
            if localrepo::origin_repo(&clone).is_none_or(|r| !r.eq_ignore_ascii_case(&info.full_name)) {
                bail!("{} ist kein Klon von {}", clone.display(), info.full_name);
            }
            let relative = localrepo::relative_to_project(&project, &clone);
            ctx.ok(format!("Lokaler Klon: {relative}"));
            Some(relative)
        }
    };
    ctx.runner.check()?;

    let product = claude::NewProduct {
        repo: info.full_name.clone(),
        name: name.to_owned(),
        local_path,
        latest_tag: latest_tag.clone(),
        description: info.description.clone(),
        license: info.license.clone(),
        private: info.private,
    };
    let model = Some(input.model.trim().to_owned()).filter(|m| !m.is_empty()).or_else(|| site.claude.model.clone());
    let effort = site.claude.effort.clone().unwrap_or_else(|| "high".into());
    let prompt = claude::add_product_prompt(&site, openreleasepilot_core::site::file_name(&project), &product, &input.instructions);
    let outcome = run_claude(ctx, &site, &project, &prompt, "Neues Produkt", model.as_deref(), Some(&effort))?;

    if latest_tag.is_some() {
        let listed = ctx.site().map(|s| s.products.iter().any(|p| p.repo.eq_ignore_ascii_case(&info.full_name))).unwrap_or(false);
        if !listed {
            ctx.warn("site.json enthält das Produkt nicht: Precheck und Update beobachten seine Releases nicht.");
        }
    }
    match build_and_commit(ctx, &site, &project, &format!("Neues Produkt: {name}"), &outcome.text)? {
        Some((sha, files)) => Ok((Status::Ok, format!("Commit {sha}: {name} aufgenommen, {files} Dateien"))),
        None => Ok((Status::Warning, "Claude hat keine Dateien geändert".into())),
    }
}

// MARK: Upload

pub(crate) fn upload(ctx: &Ctx) -> Result<(Status, String)> {
    let project = ctx.project()?;
    let site = ctx.site()?;
    let config = ctx.config();
    if !config.sftp.is_complete() {
        bail!("SFTP ist nicht vollständig eingerichtet");
    }
    let changes = git::changes(&project)?;
    if !changes.is_empty() {
        return Err(dirty_tree(ctx, &project, &changes, "Upload", "Hochgeladen wird nur ein committeter Stand."));
    }
    let upstream = git::has_upstream(&project);
    let mut ahead = 0;
    if upstream {
        git::fetch(&project)?;
        let (a, behind) = git::ahead_behind(&project)?;
        if behind > 0 {
            bail!("{behind} Commit(s) hinter origin. Erst Update ausführen (holt sie per pull).");
        }
        ahead = a;
    }

    build(ctx, &site, &project)?;
    ctx.runner.check()?;
    let output = site.output_dir(&project);
    let mut current = manifest::scan(&output)?;
    if current.files.is_empty() {
        bail!("Ausgabeordner {} ist leer", site.output);
    }
    let previous = manifest::load_last(&config.sftp).unwrap_or_default();
    let first = previous.files.is_empty();
    let diff = manifest::diff(&previous, &current);
    ctx.info(format!("{} neu oder geändert, {} zu entfernen, {} unverändert", diff.upload.len(), diff.delete.len(), diff.unchanged));
    if first {
        ctx.warn("Erster Upload mit dieser App: alle Dateien werden hochgeladen, vorhandene Dateien auf dem Server bleiben unberührt.");
    }

    let head = git::head(&project).unwrap_or_default().chars().take(7).collect::<String>();
    let pushed = if ahead > 0 {
        ctx.cmd("$ git push origin HEAD");
        git::push(&project)?;
        ctx.ok(format!("{ahead} Commit(s) gepusht, origin steht jetzt auf {head}"));
        format!("{ahead} Commit(s) gepusht")
    } else if upstream {
        ctx.ok(format!("Nichts zu pushen: origin steht bereits auf {head}"));
        "bereits gepusht".to_owned()
    } else {
        ctx.warn("Kein Upstream-Branch: nicht gepusht");
        "nicht gepusht (kein Upstream-Branch)".to_owned()
    };

    if diff.upload.is_empty() && diff.delete.is_empty() {
        current.commit = git::head(&project).ok();
        current.uploaded_at = Some(now());
        manifest::save_last(&config.sftp, &current)?;
        with_model(&ctx.app, |m| m.upload = Some(UploadResult { uploaded: 0, deleted: 0, unchanged: diff.unchanged, pushed: pushed.clone() }));
        return Ok((Status::Ok, format!("Server war bereits auf dem Stand des Builds · Git: {pushed}")));
    }

    ctx.info(format!("Verbinde mit {}@{} …", config.sftp.user, config.sftp.host));
    let connection = connect(ctx)?;
    for (i, path) in diff.upload.iter().enumerate() {
        ctx.runner.check()?;
        connection.upload(&output.join(path), path)?;
        ctx.log("out", format!("↑ {path} ({}/{})", i + 1, diff.upload.len()));
    }
    for path in &diff.delete {
        ctx.runner.check()?;
        connection.delete(path)?;
        ctx.log("out", format!("✕ {path}"));
    }
    current.commit = git::head(&project).ok();
    current.uploaded_at = Some(now());
    manifest::save_last(&config.sftp, &current)?;
    let result = UploadResult { uploaded: diff.upload.len(), deleted: diff.delete.len(), unchanged: diff.unchanged, pushed };
    with_model(&ctx.app, |m| m.upload = Some(result.clone()));
    Ok((Status::Ok, format!("{} Dateien hochgeladen, {} entfernt · Git: {}", result.uploaded, result.deleted, result.pushed)))
}

// MARK: Verify

pub(crate) fn verify(ctx: &Ctx) -> Result<(Status, String)> {
    let project = ctx.project()?;
    let site = ctx.site()?;
    let config = ctx.config();
    let manifest = match manifest::load_last(&config.sftp) {
        Some(last) => {
            ctx.info(format!("Vergleiche {} mit dem letzten Upload ({} Dateien)", site.url, last.files.len()));
            last
        }
        None => {
            let output = site.output_dir(&project);
            if !output.is_dir() {
                bail!("Noch kein Upload und kein Build vorhanden");
            }
            ctx.warn("Noch kein Upload mit dieser App: vergleiche mit dem lokalen Build");
            manifest::scan(&output)?
        }
    };
    let report = verify::compare(&site.url, &manifest, |done, total| {
        if done % 50 == 0 || done == total {
            ctx.info(format!("{done}/{total} geprüft"));
        }
    });
    for problem in report.problems.iter().take(100) {
        ctx.warn(problem.clone());
    }
    with_model(&ctx.app, |m| m.verify = Some(report.clone()));
    if report.problems.is_empty() {
        Ok((Status::Ok, format!("Alle {} Dateien live identisch", report.checked)))
    } else {
        Ok((Status::Error, format!("{} von {} Dateien weichen ab", report.problems.len(), report.checked)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_counts_as_busy_before_its_thread_runs() {
        let runner = Arc::new(Runner::default());
        assert!(busy(Some(&runner), None), "a second click right after the first");
        assert!(busy(None, Some(Step::Jobs)));
        assert!(!busy(None, None));
    }
}
