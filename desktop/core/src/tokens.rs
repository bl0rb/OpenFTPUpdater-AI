//! Tokens the user entered in the app. Per token the user chooses where it is kept: in the system
//! keychain, or in the app (`tokens.json` in the data folder, readable by the user only). Never in
//! the config. They come first; the tokens of the CLIs (`gh`, `glab`) are the fallback. The config
//! only says which tokens exist and where (`github_token_saved`, `gitlab_instances`), so showing
//! the window never reads a token: it is read once in the background (`load`) and then served
//! from a cache.

use crate::config::{self, Config, GitlabInstance};
use crate::paths::APP_NAME;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Where an own token is kept.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Storage {
    /// The system keychain.
    #[default]
    Keychain,
    /// A file in the app's data folder.
    App,
}

impl Storage {
    /// `keychain` (also for none/empty) or `app`; anything else is `None`.
    pub fn parse(text: Option<&str>) -> Option<Storage> {
        match text.map(|t| t.trim().to_lowercase()).as_deref() {
            None | Some("") | Some("keychain") => Some(Storage::Keychain),
            Some("app") => Some(Storage::App),
            Some(_) => None,
        }
    }
}

#[derive(Default)]
struct Cache {
    loaded: bool,
    github: Option<String>,
    /// host → token
    gitlab: HashMap<String, String>,
    /// host → `https` or `http`, for every instance of the config
    schemes: HashMap<String, String>,
}

static CACHE: std::sync::LazyLock<Mutex<Cache>> = std::sync::LazyLock::new(|| Mutex::new(Cache::default()));

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Account of the GitHub token.
pub fn github_account() -> String {
    "token:github".into()
}

/// Account of the token for a GitLab host.
pub fn gitlab_account(host: &str) -> String {
    format!("token:gitlab:{}", host.trim().to_lowercase())
}

fn entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(APP_NAME, account).context("Schlüsselbund nicht verfügbar")
}

/// The app's token file, next to the config file.
pub fn app_file() -> PathBuf {
    crate::paths::config_file().with_file_name("tokens.json")
}

fn read_file(path: &Path) -> BTreeMap<String, String> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_file(path: &Path, map: &BTreeMap<String, String>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("App-Datenordner anlegen")?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(map)?).context("Token-Datei schreiben")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).context("Rechte der Token-Datei setzen")?;
    }
    std::fs::rename(&tmp, path).context("Token-Datei speichern")
}

fn file_get(path: &Path, account: &str) -> Option<String> {
    read_file(path).remove(account).filter(|t| !t.is_empty())
}

fn file_set(path: &Path, account: &str, token: &str) -> Result<()> {
    let mut map = read_file(path);
    map.insert(account.to_owned(), token.to_owned());
    write_file(path, &map)
}

/// Removes the account; leaves the file alone if it does not hold it.
fn file_delete(path: &Path, account: &str) -> Result<()> {
    let mut map = read_file(path);
    if map.remove(account).is_some() {
        write_file(path, &map)?;
    }
    Ok(())
}

pub fn get(account: &str, storage: Storage) -> Option<String> {
    if cfg!(test) {
        return None;
    }
    match storage {
        Storage::Keychain => entry(account).ok()?.get_password().ok().filter(|t| !t.is_empty()),
        Storage::App => file_get(&app_file(), account),
    }
}

pub fn set(account: &str, storage: Storage, token: &str) -> Result<()> {
    match storage {
        Storage::Keychain => entry(account)?.set_password(token).context("Token konnte nicht im Schlüsselbund gespeichert werden"),
        Storage::App => file_set(&app_file(), account, token).context("Token konnte nicht in der App gespeichert werden"),
    }
}

/// Removes the token from both places, so switching the storage leaves no stale copy.
pub fn delete(account: &str) -> Result<()> {
    let keychain = match entry(account).and_then(|e| e.delete_credential().map_err(Into::into)) {
        Ok(()) => Ok(()),
        Err(e) if matches!(e.downcast_ref::<keyring::Error>(), Some(keyring::Error::NoEntry)) => Ok(()),
        Err(e) => Err(e.context("Token konnte nicht aus dem Schlüsselbund entfernt werden")),
    };
    let app = file_delete(&app_file(), account).context("Token konnte nicht aus der App entfernt werden");
    keychain.and(app)
}

