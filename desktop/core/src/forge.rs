//! Where a managed repository lives: GitHub, or a GitLab instance. The identity says it: GitHub
//! repositories are `owner/name`, GitLab projects `host/group/…/name`. A GitHub owner never contains
//! a dot or a colon, a host always does, so the first segment decides. Keys, jobs and settings of
//! GitHub repositories stay exactly as they were.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Forge {
    Github,
    Gitlab { host: String },
}

impl Forge {
    pub fn of(repo: &str) -> Forge {
        match crate::gitlab::project(repo) {
            Some(project) => Forge::Gitlab { host: project.host },
            None => Forge::Github,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Forge::Github => "GitHub",
            Forge::Gitlab { .. } => "GitLab",
        }
    }

    /// The bot that proposes dependency updates there.
    pub fn update_bot(&self) -> &'static str {
        match self {
            Forge::Github => "Dependabot",
            Forge::Gitlab { .. } => "Renovate",
        }
    }

    /// `PR` or `MR`, for texts.
    pub fn request(&self) -> &'static str {
        match self {
            Forge::Github => "PR",
            Forge::Gitlab { .. } => "MR",
        }
    }

    /// How the CLI whose token the app uses signs in.
    pub fn login_hint(&self) -> String {
        match self {
            Forge::Github => "gh auth login".into(),
            Forge::Gitlab { host } => format!("glab auth login --hostname {host}"),
        }
    }

    /// The CLI login, or an own token entered in the settings: for errors that tell the user to sign in.
    pub fn sign_in_hint(&self) -> String {
        format!("{} oder ein eigenes Token in den Einstellungen", self.login_hint())
    }

    /// Whether the CLI is signed in, `None` while unknown (nothing asked yet).
    pub fn signed_in(&self) -> Option<bool> {
        match self {
            Forge::Github => crate::github::signed_in(),
            Forge::Gitlab { host } => crate::gitlab::signed_in(host),
        }
    }

    /// Asks for the token (own, else the CLI's) once; `false` when it is not signed in.
    pub fn sign_in_check(&self) -> bool {
        match self {
            Forge::Github => crate::github::token().is_some(),
            Forge::Gitlab { host } => crate::gitlab::token(host).is_some(),
        }
    }
}

/// `owner/name` on GitHub, `host/group/…/name` on GitLab.
pub fn is_repo_id(text: &str) -> bool {
    match text.trim().split_once('/') {
        Some((first, _)) if crate::gitlab::is_host(first) => crate::gitlab::project(text).is_some(),
        _ => crate::github::is_repo_name(text.trim()),
    }
}

/// The repository without its host: `owner/name`, `group/sub/name`.
pub fn path(repo: &str) -> &str {
    match repo.split_once('/') {
        Some((host, rest)) if crate::gitlab::is_host(host) => rest,
        _ => repo,
    }
}

/// The URL `git clone` uses (the user's own git credentials).
pub fn clone_url(repo: &str) -> String {
    match crate::gitlab::project(repo) {
        Some(p) => format!("{}://{}/{}.git", crate::gitlab::scheme(&p.host), p.host, p.path),
        None => format!("https://github.com/{repo}.git"),
    }
}

/// The URL of the repository's wiki, a git repository of its own on both services.
pub fn wiki_url(repo: &str) -> String {
    match crate::gitlab::project(repo) {
        Some(p) => format!("{}://{}/{}.wiki.git", crate::gitlab::scheme(&p.host), p.host, p.path),
        None => format!("https://github.com/{repo}.wiki.git"),
    }
}

/// The repository's page in the browser.
pub fn web_url(repo: &str) -> String {
    match crate::gitlab::project(repo) {
        Some(p) => p.web_url(),
        None => format!("https://github.com/{repo}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_segment_tells_the_services_apart() {
        assert_eq!(Forge::of("bl0rb/Weave"), Forge::Github);
        assert_eq!(Forge::of("gitlab.example.com/werk/tools/app"), Forge::Gitlab { host: "gitlab.example.com".into() });
        assert_eq!(Forge::of("git.example.com:8443/werk/app"), Forge::Gitlab { host: "git.example.com:8443".into() });
        assert!(is_repo_id("o/app") && is_repo_id("gitlab.example.com/g/app") && is_repo_id("gitlab.example.com/g/sub/app"));
        assert!(!is_repo_id("gitlab.example.com/app"), "a GitLab project lives in a group or user namespace");
        assert!(!is_repo_id("o/a/b"), "GitHub has no subgroups");
        assert_eq!(path("gitlab.example.com/g/sub/app"), "g/sub/app");
        assert_eq!(path("o/app"), "o/app");
        assert_eq!(clone_url("gitlab.example.com/g/app"), "https://gitlab.example.com/g/app.git");
        assert_eq!(Forge::Github.sign_in_hint(), "gh auth login oder ein eigenes Token in den Einstellungen");
        let forge: Forge = serde_json::from_str(r#"{"kind":"gitlab","host":"git.example.com"}"#).unwrap();
        assert_eq!(forge, Forge::Gitlab { host: "git.example.com".into() });
        assert_eq!(serde_json::from_str::<Forge>(r#"{"kind":"github"}"#).unwrap(), Forge::Github);
        assert_eq!(wiki_url("o/app"), "https://github.com/o/app.wiki.git");
        assert_eq!(wiki_url("gitlab.example.com/g/app"), "https://gitlab.example.com/g/app.wiki.git");
    }
}
