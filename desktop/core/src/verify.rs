//! Compares the live site with the build output, file by file, byte for byte.

use crate::manifest::Manifest;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub checked: usize,
    pub identical: usize,
    /// `path: reason`, e.g. `index.html: anderer Inhalt` or `logo.svg: HTTP 404`.
    pub problems: Vec<String>,
}

fn url_for(base: &str, path: &str) -> String {
    let encoded: String = path
        .split('/')
        .map(|segment| {
            segment
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/");
    format!("{}/{}", base.trim_end_matches('/'), encoded)
}

fn check(agent: &ureq::Agent, base: &str, path: &str, expected: &str) -> Result<(), String> {
    let response = agent.get(&url_for(base, path)).call().map_err(|e| match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        other => other.to_string(),
    })?;
    let mut body = Vec::new();
    response.into_reader().take(256 * 1024 * 1024).read_to_end(&mut body).map_err(|e| e.to_string())?;
    if hex::encode(Sha256::digest(&body)) == expected {
        Ok(())
    } else {
        Err("anderer Inhalt".into())
    }
}

/// Fetches every file (dotfiles excepted, servers usually hide them) with a few parallel requests.
pub fn compare(base: &str, manifest: &Manifest, on_progress: impl Fn(usize, usize) + Sync) -> Report {
    let files: Vec<(&String, &String)> = manifest
        .files
        .iter()
        .filter(|(path, _)| !path.split('/').any(|s| s.starts_with('.')))
        .map(|(path, entry)| (path, &entry.sha256))
        .collect();
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent("OpenReleasePilot verify")
        .build();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let problems = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some((path, sha)) = files.get(i) else { break };
                if let Err(reason) = check(&agent, base, path, sha) {
                    problems.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{path}: {reason}"));
                }
                on_progress(done.fetch_add(1, Ordering::SeqCst) + 1, files.len());
            });
        }
    });
    let mut problems = problems.into_inner().unwrap_or_else(|e| e.into_inner());
    problems.sort();
    Report { checked: files.len(), identical: files.len() - problems.len(), problems }
}

#[cfg(test)]
mod tests {
    use super::url_for;

    #[test]
    fn encodes_segments() {
        assert_eq!(url_for("https://example.com/", "a b/ü.html"), "https://example.com/a%20b/%C3%BC.html");
    }
}
