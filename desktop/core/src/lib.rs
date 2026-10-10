//! OpenFTPUpdater-AI core: everything the four steps need, without UI.
//!
//! - `config`: the app's own settings (project folder, SFTP target), stored in the app data folder
//! - `site`: the project's `.openftpupdater/site.json` (build, output, URL, watched products)
//! - `shell`, `git`, `github`, `claude`: external tools
//! - `sftp`, `manifest`, `verify`, `secrets`: upload, change tracking, live check, password storage

pub mod claude;
pub mod config;
pub mod git;
pub mod github;
pub mod manifest;
pub mod paths;
pub mod secrets;
pub mod sftp;
pub mod shell;
pub mod site;
pub mod verify;
