//! The project file `.openftpupdater/site.json`, committed in the website repository.
//!
//! ```json
//! {
//!   "name": "example.com",
//!   "url": "https://example.com",
//!   "build": "npm run build",
//!   "output": "dist",
//!   "skill": "site-update",
//!   "addProductSkill": "product-add",
//!   "failOnOutput": ["[releases]"],
//!   "products": [{ "id": "app", "name": "App", "repo": "owner/app", "tagPrefix": "v", "documented": "1.2.0" }],
//!   "claude": { "allowedTools": ["Bash(node scripts/screenshots/*)"] }
//! }
//! ```

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const SITE_FILE: &str = ".openftpupdater/site.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Site {
    pub name: String,
    /// Public URL of the site; the build output root is served there.
    pub url: String,
    /// Shell command, run in the project folder.
    pub build: String,
    /// Build output folder, relative to the project.
    pub output: String,
    /// Claude Code skill that updates the site; the prompt asks Claude to use it.
    #[serde(default)]
    pub skill: Option<String>,
    /// Claude Code skill that adds a new product (page, listings, site.json entry).
    #[serde(default)]
    pub add_product_skill: Option<String>,
    /// Build output lines that make the build count as failed, e.g. a fallback warning.
    #[serde(default)]
    pub fail_on_output: Vec<String>,
    #[serde(default)]
    pub products: Vec<Product>,
    #[serde(default)]
    pub claude: ClaudeSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Product {
    pub id: String,
    pub name: String,
    /// GitHub `owner/repo` whose latest release is watched.
    pub repo: String,
    /// `v` for tags like `v1.2.0`, empty for `1.2.0`.
    #[serde(default)]
    pub tag_prefix: String,
    /// Release the site content describes; the update skill raises it.
    pub documented: String,
    /// Local clone, relative to the project; only Claude reads it.
    #[serde(default)]
    pub local_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSettings {
    /// Added to the app's default allowlist.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
}

impl Site {
    pub fn output_dir(&self, project: &Path) -> PathBuf {
        project.join(&self.output)
    }
}

pub fn load(project: &Path) -> Result<Site> {
    let file = project.join(SITE_FILE);
    let text = fs::read_to_string(&file).with_context(|| format!("{} nicht gefunden", SITE_FILE))?;
    let site: Site = serde_json::from_str(&text).with_context(|| format!("{} ist ungültig", SITE_FILE))?;
    if site.build.trim().is_empty() || site.output.trim().is_empty() {
        bail!("{}: \"build\" und \"output\" müssen gesetzt sein", SITE_FILE);
    }
    if !site.url.starts_with("https://") && !site.url.starts_with("http://") {
        bail!("{}: \"url\" muss mit http(s):// beginnen", SITE_FILE);
    }
    Ok(site)
}
