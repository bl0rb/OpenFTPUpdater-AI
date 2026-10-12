// No console window next to the app on Windows (release builds).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod steps;

use steps::{Job, NewProductInput};

use openreleasepilot_core::claude::{Outcome, Outdated, ReleaseCheck};
use openreleasepilot_core::config::{self, AuthKind, Config};
use openreleasepilot_core::update::{self, ReleaseInfo};
use openreleasepilot_core::{github, localrepo, manifest, repocare, repos};
use openreleasepilot_core::paths::{self, abbreviate_home, expand_home};
use openreleasepilot_core::secrets;
use openreleasepilot_core::sftp::{self, ConnectError};
use openreleasepilot_core::shell::Runner;
use openreleasepilot_core::site::{self, Site};
use openreleasepilot_core::verify::Report;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Step {
    Precheck,
    Update,
    Upload,
    Verify,
    /// Care of the products' repositories (area "Repositories"); not part of the four-step flow.
    Repos,
}

impl Step {
    pub const ALL: [Step; 5] = [Step::Precheck, Step::Update, Step::Upload, Step::Verify, Step::Repos];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    #[default]
    Idle,
    Running,
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    /// `info`, `ok`, `warn`, `error`, `cmd`, `out` or `claude`.
    pub kind: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StepState {
    pub status: Status,
    pub summary: String,
    /// Unix seconds.
    pub finished_at: Option<u64>,
    /// The result predates a later Update or Upload (only used for Precheck).
    pub stale: bool,
    #[serde(skip)]
    pub log: Vec<LogLine>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub label: String,
    pub status: Status,
    pub detail: String,
}

/// What became of one planned care action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemOutcome {
    Done,
    Skipped,
    Failed,
    /// Not tried: the run was cancelled before.
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoItem {
    pub label: String,
    pub outcome: ItemOutcome,
    pub detail: String,
}

/// What the care run did in one repository, one row per planned action.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoReport {
    pub name: String,
    pub repo: String,
    pub items: Vec<RepoItem>,
}

/// What the Repositories area knows about one repository; read only, kept in memory.
#[derive(Default)]
pub struct RepoStatus {
    pub facts: Option<repos::Facts>,
    pub loading: bool,
    /// The last refresh failed entirely; `facts`, if any, are from before.
    pub error: Option<String>,
    /// Answers to an older request are dropped.
    pub token: u64,
    pub details: Option<repos::Details>,
    pub details_loading: bool,
    pub details_token: u64,
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// A number never handed out before: request tokens, preview ids.
pub fn next_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::SeqCst)
}

