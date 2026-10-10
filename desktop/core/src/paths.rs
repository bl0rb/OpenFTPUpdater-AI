use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "OpenFTPUpdater-AI";

/// `~/Library/Application Support/OpenFTPUpdater-AI` on macOS, `%APPDATA%\OpenFTPUpdater-AI` on Windows.
pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join(APP_NAME)
}

pub fn config_file() -> PathBuf {
    data_dir().join("config.json")
}

/// One manifest per upload target, see `manifest::last_upload_file`.
pub fn uploads_dir() -> PathBuf {
    data_dir().join("uploads")
}

/// `/Users/me/Code/site` → `~/Code/site` for display.
pub fn abbreviate_home(path: &Path) -> String {
    match dirs::home_dir() {
        Some(home) if path.starts_with(&home) => format!("~/{}", path.strip_prefix(&home).unwrap_or(path).display()),
        _ => path.display().to_string(),
    }
}

/// Expands a leading `~/`.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}
