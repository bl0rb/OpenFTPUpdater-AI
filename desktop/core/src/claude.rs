//! Runs Claude Code headless (`claude -p`) in the project and turns its stream-json output
//! into readable log lines.
//!
//! Claude may edit files in the project, read other repositories through git, look up releases
//! and run the build. Everything else is denied without a prompt, because `-p` cannot ask.
//! Committing, pushing and uploading are the app's job, never Claude's.

use crate::site::{Product, Site, Task};
use serde_json::Value;

/// Tools Claude may use without asking; site.json `claude.allowedTools` adds to it.
pub const DEFAULT_ALLOWED: &[&str] = &[
    "Skill",
    "Read",
    "Glob",
    "Grep",
    "Edit",
    "MultiEdit",
    "Write",
    "TodoWrite",
    "Bash(git status*)",
    "Bash(git diff*)",
    "Bash(git log*)",
    "Bash(git show*)",
    "Bash(git -C * log *)",
    "Bash(git -C * show *)",
    "Bash(git -C * diff *)",
    "Bash(git -C * tag *)",
    "Bash(git -C * describe *)",
    "Bash(git -C * ls-tree *)",
    "Bash(git -C * fetch --tags*)",
    "Bash(gh release list *)",
    "Bash(gh release view *)",
    "Bash(grep *)",
    "Bash(ls *)",
];

/// Denied even if an allow rule would match.
pub const DEFAULT_DISALLOWED: &[&str] = &[
    "Bash(git push*)",
    "Bash(git -C * push*)",
    "Bash(git commit*)",
    "Bash(git -C * commit*)",
    "Bash(git reset*)",
    "Bash(git -C * reset*)",
    "Bash(git checkout*)",
    "Bash(git -C * checkout*)",
    "Bash(rm *)",
];

/// A product whose newest release is newer than the documented one.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outdated {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub documented: String,
    pub latest_tag: String,
}

impl Outdated {
    pub fn new(product: &Product, latest_tag: String) -> Self {
        Self {
            id: product.id.clone(),
            name: product.name.clone(),
            repo: product.repo.clone(),
            documented: product.documented.clone(),
            latest_tag,
        }
    }

    pub fn latest_version(&self) -> &str {
        self.latest_tag.trim_start_matches('v')
    }
}

/// `skipped`: names of the products the user turned off; Claude must leave them alone.
pub fn task_prompt(site: &Site, task: &Task, outdated: &[Outdated], skipped: &[String], instructions: &str) -> String {
    let mut text = if task.skill.is_empty() {
        format!("Aktualisiere die Website {} in diesem Repository.\n\n", site.name)
    } else {
        format!("Aufgabe „{}“ für die Website {}: Nutze den Skill {}.\n\n", task.label, site.name, task.skill)
    };
    if outdated.is_empty() {
        text.push_str("Kein Produkt hat ein neues Release.\n");
    } else {
        text.push_str("Veraltete Produkte (dokumentierter Stand → neuestes Release):\n");
        for o in outdated {
            text.push_str(&format!("- {} ({}): {} → {} (Tag {})\n", o.name, o.repo, o.documented, o.latest_version(), o.latest_tag));
        }
    }
    if !skipped.is_empty() {
        let selected: Vec<&str> = site.products.iter().map(|p| p.name.as_str()).filter(|n| !skipped.iter().any(|s| s == n)).collect();
        text.push_str(&format!(
            "Ausgewählte Produkte: {}. Nicht ausgewählt, daran nichts ändern: {}.\n",
            if selected.is_empty() { "keine".to_owned() } else { selected.join(", ") },
            skipped.join(", ")
        ));
    }
    if !instructions.trim().is_empty() {
        text.push_str(&format!("\nZusätzliche Anweisung:\n{}\n", instructions.trim()));
    }
    if !site.tasks.is_empty() {
        text.push_str("\nBearbeite nur diese Aufgabe; die anderen Aufgaben laufen in eigenen Durchgängen.\n");
    }
    text.push_str(&closing(site, "eine Zeile pro Produkt"));
    text
}