pub struct Model {
    pub config: Config,
    pub site: Result<Site, String>,
    pub steps: BTreeMap<Step, StepState>,
    pub running: Option<Step>,
    pub runner: Option<Arc<Runner>>,
    pub checks: Vec<Check>,
    /// `None` until a precheck has looked at the releases.
    pub outdated: Option<ReleaseCheck>,
    /// Product id → newest release tag, as last looked up.
    pub latest_tags: BTreeMap<String, String>,
    /// Claude runs of the last Update or New product, one per task.
    pub claude_runs: Vec<ClaudeRun>,
    pub upload: Option<UploadResult>,
    pub verify: Option<Report>,
    /// Repository key → what the Repositories area read about it.
    pub repo_status: BTreeMap<String, RepoStatus>,
    /// Care preview, waiting for the user to run or discard it.
    pub repo_plan: Option<repocare::PlanSet>,
    pub repo_reports: Vec<RepoReport>,
    /// Fingerprint the user has to confirm before the first SFTP connection.
    pub pending_host_key: Option<String>,
    /// Newer version of the app itself, found by the update check.
    pub available_update: Option<ReleaseInfo>,
    pub checking_update: bool,
    pub installing_update: bool,
    pub notice: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeRun {
    pub task: String,
    pub model: String,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UploadResult {
    pub uploaded: usize,
    pub deleted: usize,
    pub unchanged: usize,
    /// What happened with git, e.g. `2 Commit(s) gepusht`.
    pub pushed: String,
}

impl Model {
    fn load() -> Self {
        let config = config::load();
        let site = load_site(&config);
        Self {
            config,
            site,
            steps: Step::ALL.iter().map(|s| (*s, StepState::default())).collect(),
            running: None,
            runner: None,
            checks: Vec::new(),
            outdated: None,
            latest_tags: BTreeMap::new(),
            claude_runs: Vec::new(),
            upload: None,
            verify: None,
            repo_status: BTreeMap::new(),
            repo_plan: None,
            repo_reports: Vec::new(),
            pending_host_key: None,
            available_update: None,
            checking_update: false,
            installing_update: false,
            notice: None,
            error: None,
        }
    }
}

pub fn load_site(config: &Config) -> Result<Site, String> {
    if config.project_dir.trim().is_empty() {
        return Err("Kein Projektordner eingerichtet".into());
    }
    site::load(&expand_home(&config.project_dir)).map_err(|e| format!("{e:#}"))
}

/// The repositories of the site's products, each once; empty without a readable site.json.
pub fn repo_targets(site: &Result<Site, String>, config: &Config) -> Vec<repos::RepoTarget> {
    let Ok(site) = site else { return Vec::new() };
    if config.project_dir.trim().is_empty() {
        return Vec::new();
    }
    let project = expand_home(&config.project_dir);
    let website = localrepo::origin_repo(&project);
    repos::targets(site, &project, &config.skipped_products, website.as_deref())
}

struct Shared(Mutex<Model>);

pub fn with_model<T>(app: &AppHandle, f: impl FnOnce(&mut Model) -> T) -> T {
    let shared = app.state::<Shared>();
    let mut model = shared.0.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut model)
}

/// Tells the window to fetch a new view.
pub fn changed(app: &AppHandle) {
    let _ = app.emit("changed", ());
}

/// Runs blocking work (files, network, dialogs) off the main thread.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tauri::async_runtime::spawn_blocking(f).await.expect("background task panicked")
}

// MARK: View

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LastUpload {
    commit: Option<String>,
    uploaded_at: Option<u64>,
    files: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct View {
    version: String,
    /// Debug build: no update check.
    dev_build: bool,
    config: Config,
    project_display: String,
    /// Folder with the clones, as displayed (auto value if not set).
    workspace_display: String,
    /// `owner/name` of the website repository (origin of the project folder).
    site_repo: Option<String>,
    site: Option<Site>,
    /// Project file in use: `.openreleasepilot/site.json`, or the old path while only that exists.
    site_file: &'static str,
    site_error: Option<String>,
    sftp_ready: bool,
    steps: BTreeMap<Step, StepState>,
    running: Option<Step>,
    checks: Vec<Check>,
    outdated: Option<Vec<Outdated>>,
    /// Products whose newest release the last lookup could not read.
    release_unknown: Vec<String>,
    products: Vec<steps::ProductView>,
    tasks: Vec<steps::ResolvedTask>,
    claude_runs: Vec<ClaudeRun>,
    upload: Option<UploadResult>,
    verify: Option<Report>,
    repositories: Vec<RepoView>,
    repo_plan: Option<repocare::PlanSet>,
    /// Repositories whose default branch moved since the preview, as a later status read shows.
    repo_plan_moved: Vec<String>,
    repo_reports: Vec<RepoReport>,
    /// Facts older than this many seconds are shown as old.
    fresh_secs: u64,
    /// Unix seconds when the view was made.
    now: u64,
    last_upload: Option<LastUpload>,
    pending_host_key: Option<String>,
    available_update: Option<ReleaseInfo>,
    checking_update: bool,
    installing_update: bool,
    notice: Option<String>,
    error: Option<String>,
}

/// One repository in the Repositories area.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RepoView {
    #[serde(flatten)]
    target: repos::RepoTarget,
    selected: bool,
    facts: Option<repos::Facts>,
    health: repos::Health,
    capabilities: repos::Capabilities,
    loading: bool,
    error: Option<String>,
    details: Option<repos::Details>,
    details_loading: bool,
}

