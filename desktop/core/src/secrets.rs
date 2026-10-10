//! The SFTP password (or key passphrase) in the system keychain, never in a file.

use crate::config::SftpConfig;
use crate::paths::APP_NAME;
use anyhow::{Context, Result};

fn entry(sftp: &SftpConfig) -> Result<keyring::Entry> {
    keyring::Entry::new(APP_NAME, &sftp.secret_account()).context("Schlüsselbund nicht verfügbar")
}

pub fn get(sftp: &SftpConfig) -> Option<String> {
    entry(sftp).ok()?.get_password().ok()
}

pub fn set(sftp: &SftpConfig, secret: &str) -> Result<()> {
    entry(sftp)?.set_password(secret).context("Passwort konnte nicht im Schlüsselbund gespeichert werden")
}

pub fn delete(sftp: &SftpConfig) -> Result<()> {
    match entry(sftp)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("Passwort konnte nicht entfernt werden"),
    }
}