fn closing(site: &Site, summary: &str) -> String {
    format!(
        "\nAndere Repositories liest du nur mit einzelnen Befehlen wie `git -C <Pfad> show <Tag>:<Datei>`, ohne cd, Pipes oder &&; \
         zusammengesetzte Befehle werden ohne Rückfrage abgelehnt.\
         \nDer Build ({}) muss am Ende fehlerfrei laufen. Committe und pushe nicht; das übernimmt die App. \
         Beende deine Antwort mit einer kurzen Zusammenfassung auf Deutsch, {summary}, danach offene Punkte.",
        site.build
    )
}

/// A repository that becomes a new product on the site.
#[derive(Debug, Clone)]
pub struct NewProduct {
    pub repo: String,
    pub name: String,
    /// Local clone, as site.json should reference it (e.g. `../app`).
    pub local_path: Option<String>,
    pub latest_tag: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub private: bool,
}

/// `site_file`: the project file as `site::file_name` finds it.
pub fn add_product_prompt(site: &Site, site_file: &str, product: &NewProduct, instructions: &str) -> String {
    let mut text = match &site.add_product_skill {
        Some(skill) => format!("Nutze den Skill {skill}, um ein neues Produkt auf der Website {} aufzunehmen.\n\n", site.name),
        None => format!(
            "Nimm ein neues Produkt auf der Website {} auf: Produktseite, alle Produktlisten und einen Eintrag in {site_file}.\n\n",
            site.name
        ),
    };
    text.push_str(&format!("Produkt: {}\nGitHub-Repository: {}\n", product.name, product.repo));
    if let Some(description) = &product.description {
        text.push_str(&format!("Beschreibung auf GitHub: {description}\n"));
    }
    text.push_str(&format!("Lizenz laut GitHub: {}\n", product.license.as_deref().unwrap_or("keine erkannt")));
    if product.private {
        text.push_str("Achtung: Das Repository ist privat; Besucher können GitHub-Links darauf nicht öffnen.\n");
    }
    match &product.local_path {
        Some(path) => text.push_str(&format!("Lokaler Klon (relativ zum Projekt): {path}. Lies daraus nur über git -C, auf dem Release-Tag.\n")),
        None => text.push_str("Kein lokaler Klon: Inhalte über gh release view und die GitHub-Seite des Repos ermitteln.\n"),
    }
    match &product.latest_tag {
        Some(tag) => text.push_str(&format!(
            "Neuestes Release: {} (Tag {tag}). Der Eintrag in site.json bekommt documented \"{}\".\n",
            tag.trim_start_matches('v'),
            tag.trim_start_matches('v')
        )),
        None => text.push_str("Es gibt noch kein veröffentlichtes Release: kein Versions-Badge, und das Produkt kommt nicht in die products-Liste von site.json.\n"),
    }
    if !instructions.trim().is_empty() {
        text.push_str(&format!("\nZusätzliche Anweisung:\n{}\n", instructions.trim()));
    }
    text.push_str(&closing(site, "welche Dateien angelegt oder geändert wurden"));
    text
}

/// `model` and `effort` already resolved (task → site → Claude Code default).
pub fn args(site: &Site, prompt: &str, model: Option<&str>, effort: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        prompt.into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--permission-mode".into(),
        "acceptEdits".into(),
    ];
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        args.extend(["--model".into(), model.to_owned()]);
    }
    if let Some(effort) = effort.filter(|e| !e.is_empty()) {
        args.extend(["--effort".into(), effort.to_owned()]);
    }
    if let Some(budget) = site.claude.max_budget_usd {
        args.extend(["--max-budget-usd".into(), budget.to_string()]);
    }
    args.push("--allowedTools".into());
    args.extend(DEFAULT_ALLOWED.iter().map(|s| s.to_string()));
    args.push(format!("Bash({})", site.build.trim()));
    args.extend(site.claude.allowed_tools.iter().cloned());
    args.push("--disallowedTools".into());
    args.extend(DEFAULT_DISALLOWED.iter().map(|s| s.to_string()));
    args.extend(site.claude.disallowed_tools.iter().cloned());
    args
}

/// What one stream-json line means for the log.
pub enum Event {
    Line(String),
    Done(Outcome),
    Ignore,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub success: bool,
    /// Claude's final answer (the summary the prompt asks for).
    pub text: String,
    pub cost_usd: Option<f64>,
    pub turns: Option<u64>,
    /// Tool calls the allowlist blocked, e.g. `Bash: npm install`.
    pub denials: Vec<String>,
}