#[tauri::command]
fn get_view(app: AppHandle) -> View {
    let version = app.package_info().version.to_string();
    // Reads .git/config of the clones: outside the lock.
    let (site, config) = with_model(&app, |m| (m.site.clone(), m.config.clone()));
    let targets = repo_targets(&site, &config);
    let now = steps::now();
    with_model(&app, |m| View {
        version,
        dev_build: cfg!(debug_assertions),
        config: m.config.clone(),
        project_display: if m.config.project_dir.is_empty() { String::new() } else { abbreviate_home(&expand_home(&m.config.project_dir)) },
        workspace_display: abbreviate_home(&m.config.workspace()),
        site_repo: if m.config.project_dir.is_empty() { None } else { localrepo::origin_repo(&expand_home(&m.config.project_dir)) },
        site: m.site.as_ref().ok().cloned(),
        site_file: if m.config.project_dir.is_empty() { site::SITE_FILE } else { site::file_name(&expand_home(&m.config.project_dir)) },
        site_error: m.site.as_ref().err().cloned(),
        sftp_ready: m.config.sftp.is_complete(),
        steps: m.steps.clone(),
        running: m.running,
        checks: m.checks.clone(),
        outdated: m.outdated.as_ref().map(|c| c.outdated.clone()),
        release_unknown: m.outdated.as_ref().map(|c| c.unknown.clone()).unwrap_or_default(),
        products: m.site.as_ref().map(|site| steps::resolve_products(site, &m.config, &m.latest_tags)).unwrap_or_default(),
        tasks: m.site.as_ref().map(|site| steps::resolve_tasks(site, &m.config)).unwrap_or_default(),
        claude_runs: m.claude_runs.clone(),
        upload: m.upload.clone(),
        verify: m.verify.clone(),
        repositories: targets
            .into_iter()
            .map(|target| {
                let status = m.repo_status.get(&target.key);
                let facts = status.and_then(|s| s.facts.clone());
                let details = status.and_then(|s| s.details.clone());
                RepoView {
                    selected: target.selected(),
                    health: repos::health(&target, facts.as_ref(), details.as_ref(), now),
                    capabilities: repos::capabilities(&target, facts.as_ref(), github::signed_in()),
                    loading: status.is_some_and(|s| s.loading),
                    error: status.and_then(|s| s.error.clone()),
                    details_loading: status.is_some_and(|s| s.details_loading),
                    facts,
                    details,
                    target,
                }
            })
            .collect(),
        repo_plan_moved: m
            .repo_plan
            .iter()
            .flat_map(|set| set.plans.iter().map(move |plan| (set.created_at, plan)))
            .filter(|(created, plan)| {
                let facts = m.repo_status.get(&plan.key).and_then(|s| s.facts.as_ref());
                facts.is_some_and(|f| f.checked_at > *created && f.head.ok().is_some_and(|head| !plan.base_head.is_empty() && *head != plan.base_head))
            })
            .map(|(_, plan)| plan.repo.clone())
            .collect(),
        repo_plan: m.repo_plan.clone(),
        repo_reports: m.repo_reports.clone(),
        fresh_secs: repos::FRESH_SECS,
        now,
        last_upload: manifest::load_last(&m.config.sftp).map(|last| LastUpload {
            commit: last.commit.map(|c| c.chars().take(7).collect()),
            uploaded_at: last.uploaded_at,
            files: last.files.len(),
        }),
        pending_host_key: m.pending_host_key.clone(),
        available_update: m.available_update.clone(),
        checking_update: m.checking_update,
        installing_update: m.installing_update,
        notice: m.notice.clone(),
        error: m.error.clone(),
    })
}

#[tauri::command]
fn get_log(app: AppHandle, step: Step) -> Vec<LogLine> {
    with_model(&app, |m| m.steps.get(&step).map(|s| s.log.clone()).unwrap_or_default())
}

