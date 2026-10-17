//! OpenReleasePilot core: repository care, content targets and the website steps, without UI.
//!
//! - `config`: the app's own settings (managed repositories, targets, website project, SFTP), stored in the app data folder
//! - `site`: the project's `.openreleasepilot/site.json` (build, output, URL, watched products)
//! - `shell`, `git`, `github`, `gitlab`, `claude`: external tools
//! - `forge`: whether a repository lives on GitHub or on a GitLab instance
//! - `sftp`, `manifest`, `verify`, `secrets`: upload, change tracking, live check, password storage
//! - `update`: updates of the app itself from its GitHub releases
//! - `repos`: the managed repositories, what GitHub reports about them, their health (read only)
//! - `security`: security check of the managed repositories (branch protection, scanning, alerts) and its fixes
//! - `repocare`: merging, tagging and branch cleanup in the managed repositories
//! - `content`: README/documentation and wiki targets: work folder, checks, proposal, publishing
//! - `jobs`: care jobs with their steps, approvals and results, stored so they survive a restart

pub mod claude;
pub mod config;
pub mod content;
pub mod forge;
pub mod git;
pub mod github;
pub mod gitlab;
pub mod jobs;
pub mod localrepo;
pub mod manifest;
pub mod paths;
pub mod repocare;
pub mod repos;
pub mod secrets;
pub mod security;
pub mod sftp;
pub mod shell;
pub mod site;
pub mod update;
pub mod verify;