/// Reads the tokens the config lists, each from the storage it names, and only those, into the cache.
pub fn load(config: &Config) {
    let github = if config.github_token_saved { get(&github_account(), config.github_token_storage) } else { None };
    let mut gitlab = HashMap::new();
    let mut schemes = HashMap::new();
    for GitlabInstance { host, scheme, storage } in &config.gitlab_instances {
        if let Some(token) = get(&gitlab_account(host), *storage) {
            gitlab.insert(host.to_lowercase(), token);
        }
        schemes.insert(host.to_lowercase(), scheme.clone());
    }
    *cache() = Cache { loaded: true, github, gitlab, schemes };
}

/// Loads once from the saved config if nobody did yet, so no request goes out without the own token.
/// Tests never touch the keychain.
pub fn ensure_loaded() {
    if cfg!(test) || cache().loaded {
        return;
    }
    load(&config::load());
}

pub fn github() -> Option<String> {
    ensure_loaded();
    cache().github.clone()
}

pub fn gitlab(host: &str) -> Option<String> {
    ensure_loaded();
    cache().gitlab.get(&host.to_lowercase()).cloned()
}

/// Whether the cache holds an own GitHub token; never reads the keychain.
pub fn has_github_cached() -> bool {
    cache().github.is_some()
}

/// Whether the cache holds an own token for the host; never reads the keychain.
pub fn has_gitlab_cached(host: &str) -> bool {
    cache().gitlab.contains_key(&host.to_lowercase())
}

/// The scheme the user chose for the host, if it is one of the own instances.
pub fn scheme_of(host: &str) -> Option<String> {
    ensure_loaded();
    cache().schemes.get(&host.to_lowercase()).cloned()
}

/// Keeps a just saved (or removed) GitHub token in the cache.
pub fn remember_github(token: Option<String>) {
    let mut cache = cache();
    cache.github = token;
}

/// Keeps a just saved (or removed) GitLab token and scheme in the cache.
pub fn remember_gitlab(host: &str, own: Option<(String, String)>) {
    let host = host.to_lowercase();
    let mut cache = cache();
    match own {
        Some((token, scheme)) => {
            cache.gitlab.insert(host.clone(), token);
            cache.schemes.insert(host, scheme);
        }
        None => {
            cache.gitlab.remove(&host);
            cache.schemes.remove(&host);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_accounts_are_per_service_and_host() {
        assert_eq!(github_account(), "token:github");
        assert_eq!(gitlab_account(" GitLab.Example.com "), "token:gitlab:gitlab.example.com");
        assert_eq!(gitlab_account("192.168.178.88:8929"), "token:gitlab:192.168.178.88:8929");
    }

    #[test]
    fn storage_parses_and_serializes_lowercase() {
        assert_eq!(Storage::parse(None), Some(Storage::Keychain));
        assert_eq!(Storage::parse(Some(" ")), Some(Storage::Keychain));
        assert_eq!(Storage::parse(Some("App")), Some(Storage::App));
        assert_eq!(Storage::parse(Some("disk")), None);
        assert_eq!(serde_json::to_string(&Storage::App).unwrap(), "\"app\"");
        assert_eq!(Storage::default(), Storage::Keychain);
    }

    #[test]
    fn app_file_reads_writes_and_deletes() {
        let dir = std::env::temp_dir().join(format!("orp-tokens-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("sub").join("tokens.json");
        assert_eq!(file_get(&file, "token:github"), None, "no file yet");
        file_set(&file, "token:github", "a").unwrap();
        file_set(&file, "token:gitlab:h", "b").unwrap();
        file_set(&file, "token:github", "c").unwrap();
        assert_eq!(file_get(&file, "token:github").as_deref(), Some("c"));
        assert_eq!(file_get(&file, "token:gitlab:h").as_deref(), Some("b"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        file_delete(&file, "token:github").unwrap();
        file_delete(&file, "token:missing").unwrap();
        assert_eq!(file_get(&file, "token:github"), None);
        assert_eq!(file_get(&file, "token:gitlab:h").as_deref(), Some("b"));
        assert!(!file.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
