//! The four steps. Each runs on its own thread, writes log lines as it goes and ends with a
//! status and a one-line summary.

use crate::{changed, load_site, with_model, Check, LogLine, Status, Step, UploadResult};
use anyhow::{anyhow, bail, Context, Result};
use openftpupdater_core::claude::{self, Event, Outdated};
use openftpupdater_core::config::{AuthKind, Config};
use openftpupdater_core::paths::expand_home;
use openftpupdater_core::sftp::{self, ConnectError, Connection};
use openftpupdater_core::shell::{self, Runner};
use openftpupdater_core::site::Site;
use openftpupdater_core::{git, github, manifest, secrets, verify};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

const MAX_LOG_LINES: usize = 5000;

fn now() -> u64 {
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
    app: AppHandle,
    step: Step,
    runner: Arc<Runner>,
}

impl Ctx {
    fn log(&self, kind: &'static str, text: impl Into<String>) {
        let text = strip_ansi(&text.into());
        let line = LogLine { kind, text };
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
    fn info(&self, text: impl Into<String>) {
        self.log("info", text)
    }
    fn ok(&self, text: impl Into<String>) {
        self.log("ok", text)
    }
    fn warn(&self, text: impl Into<String>) {
        self.log("warn", text)
    }
    fn cmd(&self, text: impl Into<String>) {
        self.log("cmd", text)
    }

    fn config(&self) -> Config {
        with_model(&self.app, |m| m.config.clone())
    }

    fn project(&self) -> Result<PathBuf> {
        let dir = self.config().project_dir;
        if dir.trim().is_empty() {
            bail!("Kein Projektordner eingerichtet");
        }
        let path = expand_home(&dir);
        if !path.is_dir() {
            bail!("Projektordner {} nicht gefunden", path.display());
        }
        Ok(path)
    }

    /// Reloads site.json, so edits (e.g. a raised `documented`) are always current.
    fn site(&self) -> Result<Site> {
        let config = self.config();
        let site = load_site(&config);
        with_model(&self.app, |m| m.site = site.clone());
        site.map_err(|e| anyhow!(e))
    }
}

/// Starts the given steps one after another on a background thread.
pub fn start(app: &AppHandle, steps: Vec<Step>, instructions: String) -> Result<(), String> {
    let runner = Arc::new(Runner::default());
    let busy = with_model(app, |m| {
        if m.running.is_some() {
            return true;
        }
        m.runner = Some(runner.clone());
        m.error = None;
        m.notice = None;
        false
    });
    if busy {
        return Err("Es läuft bereits ein Schritt.".into());
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let all = steps.len() > 1;
        for step in steps {
            if all && step == Step::Update {
                let nothing_to_do = with_model(&app, |m| m.outdated.as_ref().is_some_and(|o| o.is_empty())) && instructions.trim().is_empty();
                if nothing_to_do {
                    continue;
                }
            }
            let ctx = Ctx { app: app.clone(), step, runner: runner.clone() };
            with_model(&app, |m| {
                m.running = Some(step);
                let state = m.steps.entry(step).or_default();
                state.status = Status::Running;
                state.summary.clear();
                state.log.clear();
            });
            changed(&app);
            let result = match step {
                Step::Precheck => precheck(&ctx),
                Step::Update => update(&ctx, &instructions),
                Step::Upload => upload(&ctx),
                Step::Verify => verify(&ctx),
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
            });
            changed(&app);
            if status == Status::Error || runner.is_cancelled() {
                break;
            }
        }
        with_model(&app, |m| {
            m.running = None;
            m.runner = None;
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

fn find_outdated(ctx: &Ctx, site: &Site, mut on_check: impl FnMut(Check)) -> Vec<Outdated> {
    let mut outdated = Vec::new();
    for product in &site.products {
        let label = format!("Release {}", product.name);
        match github::latest_tag(&product.repo) {
            Ok(tag) if github::is_newer(&tag, &product.documented) => {
                let o = Outdated::new(product, tag);
                ctx.warn(format!("{}: dokumentiert {}, neuestes Release {}", product.name, o.documented, o.latest_version()));
                on_check(Check { label, status: Status::Warning, detail: format!("{} → {}", o.documented, o.latest_version()) });
                outdated.push(o);
            }
            Ok(tag) => {
                ctx.ok(format!("{}: {} ist aktuell", product.name, product.documented));
                on_check(Check { label, status: Status::Ok, detail: format!("{} aktuell", tag.trim_start_matches('v')) });
            }
            Err(e) => {
                ctx.warn(format!("{}: {e:#}", product.name));
                on_check(Check { label, status: Status::Warning, detail: format!("{e:#}") });
            }
        }
    }
    outdated
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
        push(&mut checks, "Arbeitsverzeichnis", Status::Warning, format!("{} offene Änderungen{}", changes.len(), preview(&names)));
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

    let outdated = find_outdated(ctx, &site, |check| checks.push(check));
    with_model(&ctx.app, |m| m.outdated = Some(outdated.clone()));
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
    let mut summary = if outdated.is_empty() {
        "Alle Produkte auf dem dokumentierten Stand".to_owned()
    } else {
        format!(
            "Neue Releases: {}",
            outdated.iter().map(|o| format!("{} {} → {}", o.name, o.documented, o.latest_version())).collect::<Vec<_>>().join(", ")
        )
    };
    if problems > 0 {
        summary.push_str(&format!(" · {problems} Hinweis(e)"));
    }
    Ok((worst, summary))
}

// MARK: Update

fn update(ctx: &Ctx, instructions: &str) -> Result<(Status, String)> {
    let project = ctx.project()?;
    let site = ctx.site()?;
    let changes = git::changes(&project)?;
    if !changes.is_empty() {
        bail!("Das Arbeitsverzeichnis enthält nicht committete Änderungen ({}). Bitte erst committen oder verwerfen.", changes.len());
    }
    if git::has_upstream(&project) {
        ctx.cmd("$ git pull --ff-only");
        let out = git::pull_ff(&project)?;
        ctx.log("out", if out.is_empty() { "Already up to date.".to_owned() } else { out });
    }

    ctx.info("Neueste Releases abfragen …");
    let outdated = find_outdated(ctx, &site, |_| {});
    with_model(&ctx.app, |m| m.outdated = Some(outdated.clone()));
    if outdated.is_empty() && instructions.trim().is_empty() {
        return Ok((Status::Ok, "Nichts zu tun: alle Produkte aktuell".into()));
    }
    ctx.runner.check()?;

    let prompt = claude::prompt(&site, &outdated, instructions);
    ctx.cmd("$ claude -p … (Prompt unten)");
    for line in prompt.lines() {
        ctx.log("claude", format!("  {line}"));
    }
    let args = claude::args(&site, &prompt);
    let mut outcome = None;
    let code = shell::stream("claude", &args, &project, &github_env(), &ctx.runner, |line| match claude::parse(line) {
        Event::Line(text) => ctx.log("claude", text),
        Event::Done(done) => outcome = Some(done),
        Event::Ignore => {}
    })?;
    let outcome = outcome.ok_or_else(|| anyhow!("Claude Code hat ohne Ergebnis beendet (Exit-Code {code})"))?;
    with_model(&ctx.app, |m| m.claude = Some(outcome.clone()));
    changed(&ctx.app);
    for denial in &outcome.denials {
        ctx.warn(format!("Nicht freigegeben: {denial}"));
    }
    if let Some(cost) = outcome.cost_usd {
        ctx.info(format!("Claude: {} Runden, {:.2} USD", outcome.turns.unwrap_or(0), cost));
    }
    if !outcome.success {
        bail!("Claude hat das Update nicht abgeschlossen: {}", outcome.text.lines().next().unwrap_or("ohne Meldung"));
    }

    let changes = git::changes(&project)?;
    if changes.is_empty() {
        return Ok((Status::Warning, "Claude hat keine Dateien geändert".into()));
    }
    ctx.info(format!("{} geänderte Dateien", changes.len()));
    if let Ok(stat) = git::diff_stat(&project) {
        for line in stat.lines() {
            ctx.log("out", line);
        }
    }
    build(ctx, &site, &project).context("Die Änderungen bleiben uncommittet im Arbeitsverzeichnis")?;

    let subject = if outdated.is_empty() {
        let first = instructions.lines().next().unwrap_or("").trim();
        format!("Website-Update: {}", first.chars().take(60).collect::<String>())
    } else {
        format!(
            "Website-Update: {}",
            outdated.iter().map(|o| format!("{} {}", o.name, o.latest_version())).collect::<Vec<_>>().join(", ")
        )
    };
    let body = format!("{}\n\nCo-Authored-By: Claude <noreply@anthropic.com>", outcome.text);
    ctx.cmd(format!("$ git commit -m \"{subject}\""));
    let sha = git::commit_all(&project, &subject, &body)?;
    ctx.ok(format!("Commit {sha}"));
    with_model(&ctx.app, |m| m.outdated = None);
    Ok((Status::Ok, format!("Commit {sha}: {} Dateien geändert", changes.len())))
}

// MARK: Upload

fn upload(ctx: &Ctx) -> Result<(Status, String)> {
    let project = ctx.project()?;
    let site = ctx.site()?;
    let config = ctx.config();
    if !config.sftp.is_complete() {
        bail!("SFTP ist nicht vollständig eingerichtet");
    }
    let changes = git::changes(&project)?;
    if !changes.is_empty() {
        bail!("Das Arbeitsverzeichnis enthält nicht committete Änderungen ({}). Hochgeladen wird nur ein committeter Stand.", changes.len());
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

    if ahead > 0 {
        ctx.cmd("$ git push origin HEAD");
        git::push(&project)?;
        ctx.ok(format!("{ahead} Commit(s) gepusht"));
    } else if !upstream {
        ctx.warn("Kein Upstream-Branch: nicht gepusht");
    }

    if diff.upload.is_empty() && diff.delete.is_empty() {
        current.commit = git::head(&project).ok();
        current.uploaded_at = Some(now());
        manifest::save_last(&config.sftp, &current)?;
        with_model(&ctx.app, |m| m.upload = Some(UploadResult { uploaded: 0, deleted: 0, unchanged: diff.unchanged }));
        return Ok((Status::Ok, "Server ist bereits auf dem Stand des Builds".into()));
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
    let result = UploadResult { uploaded: diff.upload.len(), deleted: diff.delete.len(), unchanged: diff.unchanged };
    with_model(&ctx.app, |m| m.upload = Some(result.clone()));
    Ok((Status::Ok, format!("{} Dateien hochgeladen, {} entfernt", result.uploaded, result.deleted)))
}

// MARK: Verify

fn verify(ctx: &Ctx) -> Result<(Status, String)> {
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
