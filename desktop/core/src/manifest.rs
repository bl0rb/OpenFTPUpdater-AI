//! What was uploaded: path, size and SHA-256 of every file in the build output.
//!
//! The manifest of the last upload is kept locally per target. The next upload sends only new or
//! changed files and removes only files that the app uploaded itself before; anything else on the
//! server (other domains, `.htaccess`, manual files) is never touched.

use crate::config::SftpConfig;
use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Paths relative to the output folder, always with `/`.
    pub files: BTreeMap<String, FileEntry>,
    #[serde(default)]
    pub commit: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    pub uploaded_at: Option<u64>,
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Hashes every file of the build output (skips `.DS_Store`).
pub fn scan(dir: &Path) -> Result<Manifest> {
    let mut files = BTreeMap::new();
    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = entry.with_context(|| format!("{} nicht lesbar", dir.display()))?;
        if !entry.file_type().is_file() || entry.file_name() == ".DS_Store" {
            continue;
        }
        let relative = entry.path().strip_prefix(dir)?.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
        files.insert(relative, FileEntry { sha256: sha256_file(entry.path())?, size: entry.metadata()?.len() });
    }
    Ok(Manifest { files, commit: None, uploaded_at: None })
}

#[derive(Debug, Default)]
pub struct Diff {
    pub upload: Vec<String>,
    pub delete: Vec<String>,
    pub unchanged: usize,
}

/// New or changed files to upload, files of the last upload that are gone now to delete.
/// Pages come last, so a page never links to an asset that is not there yet.
pub fn diff(previous: &Manifest, current: &Manifest) -> Diff {
    let mut d = Diff::default();
    for (path, entry) in &current.files {
        if previous.files.get(path) == Some(entry) {
            d.unchanged += 1;
        } else {
            d.upload.push(path.clone());
        }
    }
    d.upload.sort_by_key(|p| (p.ends_with(".html"), p.clone()));
    d.delete = previous.files.keys().filter(|p| !current.files.contains_key(*p)).cloned().collect();
    d
}

fn last_upload_file(sftp: &SftpConfig) -> PathBuf {
    let key = format!("{}:{}:{}:{}", sftp.host.trim(), sftp.port, sftp.user.trim(), sftp.remote_dir.trim());
    let hash = hex::encode(Sha256::digest(key.as_bytes()));
    paths::uploads_dir().join(format!("{}.json", &hash[..16]))
}

pub fn load_last(sftp: &SftpConfig) -> Option<Manifest> {
    serde_json::from_str(&fs::read_to_string(last_upload_file(sftp)).ok()?).ok()
}

pub fn save_last(sftp: &SftpConfig, manifest: &Manifest) -> Result<()> {
    let file = last_upload_file(sftp);
    fs::create_dir_all(file.parent().expect("upload file has a parent"))?;
    fs::write(&file, serde_json::to_vec_pretty(manifest)?).context("Upload-Stand speichern")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(entries: &[(&str, &str)]) -> Manifest {
        Manifest {
            files: entries.iter().map(|(p, h)| (p.to_string(), FileEntry { sha256: h.to_string(), size: 1 })).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn uploads_changes_pages_last_and_deletes_only_own_files() {
        let old = manifest(&[("index.html", "a"), ("old.css", "b"), ("logo.svg", "c")]);
        let new = manifest(&[("index.html", "x"), ("app.css", "d"), ("logo.svg", "c")]);
        let d = diff(&old, &new);
        assert_eq!(d.upload, vec!["app.css", "index.html"]);
        assert_eq!(d.delete, vec!["old.css"]);
        assert_eq!(d.unchanged, 1);
    }
}
