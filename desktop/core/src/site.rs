//! The project file `.openftpupdater/site.json`, committed in the website repository.
//!
//! ```json
//! {
//!   "name": "example.com",
//!   "url": "https://example.com",
//!   "build": "npm run build",
//!   "output": "dist",
//!   "addProductSkill": "product-add",
//!   "tasks": [{ "id": "texts", "label": "Texte", "skill": "site-texts", "effort": "high" }],
//!   "failOnOutput": ["[releases]"],
//!   "products": [{ "id": "app", "name": "App", "repo": "owner/app", "tagPrefix": "v", "documented": "1.2.0" }],
//!   "claude": { "model": "opus", "effort": "high", "maxBudgetUsd": 15, "allowedTools": ["Bash(node scripts/screenshots/*)"] }
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
    /// Single update skill, for sites without `tasks`.
    #[serde(default)]
    pub skill: Option<String>,
    /// Selectable update tasks, each backed by its own skill and run as its own Claude session.
    #[serde(default)]
    pub tasks: Vec<Task>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    pub label: String,
    /// Skill that does the task; empty means a plain prompt.
    #[serde(default)]
    pub skill: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Selected unless the user turns it off.
    #[serde(default = "selected_by_default")]
    pub default: bool,
    /// Model alias or id (`opus`, `sonnet`, `fable`, `haiku`, …); falls back to `claude.model`.
    #[serde(default)]
    pub model: Option<String>,
    /// `low` … `max`; falls back to `claude.effort`.
    #[serde(default)]
    pub effort: Option<String>,
}

fn selected_by_default() -> bool {
    true
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
    #[serde(default)]
    pub effort: Option<String>,
    /// Cost limit per Claude run (`--max-budget-usd`).
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
}

impl Site {
    /// The update tasks; a site with only `skill` gets one task for it.
    pub fn update_tasks(&self) -> Vec<Task> {
        if !self.tasks.is_empty() {
            return self.tasks.clone();
        }
        vec![Task {
            id: "update".into(),
            label: "Update".into(),
            skill: self.skill.clone().unwrap_or_default(),
            description: None,
            default: true,
            model: None,
            effort: None,
        }]
    }

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