// MARK: Settings

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsInput {
    project_dir: String,
    #[serde(default)]
    workspace_dir: String,
    host: String,
    port: u16,
    user: String,
    remote_dir: String,
    auth: AuthKind,
    key_path: String,
    /// New password or passphrase; empty keeps the stored one.
    secret: String,
    /// Look for a new app version at start.
    #[serde(default = "yes")]
    update_check: bool,
}

fn yes() -> bool {
    true
}

fn apply_settings(app: &AppHandle, input: SettingsInput) -> Result<(), String> {
    let mut next = with_model(app, |m| m.config.clone());
    let previous_account = next.sftp.secret_account();
    let target_changed = next.sftp.host.trim() != input.host.trim() || next.sftp.port != input.port;
    next.project_dir = input.project_dir.trim().to_owned();
    next.workspace_dir = input.workspace_dir.trim().to_owned();
    next.sftp.host = input.host.trim().to_owned();
    next.sftp.port = if input.port == 0 { 22 } else { input.port };
    next.sftp.user = input.user.trim().to_owned();
    next.sftp.remote_dir = input.remote_dir.trim().to_owned();
    next.sftp.auth = input.auth;
    next.sftp.key_path = input.key_path.trim().to_owned();
    next.no_update_check = !input.update_check;
    if target_changed {
        // Another server needs its own confirmed host key.
        next.sftp.host_key.clear();
    }
    if next.sftp.secret_account() != previous_account && next.sftp.secret_saved && input.secret.is_empty() {
        // The keychain entry belongs to the old user/host; ask for the password again.
        next.sftp.secret_saved = false;
    }
    if !input.secret.is_empty() {
        secrets::set(&next.sftp, &input.secret).map_err(|e| format!("{e:#}"))?;
        next.sftp.secret_saved = true;
    }
    config::save(&next).map_err(|e| format!("{e:#}"))?;
    let site = load_site(&next);
    with_model(app, |m| {
        if m.config.project_dir != next.project_dir {
            // Another project: its repositories, previews and results are not this one's.
            m.repo_status.clear();
            m.repo_plan = None;
            m.repo_reports.clear();
        }
        m.config = next;
        m.site = site;
        m.pending_host_key = None;
    });
    changed(app);
    Ok(())
}

#[tauri::command]
async fn save_settings(app: AppHandle, input: SettingsInput) -> Result<(), String> {
    blocking(move || apply_settings(&app, input)).await
}

/// Saves, then connects once and lists the target folder.
#[tauri::command]
async fn test_connection(app: AppHandle, input: SettingsInput) -> Result<String, String> {
    blocking(move || {
        apply_settings(&app, input)?;
        let sftp_config = with_model(&app, |m| m.config.sftp.clone());
        let secret = secrets::get(&sftp_config);
        match sftp::connect(&sftp_config, secret.as_deref()) {
            Ok(connection) => {
                let names = connection.list_root().map_err(|e| format!("{e:#}"))?;
                Ok(format!("Verbunden. Zielordner enthält {} Einträge{}", names.len(), steps::preview(&names)))
            }
            Err(ConnectError::UnknownHostKey(fingerprint)) => {
                with_model(&app, |m| m.pending_host_key = Some(fingerprint));
                changed(&app);
                Err("Bitte zuerst den Server-Schlüssel bestätigen, dann erneut testen.".into())
            }
            Err(e) => Err(e.to_string()),
        }
    })
    .await
}

#[tauri::command]
fn trust_host_key(app: AppHandle, fingerprint: String) -> Result<(), String> {
    let next = with_model(&app, |m| {
        m.config.sftp.host_key = fingerprint;
        m.pending_host_key = None;
        m.config.clone()
    });
    config::save(&next).map_err(|e| format!("{e:#}"))?;
    changed(&app);
    Ok(())
}

#[tauri::command]
fn reject_host_key(app: AppHandle) {
    with_model(&app, |m| m.pending_host_key = None);
    changed(&app);
}

