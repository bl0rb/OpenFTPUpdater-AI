//! The app's own settings. Never contains secrets: the SFTP password lives in the keychain (`secrets`).

use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum AuthKind {
    #[default]
    Password,
    /// Private key file; a passphrase, if any, is stored like a password.
    Key,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SftpConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Folder on the server that receives the build output, e.g. `/example.com`.
    pub remote_dir: String,
    pub auth: AuthKind,
    pub key_path: String,
    /// Trusted host key as `SHA256:<base64>`; empty until the user trusted the server once.
    pub host_key: String,
    /// Whether a password/passphrase is in the keychain. Kept here so that showing the
    /// window never reads the keychain (which can ask the user for permission).
    pub secret_saved: bool,
}

impl Default for SftpConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 22,
            user: String::new(),
            remote_dir: String::new(),
            auth: AuthKind::Password,
            key_path: String::new(),
            host_key: String::new(),
            secret_saved: false,
        }
    }
}

impl SftpConfig {
    pub fn is_complete(&self) -> bool {
        !self.host.trim().is_empty()
            && !self.user.trim().is_empty()
            && !self.remote_dir.trim().is_empty()
            && match self.auth {
                AuthKind::Password => self.secret_saved,
                AuthKind::Key => !self.key_path.trim().is_empty(),
            }
    }

    /// Keychain account for the password or key passphrase.
    pub fn secret_account(&self) -> String {
        format!("sftp:{}@{}:{}", self.user.trim(), self.host.trim(), self.port)
    }
}

/// The user's choice for one update task.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskChoice {
    /// `None` = the task's default from site.json.
    pub enabled: Option<bool>,
    /// Model alias; empty = the task's or site's default.
    pub model: String,
}

/// A repository the app looks after, independent of any website.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedRepo {
    /// `owner/name` as GitHub reported it when it was added.
    pub repo: String,
    /// GitHub's id, so a rename does not make it a second repository.
    #[serde(default)]
    pub github_id: Option<u64>,
    /// Turned off: stays listed and readable, but no care job takes it.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Local clone (absolute or `~/…`); empty = none. Status and pull request care work without one.
    #[serde(default)]
    pub local_path: String,
    /// How the repository releases; `None` = no automatic tag (never guessed).
    #[serde(default)]
    pub release: Option<ReleaseRule>,
}

fn yes() -> bool {
    true
}

impl ManagedRepo {
    pub fn new(repo: &str) -> Self {
        Self { repo: repo.trim().to_owned(), github_id: None, enabled: true, local_path: String::new(), release: None }
    }

    /// The identity: `owner/name` in lower case.
    pub fn key(&self) -> String {
        self.repo.trim().to_lowercase()
    }
}

/// The release line the app may continue: tags `<prefix>x.y.z`, the next patch only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseRule {
    /// `v` for `v1.2.0`, empty for `1.2.0`.
    pub tag_prefix: String,
    /// A workflow of the repository publishes a GitHub release for each tag; the app waits for it
    /// before steps that need the release.
    #[serde(default)]
    pub publishes_release: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// The repositories the app looks after; the basis of everything else.
    pub repositories: Vec<ManagedRepo>,
    /// README/documentation and wiki targets fed by those repositories.
    pub content_targets: Vec<crate::content::ContentTarget>,
    /// Claude model for reviews and content work without a choice in the job; empty = the
    /// website's setting, else Claude Code's default.
    pub claude_model: String,
    /// Cost limit per Claude run of a care job; `None` = the website's setting, if any.
    pub max_budget_usd: Option<f64>,
    /// Local clone of the website repository (contains `.openreleasepilot/site.json`); optional.
    pub project_dir: String,
    /// Folder that holds the local clones (website and products); empty = the project's parent.
    pub workspace_dir: String,
    pub sftp: SftpConfig,
    /// Task id → choice.
    pub tasks: std::collections::BTreeMap<String, TaskChoice>,
    /// Product ids (site.json) the user turned off: Precheck and Update leave them alone.
    pub skipped_products: std::collections::BTreeSet<String>,
    /// No look for a new version of the app at start (e.g. when IT distributes it).
    pub no_update_check: bool,
}

