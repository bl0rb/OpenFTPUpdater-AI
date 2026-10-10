//! The app's own settings. Never contains secrets: the SFTP password lives in the keychain (`secrets`).

use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// Local clone of the website repository (contains `.openftpupdater/site.json`).
    pub project_dir: String,
    pub sftp: SftpConfig,
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