#[tauri::command]
async fn forget_secret(app: AppHandle) -> Result<(), String> {
    blocking(move || {
        let sftp_config = with_model(&app, |m| m.config.sftp.clone());
        secrets::delete(&sftp_config).map_err(|e| format!("{e:#}"))?;
        let next = with_model(&app, |m| {
            m.config.sftp.secret_saved = false;
            m.config.clone()
        });
        config::save(&next).map_err(|e| format!("{e:#}"))?;
        changed(&app);
        Ok(())
    })
    .await
}

#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<String> {
    blocking(move || app.dialog().file().blocking_pick_folder().and_then(|p| p.into_path().ok()).map(|p| p.display().to_string())).await
}

#[tauri::command]
async fn pick_file(app: AppHandle) -> Option<String> {
    blocking(move || app.dialog().file().blocking_pick_file().and_then(|p| p.into_path().ok()).map(|p| p.display().to_string())).await
}

#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("Nur http(s)-Adressen".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

/// Turns an update task on or off and sets its model (empty = default).
#[tauri::command]
fn set_task(app: AppHandle, id: String, enabled: bool, model: String) -> Result<(), String> {
    let next = with_model(&app, |m| {
        let choice = m.config.tasks.entry(id).or_default();
        choice.enabled = Some(enabled);
        choice.model = model.trim().to_owned();
        m.config.clone()
    });
    config::save(&next).map_err(|e| format!("{e:#}"))?;
    changed(&app);
    Ok(())
}

/// Turns watched products on or off; turned-off products are neither checked nor updated.
#[tauri::command]
fn set_products(app: AppHandle, ids: Vec<String>, enabled: bool) -> Result<(), String> {
    let next = with_model(&app, |m| {
        for id in ids {
            if enabled {
                m.config.skipped_products.remove(&id);
            } else {
                m.config.skipped_products.insert(id);
            }
        }
        m.config.clone()
    });
    config::save(&next).map_err(|e| format!("{e:#}"))?;
    changed(&app);
    Ok(())
}

// MARK: App updates

/// Looks for a newer release of the app. Betas count when asked for or when a beta is installed.
/// A check at start stays silent unless it finds something.
fn check_updates(app: &AppHandle, beta: bool, user_initiated: bool) {
    let version = app.package_info().version.to_string();
    if cfg!(debug_assertions) {
        if user_initiated {
            with_model(app, |m| m.notice = Some("Entwicklungsversion – keine Update-Prüfung.".into()));
            changed(app);
        }
        return;
    }
    with_model(app, |m| m.checking_update = true);
    changed(app);
    let result = update::latest_release(beta || update::is_prerelease(&version));
    with_model(app, |m| {
        m.checking_update = false;
        match result {
            Ok(Some(release)) if update::is_newer(&release.version, &version) => m.available_update = Some(release),
            Ok(_) => {
                m.available_update = None;
                if user_initiated {
                    m.notice = Some(format!("OpenReleasePilot {version} ist aktuell."));
                }
            }
            Err(e) if user_initiated => m.error = Some(format!("Update-Prüfung fehlgeschlagen: {e:#}")),
            Err(_) => {}
        }
    });
    changed(app);
}

#[tauri::command]
async fn check_for_updates(app: AppHandle, beta: bool) {
    blocking(move || check_updates(&app, beta, true)).await
}

/// Downloads the package of the available update, checks it and opens it in the macOS installer.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    let release = with_model(&app, |m| {
        if m.installing_update || m.running.is_some() {
            return None;
        }
        let release = m.available_update.clone()?;
        m.installing_update = true;
        Some(release)
    });
    let Some(release) = release else { return Ok(()) };
    changed(&app);
    blocking(move || {
        let result = update::download_package(&release)
            .map_err(|e| format!("{e:#}"))
            .and_then(|path| app.opener().open_path(path.to_string_lossy(), None::<&str>).map_err(|e| e.to_string()));
        with_model(&app, |m| {
            m.installing_update = false;
            match &result {
                Ok(()) => {
                    m.available_update = None;
                    m.notice = Some(format!(
                        "Installer für OpenReleasePilot {} geöffnet. Nach der Installation die App neu starten.",
                        release.version
                    ));
                }
                Err(e) => m.error = Some(format!("Update konnte nicht geladen werden: {e}")),
            }
        });
        changed(&app);
        Ok(())
    })
    .await
}

