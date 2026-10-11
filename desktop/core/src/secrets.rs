//! The SFTP password (or key passphrase) in the system keychain, never in a file.

use crate::config::SftpConfig;
use crate::paths::{APP_NAME, LEGACY_APP_NAME};
use anyhow::{Context, Result};

fn entry(sftp: &SftpConfig) -> Result<keyring::Entry> {
    keyring::Entry::new(APP_NAME, &sftp.secret_account()).context("Schlüsselbund nicht verfügbar")
}

pub fn get(sftp: &SftpConfig) -> Option<String> {
    if let Some(secret) = entry(sftp).ok()?.get_password().ok() {
        return Some(secret);
    }
    // Saved by the app under its old name: move it over once.
    let legacy = keyring::Entry::new(LEGACY_APP_NAME, &sftp.secret_account()).ok()?;
    let secret = legacy.get_password().ok()?;
    if set(sftp, &secret).is_ok() {
        let _ = legacy.delete_credential();
    }
    Some(secret)
}

pub fn set(sftp: &SftpConfig, secret: &str) -> Result<()> {
    entry(sftp)?.set_password(secret).context("Passwort konnte nicht im Schlüsselbund gespeichert werden")
}

pub fn delete(sftp: &SftpConfig) -> Result<()> {
    // Otherwise `get` would bring a not yet moved entry back.
    if let Ok(legacy) = keyring::Entry::new(LEGACY_APP_NAME, &sftp.secret_account()) {
        let _ = legacy.delete_credential();
    }
    match entry(sftp)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("Passwort konnte nicht entfernt werden"),
    }
}
