//! Updates of the app itself, as in ClaudeKeyZapper: the newest GitHub release of
//! `bl0rb/OpenFTPUpdater-AI`, its `.pkg` downloaded and verified against the SHA-256 digest GitHub
//! publishes for the asset, then handed to the macOS installer.

use crate::github;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

pub const REPOSITORY: &str = "bl0rb/OpenFTPUpdater-AI";
const PACKAGE_EXTENSION: &str = ".pkg";

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    pub version: String,
    /// GitHub pre-release (tags like `0.2.0-beta.1`).
    pub prerelease: bool,
    pub page_url: String,
    #[serde(skip)]
    pub package_url: Option<String>,
    /// Hex SHA-256 of the package as GitHub publishes it (`digest` of the release asset).
    #[serde(skip)]
    pub package_sha256: Option<String>,
}

/// The newest release; with `include_prereleases` also betas (whichever version is highest).
/// `None` if nothing is published yet.
pub fn latest_release(include_prereleases: bool) -> Result<Option<ReleaseInfo>> {
    if !include_prereleases {
        return Ok(github::get(&format!("repos/{REPOSITORY}/releases/latest"))?.as_ref().and_then(release_info));
    }
    let list = github::get(&format!("repos/{REPOSITORY}/releases?per_page=30"))?;
    Ok(list.as_ref().and_then(newest))
}

/// The highest-versioned published release in a `GET /releases` list.
fn newest(releases: &serde_json::Value) -> Option<ReleaseInfo> {
    releases
        .as_array()?
        .iter()
        .filter(|r| r["draft"].as_bool() != Some(true))
        .filter_map(release_info)
        .max_by(|a, b| compare_versions(&a.version, &b.version))
}

fn release_info(release: &serde_json::Value) -> Option<ReleaseInfo> {
    // Only packages served by GitHub itself; `*.pkg.sha256` does not end with `.pkg`.
    let asset = release["assets"].as_array()?.iter().find(|a| {
        a["name"].as_str().is_some_and(|n| n.ends_with(PACKAGE_EXTENSION))
            && a["browser_download_url"].as_str().is_some_and(|u| u.starts_with("https://github.com/"))
    });
    Some(ReleaseInfo {
        version: normalized(release["tag_name"].as_str()?),
        prerelease: release["prerelease"].as_bool().unwrap_or(false),
        page_url: release["html_url"].as_str()?.to_owned(),
        package_url: asset.and_then(|a| a["browser_download_url"].as_str()).map(String::from),
        package_sha256: asset.and_then(|a| a["digest"].as_str()).and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase),
    })
}

fn normalized(version: &str) -> String {
    version.strip_prefix('v').unwrap_or(version).to_owned()
}

/// Semantic version order: numeric fields, then a pre-release (`1.2.0-beta.2`) before its release
/// (`1.2.0`), pre-release fields compared numerically where both are numbers.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let split = |v: &str| {
        let v = normalized(v);
        let (core, pre) = v.split_once('-').map(|(c, p)| (c.to_owned(), Some(p.to_owned()))).unwrap_or((v.clone(), None));
        (core.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>(), pre)
    };
    let ((ca, pa), (cb, pb)) = (split(a), split(b));
    for i in 0..ca.len().max(cb.len()) {
        let order = ca.get(i).unwrap_or(&0).cmp(cb.get(i).unwrap_or(&0));
        if order.is_ne() {
            return order;
        }
    }
    match (pa, pb) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(pa), Some(pb)) => {
            let (fa, fb): (Vec<&str>, Vec<&str>) = (pa.split('.').collect(), pb.split('.').collect());
            for i in 0..fa.len().max(fb.len()) {
                let order = match (fa.get(i), fb.get(i)) {
                    (None, _) => Ordering::Less,
                    (_, None) => Ordering::Greater,
                    (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                        (Ok(x), Ok(y)) => x.cmp(&y),
                        _ => x.cmp(y),
                    },
                };
                if order.is_ne() {
                    return order;
                }
            }
            Ordering::Equal
        }
    }
}

/// Beta and other pre-release versions (`0.2.0-beta.1`).
pub fn is_prerelease(version: &str) -> bool {
    version.contains('-')
}

pub fn is_newer(candidate: &str, installed: &str) -> bool {
    compare_versions(candidate, installed) == Ordering::Greater
}