/// Hides the update banner until the next check.
#[tauri::command]
fn dismiss_update(app: AppHandle) {
    with_model(&app, |m| m.available_update = None);
    changed(&app);
}

#[tauri::command]
fn dismiss(app: AppHandle) {
    with_model(&app, |m| {
        m.notice = None;
        m.error = None;
    });
    changed(&app);
}

// MARK: Steps

#[tauri::command]
fn run_step(app: AppHandle, step: Step, instructions: String) -> Result<(), String> {
    let job = match step {
        Step::Precheck => Job::Precheck,
        Step::Update => Job::Update(instructions),
        Step::Upload => Job::Upload,
        Step::Verify => Job::Verify,
        Step::Repos => return Err("Die Pflege startet im Bereich Repositories".into()),
    };
    steps::start(&app, vec![job])
}

// MARK: Repositories

/// Reads the facts of the repositories (all when `keys` is empty), four at a time, each stored as
/// soon as it arrives. Read only and without Claude; runs beside a step, never under the model lock.
pub fn refresh_status(app: &AppHandle, keys: &[String]) {
    let (site, config) = with_model(app, |m| (m.site.clone(), m.config.clone()));
    let targets: Vec<repos::RepoTarget> = repo_targets(&site, &config).into_iter().filter(|t| keys.is_empty() || keys.contains(&t.key)).collect();
    if targets.is_empty() {
        return;
    }
    let jobs: VecDeque<(repos::RepoTarget, u64)> = with_model(app, |m| {
        targets
            .into_iter()
            .map(|target| {
                let token = next_token();
                let status = m.repo_status.entry(target.key.clone()).or_default();
                status.token = token;
                status.loading = true;
                (target, token)
            })
            .collect()
    });
    changed(app);
    let app = app.clone();
    std::thread::spawn(move || {
        github::token(); // asks gh once, so the view knows whether it is signed in
        let queue = Mutex::new(jobs);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| loop {
                    let Some((target, token)) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() else { break };
                    let current = |m: &Model| m.repo_status.get(&target.key).is_some_and(|s| s.token == token);
                    if !with_model(&app, |m| current(m)) {
                        continue;
                    }
                    let facts = repos::fetch_facts(&target, steps::now());
                    with_model(&app, |m| {
                        let Some(status) = m.repo_status.get_mut(&target.key).filter(|s| s.token == token) else { return };
                        status.loading = false;
                        match &facts.access {
                            // An older reading is worth more than none; it shows its age.
                            repos::Fetch::Failed(why) if facts.failed_entirely() && status.facts.is_some() => status.error = Some(why.clone()),
                            _ => {
                                status.error = None;
                                status.facts = Some(facts);
                                status.details = None;
                            }
                        }
                    });
                    changed(&app);
                });
            }
        });
    });
}

/// "Status aktualisieren": all repositories, or the given ones.
#[tauri::command]
fn refresh_repos(app: AppHandle, keys: Vec<String>) {
    refresh_status(&app, &keys);
}

/// Drops the answers of running status reads; what is already shown stays.
#[tauri::command]
fn cancel_repo_refresh(app: AppHandle) {
    with_model(&app, |m| {
        for status in m.repo_status.values_mut() {
            if status.loading {
                status.token = next_token();
                status.loading = false;
            }
            if status.details_loading {
                status.details_token = next_token();
                status.details_loading = false;
            }
        }
    });
    changed(&app);
}

/// Mergeability and CI of a repository's open pull requests, for its detail view.
#[tauri::command]
fn load_repo_details(app: AppHandle, key: String) -> Result<(), String> {
    let (repo, prs, token) = with_model(&app, |m| {
        let status = m.repo_status.get_mut(&key)?;
        let facts = status.facts.as_ref()?;
        let repo = facts.access.ok()?.full_name.clone();
        let prs = facts.prs.ok()?.clone();
        let token = next_token();
        status.details_token = token;
        status.details_loading = true;
        Some((repo, prs, token))
    })
    .ok_or("Erst den Status des Repositorys lesen")?;
    changed(&app);
    std::thread::spawn(move || {
        let details = repos::fetch_details(&repo, &prs, steps::now());
        with_model(&app, |m| {
            if let Some(status) = m.repo_status.get_mut(&key).filter(|s| s.details_token == token) {
                status.details_loading = false;
                status.details = Some(details);
            }
        });
        changed(&app);
    });
    Ok(())
}

