//! Updates of the app itself, as in ClaudeKeyZapper: the newest GitHub release of
//! `bl0rb/OpenReleasePilot`, its `.pkg` downloaded and verified against the SHA-256 digest GitHub
//! publishes for the asset and against the release's minisign signature (`<package>.minisig`), then
//! handed to the macOS installer. The signature is made in CI with a key whose public half is built
//! into the app (`desktop/update-key.pub`): the hash alone only proves the download is what GitHub
//! serves, the signature proves the release was built by our pipeline.

use crate::github;
use anyhow::{bail, Context, Result};
use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

pub const REPOSITORY: &str = "bl0rb/OpenReleasePilot";
const PACKAGE_EXTENSION: &str = ".pkg";
const SIGNATURE_EXTENSION: &str = ".minisig";
/// minisign public key of the release pipeline (the file `minisign -G` writes).
const UPDATE_PUBLIC_KEY: &str = include_str!("../../update-key.pub");

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
    /// `<package>.minisig` of the same release, if published.
    #[serde(skip)]
    pub signature_url: Option<String>,
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
    let assets = release["assets"].as_array()?;
    let from_github = |a: &&serde_json::Value| a["browser_download_url"].as_str().is_some_and(|u| u.starts_with("https://github.com/"));
    let asset = assets.iter().filter(from_github).find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(PACKAGE_EXTENSION)));
    let signature = asset.and_then(|a| a["name"].as_str()).and_then(|package| {
        let name = format!("{package}{SIGNATURE_EXTENSION}");
        assets.iter().filter(from_github).find(|a| a["name"].as_str() == Some(name.as_str()))
    });
    Some(ReleaseInfo {
        version: normalized(release["tag_name"].as_str()?),
        prerelease: release["prerelease"].as_bool().unwrap_or(false),
        page_url: release["html_url"].as_str()?.to_owned(),
        package_url: asset.and_then(|a| a["browser_download_url"].as_str()).map(String::from),
        package_sha256: asset.and_then(|a| a["digest"].as_str()).and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase),
        signature_url: signature.and_then(|a| a["browser_download_url"].as_str()).map(String::from),
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

/// Downloads the package and verifies its SHA-256 and its minisign signature; refuses without a
/// published checksum or signature. The previous download is removed first.
pub fn download_package(release: &ReleaseInfo) -> Result<PathBuf> {
    let Some(url) = &release.package_url else { bail!("Das Release enthält kein Installationspaket (.pkg)") };
    let Some(expected) = &release.package_sha256 else {
        bail!("Für das Paket ist keine Prüfsumme veröffentlicht. Bitte manuell von der Release-Seite installieren.")
    };
    let Some(signature_url) = &release.signature_url else {
        bail!("Für das Paket ist keine Signatur veröffentlicht. Bitte manuell von der Release-Seite installieren.")
    };
    let data = download(url, 512 * 1024 * 1024).context("Paket konnte nicht geladen werden")?;
    if hex_sha256(&data) != *expected {
        bail!("Prüfsumme des heruntergeladenen Pakets stimmt nicht. Installation abgebrochen.");
    }
    let name = url.rsplit('/').next().filter(|n| n.ends_with(PACKAGE_EXTENSION)).unwrap_or("OpenReleasePilot.pkg");
    let signature = download(signature_url, 16 * 1024).context("Signatur konnte nicht geladen werden")?;
    verify_signature(UPDATE_PUBLIC_KEY, &data, &String::from_utf8_lossy(&signature), name)?;
    let dir = std::env::temp_dir().join("OpenReleasePilot-Update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).context("Download-Ordner anlegen")?;
    let target = dir.join(name);
    std::fs::write(&target, &data).context("Paket speichern")?;
    Ok(target)
}

fn download(url: &str, limit: u64) -> Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(300)).build();
    let response = agent.get(url).set("User-Agent", "OpenReleasePilot").call()?;
    let mut data = Vec::new();
    response.into_reader().take(limit).read_to_end(&mut data)?;
    Ok(data)
}

