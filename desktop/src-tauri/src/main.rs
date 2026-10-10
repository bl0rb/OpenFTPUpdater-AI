// No console window next to the app on Windows (release builds).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod steps;

use steps::{Job, NewProductInput};

use openftpupdater_core::claude::{Outcome, Outdated};
use openftpupdater_core::config::{self, AuthKind, Config};
use openftpupdater_core::{github, localrepo, manifest};
use openftpupdater_core::paths::{abbreviate_home, expand_home};
use openftpupdater_core::secrets;
use openftpupdater_core::sftp::{self, ConnectError};
use openftpupdater_core::shell::Runner;
use openftpupdater_core::site::{self, Site};
use openftpupdater_core::verify::Report;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
}

impl Step {
    pub const ALL: [Step; 4] = [Step::Precheck, Step::Update, Step::Upload, Step::Verify];
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

pub struct Model {
    pub config: Config,
    pub site: Result<Site, String>,
    pub steps: BTreeMap<Step, StepState>,
    pub running: Option<Step>,
    pub runner: Option<Arc<Runner>>,
    pub checks: Vec<Check>,
    /// `None` until a precheck has looked at the releases.
    pub outdated: Option<Vec<Outdated>>,
    /// Claude runs of the last Update or New product, one per task.
    pub claude_runs: Vec<ClaudeRun>,
    pub upload: Option<UploadResult>,
    pub verify: Option<Report>,
    /// Fingerprint the user has to confirm before the first SFTP connection.
    pub pending_host_key: Option<String>,
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
            claude_runs: Vec::new(),
            upload: None,
            verify: None,
            pending_host_key: None,
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
    config: Config,
    project_display: String,
    /// Folder with the clones, as displayed (auto value if not set).
    workspace_display: String,
    /// `owner/name` of the website repository (origin of the project folder).
    site_repo: Option<String>,
    site: Option<Site>,
    site_error: Option<String>,
    sftp_ready: bool,
    steps: BTreeMap<Step, StepState>,
    running: Option<Step>,
    checks: Vec<Check>,
    outdated: Option<Vec<Outdated>>,
    tasks: Vec<steps::ResolvedTask>,
    claude_runs: Vec<ClaudeRun>,
    upload: Option<UploadResult>,
    verify: Option<Report>,
    last_upload: Option<LastUpload>,
    pending_host_key: Option<String>,
    notice: Option<String>,
    error: Option<String>,
}

#[tauri::command]
fn get_view(app: AppHandle) -> View {
    let version = app.package_info().version.to_string();
    with_model(&app, |m| View {
        version,
        config: m.config.clone(),
        project_display: if m.config.project_dir.is_empty() { String::new() } else { abbreviate_home(&expand_home(&m.config.project_dir)) },
        workspace_display: abbreviate_home(&m.config.workspace()),
        site_repo: if m.config.project_dir.is_empty() { None } else { localrepo::origin_repo(&expand_home(&m.config.project_dir)) },
        site: m.site.as_ref().ok().cloned(),
        site_error: m.site.as_ref().err().cloned(),
        sftp_ready: m.config.sftp.is_complete(),
        steps: m.steps.clone(),
        running: m.running,
        checks: m.checks.clone(),
        outdated: m.outdated.clone(),
        tasks: m.site.as_ref().map(|site| steps::resolve_tasks(site, &m.config)).unwrap_or_default(),
        claude_runs: m.claude_runs.clone(),
        upload: m.upload.clone(),
        verify: m.verify.clone(),
        last_upload: manifest::load_last(&m.config.sftp).map(|last| LastUpload {
            commit: last.commit.map(|c| c.chars().take(7).collect()),
            uploaded_at: last.uploaded_at,
            files: last.files.len(),
        }),
        pending_host_key: m.pending_host_key.clone(),
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
    };
    steps::start(&app, vec![job])
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
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Shared(Mutex::new(Model::load())))
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
            set_task,
            list_repos,
            clone_repo,
            inspect_repo,
            cancel
        ])
        .run(tauri::generate_context!())
        .expect("error while running OpenFTPUpdater-AI");
}