/// Care, first part: GitHub's state and Claude's review of exactly these repositories, as a
/// preview to confirm. One repository from its detail view, or those of the selected products.
#[tauri::command]
fn check_repos(app: AppHandle, keys: Vec<String>, merge: bool, cleanup: bool, model: String) -> Result<(), String> {
    if !merge && !cleanup {
        return Err("Mindestens eine Aktion wählen".into());
    }
    if keys.is_empty() {
        return Err("Kein Repository gewählt".into());
    }
    if github::token().is_none() {
        return Err("Für die Pflege die GitHub-CLI anmelden: gh auth login".into());
    }
    let (site, config) = with_model(&app, |m| (m.site.clone(), m.config.clone()));
    let all = repo_targets(&site, &config);
    let mut targets: Vec<repos::RepoTarget> = Vec::new();
    for key in keys {
        let target = all.iter().find(|t| t.key == key).ok_or_else(|| format!("{key} gehört zu keinem Produkt mehr"))?;
        if !targets.contains(target) {
            targets.push(target.clone());
        }
    }
    steps::start(&app, vec![Job::ReposCheck(steps::RepoOptions { targets, merge, cleanup, model, project: config.project_dir })])
}

/// Care, second part: runs the preview with this id. Taking it and starting the run happen under
/// one lock, so a double click cannot run it twice.
#[tauri::command]
fn run_repos(app: AppHandle, plan_id: u64) -> Result<(), String> {
    steps::start_with(&app, |m| Ok(vec![Job::Repos(repocare::take_plan(&mut m.repo_plan, plan_id)?)]))
}

#[tauri::command]
fn discard_repo_plan(app: AppHandle, plan_id: u64) {
    with_model(&app, |m| {
        if m.repo_plan.as_ref().is_some_and(|set| set.id == plan_id) {
            m.repo_plan = None;
        }
    });
    changed(&app);
}

/// Precheck, then Update (only if something is outdated or there are instructions), Upload, Verify.
#[tauri::command]
fn run_all(app: AppHandle, instructions: String) -> Result<(), String> {
    steps::start(&app, vec![Job::Precheck, Job::Update(instructions), Job::Upload, Job::Verify])
}