/// Checks `data` against a minisign signature by `public_key`. The trusted comment (signed too) must
/// name the package file, so a signature of an older release cannot be passed off for a newer one.
fn verify_signature(public_key: &str, data: &[u8], signature: &str, package_name: &str) -> Result<()> {
    let invalid = "Die Signatur des Pakets ist ungültig. Installation abgebrochen.";
    let key = PublicKey::decode(public_key.trim()).map_err(|_| anyhow::anyhow!(invalid))?;
    let signature = Signature::decode(signature.trim()).map_err(|_| anyhow::anyhow!(invalid))?;
    key.verify(data, &signature, false).map_err(|_| anyhow::anyhow!(invalid))?;
    if signature.trusted_comment().trim() != package_name {
        bail!(invalid);
    }
    Ok(())
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
              {"name":"OpenReleasePilot-0.1.2.pkg.sha256","browser_download_url":"https://github.com/x/y/a.pkg.sha256","digest":"sha256:000"},
              {"name":"Evil.pkg","browser_download_url":"https://evil.example/Evil.pkg","digest":"sha256:111"},
              {"name":"OpenReleasePilot-0.1.2.pkg","browser_download_url":"https://github.com/x/y/OpenReleasePilot-0.1.2.pkg","digest":"sha256:ABC"}]}"#,
        )
        .unwrap();
        let info = release_info(&release).unwrap();
        assert_eq!(info.version, "0.1.2");
        assert!(!info.prerelease);
        assert_eq!(info.package_sha256.as_deref(), Some("abc"));
        assert_eq!(info.package_url.as_deref(), Some("https://github.com/x/y/OpenReleasePilot-0.1.2.pkg"));
        assert_eq!(info.signature_url, None);
    }

    #[test]
    fn finds_the_signature_of_the_package() {
        let release: serde_json::Value = serde_json::from_str(
            r#"{"tag_name":"0.1.2","html_url":"https://github.com/x/y/releases/0.1.2","assets":[
              {"name":"OpenReleasePilot-0.1.2.pkg.minisig","browser_download_url":"https://evil.example/s.minisig"},
              {"name":"OpenReleasePilot-0.1.2.pkg","browser_download_url":"https://github.com/x/y/OpenReleasePilot-0.1.2.pkg","digest":"sha256:ABC"},
              {"name":"OpenReleasePilot-0.1.2.pkg.minisig","browser_download_url":"https://github.com/x/y/OpenReleasePilot-0.1.2.pkg.minisig"}]}"#,
        )
        .unwrap();
        let info = release_info(&release).unwrap();
        assert_eq!(info.signature_url.as_deref(), Some("https://github.com/x/y/OpenReleasePilot-0.1.2.pkg.minisig"));
    }

    // Throwaway key pair made with `minisign -G -W` for this test only.
    const TEST_KEY: &str = "untrusted comment: minisign public key 5E3DB03C39D0F256\nRWRW8tA5PLA9Xj3/OVZXGzEPv3l9N93BJ/HpxOz55CQlKq3CcEXmTH/u\n";
    const TEST_SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RURW8tA5PLA9XuoEKLOOQvJmARLNH2zMIUo9LGr70Vlo6yUgLSG0WBcuyF/f4mo1EBuBb5DPCK1eR58h53bKF6wOw8ikrzI8BQE=
trusted comment: OpenFTPUpdater-AI-0.1.2.pkg
MkR1W0Q+Mzft01gu2TOgUqMhi1FdrIwvfKSSIT3gQLHb6wGk8nAx5iP5niU3lU5KfSusXk0F19uiczJfon54AQ==
";

    #[test]
    fn verifies_signature_and_package_name() {
        let name = "OpenFTPUpdater-AI-0.1.2.pkg";
        assert!(verify_signature(TEST_KEY, b"hello\n", TEST_SIGNATURE, name).is_ok());
        assert!(verify_signature(TEST_KEY, b"tampered\n", TEST_SIGNATURE, name).is_err());
        assert!(verify_signature(TEST_KEY, b"hello\n", TEST_SIGNATURE, "OpenFTPUpdater-AI-0.1.3.pkg").is_err());
        assert!(verify_signature(TEST_KEY, b"hello\n", "garbage", name).is_err());
    }

    #[test]
    fn built_in_update_key_is_valid() {
        assert!(PublicKey::decode(UPDATE_PUBLIC_KEY.trim()).is_ok());
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

    /// `cargo test -p openreleasepilot-core -- --ignored`: real release, real download, no install.
    #[test]
    #[ignore = "network"]
    fn downloads_and_verifies_the_published_package() {
        let release = latest_release(false).unwrap().expect("a published release");
        let path = download_package(&release).unwrap();
        assert!(path.ends_with(format!("OpenReleasePilot-{}.pkg", release.version)));
        assert!(path.metadata().unwrap().len() > 1_000_000);
    }
}
