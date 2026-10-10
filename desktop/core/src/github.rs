//! Latest releases on GitHub, for the precheck and the build.

use crate::shell::output;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

/// Token of the GitHub CLI (`gh auth token`), if it is installed and signed in. Raises the API
/// limit from 60 to 5000 requests per hour; also handed to the build as `GITHUB_TOKEN`.
pub fn token() -> Option<&'static str> {
    static TOKEN: OnceLock<Option<String>> = OnceLock::new();
    TOKEN
        .get_or_init(|| output("gh", &["auth", "token"], Path::new("/")).ok().filter(|t| !t.is_empty()))
        .as_deref()
}

/// Tag of the newest published release (no pre-releases, no drafts).
pub fn latest_tag(repo: &str) -> Result<String> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
    let mut request = agent
        .get(&format!("https://api.github.com/repos/{repo}/releases/latest"))
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "OpenFTPUpdater-AI");
    if let Some(token) = token() {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => bail!("kein veröffentlichtes Release"),
        Err(ureq::Error::Status(403, _)) => bail!("GitHub-Limit erreicht (mit `gh auth login` gilt ein höheres Limit)"),
        Err(e) => return Err(e).context("GitHub nicht erreichbar"),
    };
    let json: serde_json::Value = serde_json::from_reader(response.into_reader()).context("Antwort von GitHub unlesbar")?;
    match json["tag_name"].as_str() {
        Some(tag) if !tag.is_empty() => Ok(tag.to_owned()),
        _ => bail!("Antwort ohne tag_name"),
    }
}

/// Compares dotted versions numerically (`1.10.0` > `1.9.0`); pre-release suffixes are ignored.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|n| n.parse().unwrap_or(0))
            .collect()
    };
    parse(candidate) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_numerically() {
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(is_newer("v1.2.0", "1.1.9"));
        assert!(!is_newer("1.2.0", "1.2.0"));
        assert!(!is_newer("0.7.2", "0.7.10"));
    }
}