/// Claude adds a repository as a new product (runs in the Update slot, ends with a commit).
#[tauri::command]
fn run_add_product(app: AppHandle, input: NewProductInput) -> Result<(), String> {
    if !github::is_repo_name(input.repo.trim()) {
        return Err("Repository im Format owner/name angeben".into());
    }
    steps::start(&app, vec![Job::AddProduct(input)])
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RepoEntry {
    #[serde(flatten)]
    info: github::RepoInfo,
    /// Absolute path of the clone in the repo folder, if any.
    local_path: Option<String>,
    on_site: bool,
    is_website: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RepoList {
    repos: Vec<RepoEntry>,
    workspace: String,
    /// Why only public repositories are listed, if so.
    note: Option<String>,
}

/// The user's GitHub repositories with their local clone and their role on the site.
/// Without a gh login: the public repositories of the owners already on the site.
#[tauri::command]
async fn list_repos(app: AppHandle) -> Result<RepoList, String> {
    let (site, config) = with_model(&app, |m| (m.site.clone().ok(), m.config.clone()));
    blocking(move || {
        let workspace = config.workspace();
        let (repos, note) = match github::user_repos() {
            Ok(repos) => (repos, None),
            Err(e) => {
                let mut owners: Vec<String> = site
                    .iter()
                    .flat_map(|s| s.products.iter())
                    .filter_map(|p| p.repo.split('/').next().map(str::to_owned))
                    .collect();
                owners.sort();
                owners.dedup();
                let mut repos = Vec::new();
                for owner in &owners {
                    repos.extend(github::owner_repos(owner).map_err(|e| format!("{e:#}"))?);
                }
                (repos, Some(format!("{e:#}. Gezeigt werden nur öffentliche Repositories.")))
            }
        };
        let mut clones = localrepo::clones_in(&workspace);
        let website = if config.project_dir.is_empty() { None } else { localrepo::origin_repo(&expand_home(&config.project_dir)) };
        let repos = repos
            .into_iter()
            .map(|info| {
                let key = info.full_name.to_lowercase();
                RepoEntry {
                    local_path: clones.remove(&key).map(|p| p.display().to_string()),
                    on_site: site.iter().flat_map(|s| s.products.iter()).any(|p| p.repo.eq_ignore_ascii_case(&info.full_name)),
                    is_website: website.as_ref().is_some_and(|w| w.eq_ignore_ascii_case(&info.full_name)),
                    info,
                }
            })
            .collect();
        Ok(RepoList { repos, workspace: abbreviate_home(&workspace), note })
    })
    .await
}

/// Clones a repository into the repo folder and returns its path.
#[tauri::command]
async fn clone_repo(app: AppHandle, repo: String, workspace_dir: String) -> Result<String, String> {
    let mut config = with_model(&app, |m| m.config.clone());
    if !workspace_dir.trim().is_empty() {
        config.workspace_dir = workspace_dir;
    }
    blocking(move || {
        if !github::is_repo_name(repo.trim()) {
            return Err("Repository im Format owner/name angeben".to_owned());
        }
        localrepo::clone(repo.trim(), &config.workspace()).map(|p| p.display().to_string()).map_err(|e| format!("{e:#}"))
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RepoCheck {
    info: github::RepoInfo,
    latest_tag: Option<String>,
    /// Absolute path of a clone found next to the project.
    local_path: Option<String>,
    on_site: bool,
}

#[tauri::command]
async fn inspect_repo(app: AppHandle, repo: String) -> Result<RepoCheck, String> {
    let (site, workspace) = with_model(&app, |m| (m.site.clone(), m.config.workspace()));
    let site = site?;
    blocking(move || {
        let info = github::repo_info(repo.trim()).map_err(|e| format!("{e:#}"))?;
        let latest_tag = github::latest_release(&info.full_name).map_err(|e| format!("{e:#}"))?;
        let local_path = localrepo::clones_in(&workspace).remove(&info.full_name.to_lowercase()).map(|p| p.display().to_string());
        let on_site = site.products.iter().any(|p| p.repo.eq_ignore_ascii_case(&info.full_name));
        Ok(RepoCheck { info, latest_tag, local_path, on_site })
    })
    .await
}

#[tauri::command]
fn cancel(app: AppHandle) {
    if let Some(runner) = with_model(&app, |m| m.runner.clone()) {
        runner.cancel();
    }
}

fn main() {
    paths::migrate_legacy_data();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Shared(Mutex::new(Model::load())))
        .setup(|app| {
            // Like ClaudeKeyZapper: look for a new version once at start, quietly.
            let handle = app.handle().clone();
            if !cfg!(debug_assertions) && !with_model(&handle, |m| m.config.no_update_check) {
                std::thread::spawn(move || check_updates(&handle, false, false));
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_view,
            get_log,
            save_settings,
            test_connection,
            trust_host_key,
            reject_host_key,
            forget_secret,
            pick_folder,
            pick_file,
            open_url,
            dismiss,
            run_step,
            run_all,
            run_add_product,
            refresh_repos,
            cancel_repo_refresh,
            load_repo_details,
            check_repos,
            run_repos,
            discard_repo_plan,
            set_task,
            set_products,
            check_for_updates,
            install_update,
            dismiss_update,
            list_repos,
            clone_repo,
            inspect_repo,
            cancel
        ])
        .run(tauri::generate_context!())
        .expect("error while running OpenReleasePilot");
}