impl Config {
    pub fn product_enabled(&self, id: &str) -> bool {
        !self.skipped_products.contains(id)
    }

    /// The managed repository with this key (`owner/name`, any case).
    pub fn repository(&self, key: &str) -> Option<&ManagedRepo> {
        let key = key.trim().to_lowercase();
        self.repositories.iter().find(|r| r.key() == key)
    }

    /// Adds a repository unless it is managed already (same `owner/name` or same GitHub id);
    /// returns whether it was added. Never changes an existing entry.
    pub fn add_repository(&mut self, repo: ManagedRepo) -> bool {
        let known = self.repositories.iter().any(|r| r.key() == repo.key() || (repo.github_id.is_some() && r.github_id == repo.github_id));
        if !known {
            self.repositories.push(repo);
        }
        !known
    }

    pub fn content_target(&self, id: &str) -> Option<&crate::content::ContentTarget> {
        self.content_targets.iter().find(|t| t.id == id)
    }

    /// Where clones are searched and created.
    pub fn workspace(&self) -> PathBuf {
        if !self.workspace_dir.trim().is_empty() {
            return paths::expand_home(self.workspace_dir.trim());
        }
        if !self.project_dir.trim().is_empty() {
            if let Some(parent) = paths::expand_home(self.project_dir.trim()).parent() {
                return parent.to_path_buf();
            }
        }
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let code = home.join("Documents").join("Code");
        if code.is_dir() {
            code
        } else {
            home
        }
    }
}

pub fn load() -> Config {
    fs::read_to_string(paths::config_file())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(config: &Config) -> Result<()> {
    let file = paths::config_file();
    fs::create_dir_all(file.parent().expect("config file has a parent")).context("App-Datenordner anlegen")?;
    let tmp = file.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(config)?).context("Einstellungen schreiben")?;
    fs::rename(&tmp, &file).context("Einstellungen speichern")?;
    Ok(())
}

/// The cost limit as typed: empty = none; otherwise a finite positive amount, with a decimal point
/// or comma. Anything else is an error, so a typo never removes the limit.
pub fn parse_budget(text: &str) -> Result<Option<f64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match text.replace(',', ".").parse::<f64>() {
        Ok(amount) if amount.is_finite() && amount > 0.0 => Ok(Some(amount)),
        _ => Err(format!("Kostengrenze „{text}“: bitte einen Betrag größer als 0 eingeben, z. B. 1,50, oder das Feld leeren")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_empty_budget_removes_the_limit() {
        assert_eq!(parse_budget(""), Ok(None));
        assert_eq!(parse_budget("  "), Ok(None));
        assert_eq!(parse_budget("1,50"), Ok(Some(1.5)));
        assert_eq!(parse_budget("1.50"), Ok(Some(1.5)));
        assert_eq!(parse_budget(" 2 "), Ok(Some(2.0)));
        for bad in ["-5", "0", "0,00", "abc", "1.000,50", "inf", "NaN", "1,5,0"] {
            assert!(parse_budget(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_repository_is_managed_once() {
        let mut config = Config::default();
        assert!(config.add_repository(ManagedRepo { github_id: Some(7), ..ManagedRepo::new("Owner/App") }));
        assert!(!config.add_repository(ManagedRepo::new("owner/app")), "same name, other case");
        assert!(!config.add_repository(ManagedRepo { github_id: Some(7), ..ManagedRepo::new("owner/renamed") }), "same GitHub id");
        assert_eq!(config.repositories.len(), 1);
        assert_eq!(config.repository("OWNER/APP").map(|r| r.repo.as_str()), Some("Owner/App"));
    }

    #[test]
    fn older_settings_load_without_repositories() {
        let config: Config = serde_json::from_str(r#"{"projectDir":"~/site","sftp":{"host":"h"}}"#).unwrap();
        assert!(config.repositories.is_empty() && config.content_targets.is_empty());
        assert_eq!(config.project_dir, "~/site");
        let repo: ManagedRepo = serde_json::from_str(r#"{"repo":"o/a"}"#).unwrap();
        assert!(repo.enabled && repo.release.is_none(), "no release rule is guessed");
    }
}