/// Downloads the package and verifies its SHA-256; refuses without a published checksum.
/// The previous download is removed first.
pub fn download_package(release: &ReleaseInfo) -> Result<PathBuf> {
    let Some(url) = &release.package_url else { bail!("Das Release enthält kein Installationspaket (.pkg)") };
    let Some(expected) = &release.package_sha256 else {
        bail!("Für das Paket ist keine Prüfsumme veröffentlicht. Bitte manuell von der Release-Seite installieren.")
    };
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(300)).build();
    let response = agent.get(url).set("User-Agent", "OpenFTPUpdater-AI").call().context("Paket konnte nicht geladen werden")?;
    let mut data = Vec::new();
    response.into_reader().take(512 * 1024 * 1024).read_to_end(&mut data).context("Paket konnte nicht geladen werden")?;
    if hex_sha256(&data) != *expected {
        bail!("Prüfsumme des heruntergeladenen Pakets stimmt nicht. Installation abgebrochen.");
    }
    let name = url.rsplit('/').next().filter(|n| n.ends_with(PACKAGE_EXTENSION)).unwrap_or("OpenFTPUpdater-AI.pkg");
    let dir = std::env::temp_dir().join("OpenFTPUpdater-AI-Update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).context("Download-Ordner anlegen")?;
    let target = dir.join(name);
    std::fs::write(&target, &data).context("Paket speichern")?;
    Ok(target)
}

fn hex_sha256(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(is_newer("0.2.0-beta.2", "0.2.0-beta.1"));
        assert!(is_newer("0.2.0-beta.10", "0.2.0-beta.9"));
        assert!(is_newer("0.2.0", "0.2.0-beta.2"));
        assert!(is_newer("0.2.0-beta.1", "0.1.1"));
        assert!(!is_newer("0.2.0-beta.1", "0.2.0"));
    }

    #[test]
    fn picks_the_github_package_and_its_digest() {
        let release: serde_json::Value = serde_json::from_str(
            r#"{"tag_name":"0.1.2","html_url":"https://github.com/x/y/releases/0.1.2","assets":[
              {"name":"OpenFTPUpdater-AI-0.1.2.pkg.sha256","browser_download_url":"https://github.com/x/y/a.pkg.sha256","digest":"sha256:000"},
              {"name":"Evil.pkg","browser_download_url":"https://evil.example/Evil.pkg","digest":"sha256:111"},
              {"name":"OpenFTPUpdater-AI-0.1.2.pkg","browser_download_url":"https://github.com/x/y/OpenFTPUpdater-AI-0.1.2.pkg","digest":"sha256:ABC"}]}"#,
        )
        .unwrap();
        let info = release_info(&release).unwrap();
        assert_eq!(info.version, "0.1.2");
        assert!(!info.prerelease);
        assert_eq!(info.package_sha256.as_deref(), Some("abc"));
        assert_eq!(info.package_url.as_deref(), Some("https://github.com/x/y/OpenFTPUpdater-AI-0.1.2.pkg"));
    }

    #[test]
    fn newest_includes_betas_but_skips_drafts() {
        let list: serde_json::Value = serde_json::from_str(
            r#"[
              {"tag_name":"0.1.1","prerelease":false,"html_url":"https://github.com/x/y/releases/0.1.1","assets":[]},
              {"tag_name":"0.2.0-beta.2","prerelease":true,"html_url":"https://github.com/x/y/releases/b2","assets":[]},
              {"tag_name":"0.2.0-beta.10","prerelease":true,"html_url":"https://github.com/x/y/releases/b10","assets":[]},
              {"tag_name":"0.3.0","draft":true,"html_url":"https://github.com/x/y/releases/d","assets":[]}]"#,
        )
        .unwrap();
        let info = newest(&list).unwrap();
        assert_eq!(info.version, "0.2.0-beta.10");
        assert!(info.prerelease);
    }

    /// `cargo test -p openftpupdater-core -- --ignored`: real release, real download, no install.
    #[test]
    #[ignore = "network"]
    fn downloads_and_verifies_the_published_package() {
        let release = latest_release(false).unwrap().expect("a published release");
        let path = download_package(&release).unwrap();
        assert!(path.ends_with(format!("OpenFTPUpdater-AI-{}.pkg", release.version)));
        assert!(path.metadata().unwrap().len() > 1_000_000);
    }
}