fn shorten(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

fn describe_tool(name: &str, input: &Value) -> String {
    let field = |key: &str| input[key].as_str().unwrap_or_default().to_owned();
    match name {
        "Bash" => format!("$ {}", shorten(&field("command"), 200)),
        "Read" | "Edit" | "MultiEdit" | "Write" => format!("{name} {}", field("file_path")),
        "Grep" => format!("Grep {}", shorten(&field("pattern"), 80)),
        "Glob" => format!("Glob {}", field("pattern")),
        "Skill" => format!("Skill {}", field("skill")),
        "TodoWrite" => "Aufgabenliste aktualisiert".into(),
        _ => format!("{name} {}", shorten(&input.to_string(), 120)),
    }
}

pub fn parse(line: &str) -> Event {
    let Ok(json) = serde_json::from_str::<Value>(line) else {
        return if line.trim().is_empty() { Event::Ignore } else { Event::Line(line.to_owned()) };
    };
    match json["type"].as_str() {
        Some("system") if json["subtype"] == "init" => {
            Event::Line(format!("Claude Code gestartet (Modell {})", json["model"].as_str().unwrap_or("?")))
        }
        Some("assistant") => {
            let lines: Vec<String> = json["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|block| match block["type"].as_str() {
                    Some("text") => Some(format!("💬 {}", shorten(block["text"].as_str().unwrap_or_default(), 400))),
                    Some("tool_use") => Some(format!("→ {}", describe_tool(block["name"].as_str().unwrap_or("?"), &block["input"]))),
                    _ => None,
                })
                .collect();
            if lines.is_empty() {
                Event::Ignore
            } else {
                Event::Line(lines.join("\n"))
            }
        }
        Some("user") => {
            let errors: Vec<String> = json["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "tool_result" && block["is_error"] == true)
                .map(|block| {
                    let content = match &block["content"] {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("  ✗ {}", shorten(&content, 200))
                })
                .collect();
            if errors.is_empty() {
                Event::Ignore
            } else {
                Event::Line(errors.join("\n"))
            }
        }
        Some("result") => Event::Done(Outcome {
            success: json["subtype"] == "success" && json["is_error"] != true,
            text: json["result"].as_str().unwrap_or_default().trim().to_owned(),
            cost_usd: json["total_cost_usd"].as_f64(),
            turns: json["num_turns"].as_u64(),
            denials: json["permission_denials"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|d| describe_tool(d["tool_name"].as_str().unwrap_or("?"), &d["tool_input"]))
                .collect(),
        }),
        _ => Event::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_use_and_result() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"npm run build"}}]}}"#;
        assert!(matches!(parse(line), Event::Line(l) if l == "→ $ npm run build"));
        let done = r#"{"type":"result","subtype":"success","result":"Fertig","total_cost_usd":0.5,"num_turns":3,"permission_denials":[{"tool_name":"Bash","tool_input":{"command":"git push"}}]}"#;
        match parse(done) {
            Event::Done(o) => {
                assert!(o.success);
                assert_eq!(o.text, "Fertig");
                assert_eq!(o.denials, vec!["$ git push"]);
            }
            _ => panic!("expected result"),
        }
    }

    #[test]
    fn task_prompt_names_skipped_products() {
        let site: Site = serde_json::from_str(
            r#"{"name":"example.com","url":"https://example.com","build":"npm run build","output":"dist",
                "products":[{"id":"a","name":"Alpha","repo":"o/a","documented":"1.0.0"},{"id":"b","name":"Beta","repo":"o/b","documented":"2.0.0"}]}"#,
        )
        .unwrap();
        let task = site.update_tasks().remove(0);
        let all = task_prompt(&site, &task, &[], &[], "");
        assert!(!all.contains("Nicht ausgewählt"));
        let some = task_prompt(&site, &task, &[], &["Beta".to_owned()], "");
        assert!(some.contains("Ausgewählte Produkte: Alpha. Nicht ausgewählt, daran nichts ändern: Beta."));
    }
}
