//! Runs Claude Code headless (`claude -p`) in the project and turns its stream-json output
//! into readable log lines.
//!
//! Claude may edit files in the project, read other repositories through git, look up releases
//! and run the build. Everything else is denied without a prompt, because `-p` cannot ask.
//! Committing, pushing and uploading are the app's job, never Claude's.

use crate::site::{Product, Site};
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
    "Bash(rm -rf*)",
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

pub fn prompt(site: &Site, outdated: &[Outdated], instructions: &str) -> String {
    let mut text = String::new();
    match &site.skill {
        Some(skill) => text.push_str(&format!("Nutze den Skill {skill}, um die Website {} zu aktualisieren.\n\n", site.name)),
        None => text.push_str(&format!("Aktualisiere die Website {} in diesem Repository.\n\n", site.name)),
    }
    if outdated.is_empty() {
        text.push_str("Kein Produkt hat ein neues Release.\n");
    } else {
        text.push_str("Veraltete Produkte (dokumentierter Stand → neuestes Release):\n");
        for o in outdated {
            text.push_str(&format!("- {} ({}): {} → {} (Tag {})\n", o.name, o.repo, o.documented, o.latest_version(), o.latest_tag));
        }
    }
    if !instructions.trim().is_empty() {
        text.push_str(&format!("\nZusätzliche Anweisung:\n{}\n", instructions.trim()));
    }
    text.push_str(&format!(
        "\nDer Build ({}) muss am Ende fehlerfrei laufen. Committe und pushe nicht; das übernimmt die App. \
         Beende deine Antwort mit einer kurzen Zusammenfassung auf Deutsch, eine Zeile pro Produkt, danach offene Punkte.",
        site.build
    ));
    text
}

pub fn args(site: &Site, prompt: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        prompt.into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--permission-mode".into(),
        "acceptEdits".into(),
    ];
    if let Some(model) = &site.claude.model {
        args.extend(["--model".into(), model.clone()]);
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
}
