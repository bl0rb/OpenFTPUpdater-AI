//! OpenReleasePilot core: everything the four steps need, without UI.
//!
//! - `config`: the app's own settings (project folder, SFTP target), stored in the app data folder
//! - `site`: the project's `.openreleasepilot/site.json` (build, output, URL, watched products)
//! - `shell`, `git`, `github`, `claude`: external tools
//! - `sftp`, `manifest`, `verify`, `secrets`: upload, change tracking, live check, password storage
//! - `update`: updates of the app itself from its GitHub releases
//! - `repos`: the products' repositories, what GitHub reports about them, their health (read only)
//! - `repocare`: merging, tagging and branch cleanup in the products' repositories

pub mod claude;
pub mod config;
pub mod git;
pub mod github;
pub mod localrepo;
pub mod manifest;
pub mod paths;
pub mod repocare;
pub mod repos;
pub mod secrets;
pub mod sftp;
pub mod shell;
pub mod site;
pub mod update;
pub mod verify;
