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

// MARK: Pull request review for "Repos pflegen"

/// Read-only: the product's clone, pull requests and releases through gh, the web.
pub const REVIEW_ALLOWED: &[&str] = &[
    "Skill",
    "Read",
    "Glob",
    "Grep",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
    "Bash(gh pr view *)",
    "Bash(gh pr diff *)",
    "Bash(gh pr checks *)",
    "Bash(gh pr list *)",
    "Bash(gh release view *)",
    "Bash(gh release list *)",
    "Bash(gh api repos/*/releases*)",
    "Bash(gh api repos/*/git/ref/tags/*)",
    "Bash(gh api repos/*/git/tags/*)",
    "Bash(gh api repos/*/compare/*)",
    "Bash(gh api repos/*/contents/*)",
    "Bash(git log*)",
    "Bash(git show*)",
    "Bash(git diff*)",
    "Bash(grep *)",
    "Bash(ls *)",
];

/// Claude only judges; merging, closing and commenting is the app's job.
pub const REVIEW_DISALLOWED: &[&str] = &[
    "Edit",
    "Write",
    "NotebookEdit",
    "Bash(gh pr merge*)",
    "Bash(gh pr close*)",
    "Bash(gh pr comment*)",
    "Bash(gh pr review*)",
    "Bash(gh pr edit*)",
    "Bash(gh pr checkout*)",
    "Bash(gh api * -X*)",
    "Bash(gh api * --method*)",
    "Bash(gh api * -f*)",
    "Bash(gh api * -F*)",
    "Bash(gh api * --field*)",
    "Bash(gh api * --raw-field*)",
    "Bash(gh api * --input*)",
    "Bash(git push*)",
    "Bash(git commit*)",
    "Bash(git checkout*)",
    "Bash(git reset*)",
    "Bash(rm *)",
];

/// One entry per pull request; see `repocare::Review`.
pub const REVIEW_SCHEMA: &str = r#"{"type":"object","properties":{"prs":{"type":"array","items":{"type":"object","properties":{"number":{"type":"integer"},"action":{"type":"string","enum":["merge","keep","close"]},"risk":{"type":"string","enum":["green","yellow","red"]},"bump":{"type":"string"},"reason":{"type":"string"}},"required":["number","action","risk","reason"]}}},"required":["prs"]}"#;

/// The rules of the dependabot-pr skill, so the review does not depend on it being installed.
const REVIEW_RULES: &str = "\
So entscheidest du (action):
- merge: nur wenn GitHub den PR als mergebar mit grünen Checks meldet und das Risiko grün ist, oder gelb und die \
Release-Notes nichts enthalten, was diesen Code betrifft.
- keep: alles Rote und alles Unklare. Lieber offen lassen, als die Software zu brechen.
- close: nur wenn der PR überflüssig ist: von einem anderen offenen PR überholt (gleiche Abhängigkeit, höhere \
Version), schon im Zielbranch enthalten oder gegenstandslos. Deine Begründung wird als Kommentar gepostet.
PRs, die GitHub nicht als mergebar meldet, kommen für merge nicht in Frage; prüfe bei ihnen nur, ob sie \
überflüssig sind, sonst keep. Wäre ein solcher Dependabot-PR nach einem Rebase mergbar, sag merge: die App \
stößt dann `@dependabot rebase` an.

Risiko (risk):
- green: Patch-Updates von Laufzeit-Abhängigkeiten; Patch/Minor von Entwicklungs-Abhängigkeiten (Tests, Typen, \
Linting, Tooling); GitHub Actions Patch/Minor mit geprüftem SHA-Pin; Docker-Basis-Patch.
- yellow: Minor-Updates von Laufzeit-Abhängigkeiten; Actions-Majors (meist nur Runner- oder Node-Anforderungen); \
Docker-Basis-Minor; Go-Stable. Release-Notes aus dem offiziellen Repository lesen (gh release view <tag> \
--repo <owner>/<repo>), nicht dem PR-Text vertrauen.
- red: Majors von Laufzeit-Abhängigkeiten, eslint-Majors, Docker-Basis-Majors (Node nur auf LTS-Majors) und \
alles, was eine der Prüfungen unten anschlägt. Unter 1.0 zählt die erste Stelle ungleich null: 0.10 → 0.11 ist \
ein Major, 0.3.1 → 0.3.2 ein Patch.

Prüfe außerdem:
- Dependabot-PRs dürfen nur Manifest-, Lock-, Workflow- und Dockerfiles ändern; sonst red.
- Actions mit SHA-Pin: gehört der neue SHA wirklich zum genannten Tag (gh api \
repos/<owner>/<action>/git/ref/tags/<tag>, bei type \"tag\" den Commit über git/tags/<sha>)? Sonst red, als \
Supply-Chain-Signal in der Begründung nennen.
- Zusammengehörige Abhängigkeiten nur gemeinsam mergen (react und @types/react, tailwindcss und \
@tailwindcss/vite, eslint und seine Configs, @types/node passend zur Node-Version). Ist einer nicht mergebar, \
beide keep.
- Bei Majors die Breaking Changes gegen die tatsächliche Nutzung im Code prüfen (grep) und den Befund mit \
Datei:Zeile in die Begründung schreiben; trotzdem keep.
- Sicherheits-Advisories (GHSA) im PR-Text in der Begründung nennen.
- PRs von Menschen oder anderen Bots (z. B. Copilot): den Diff vollständig lesen (gh pr diff). merge nur, wenn \
die Änderung abgeschlossen, nachvollziehbar, von der CI abgedeckt und klein genug ist, um sie ganz zu prüfen; \
risk dann höchstens yellow. Große, unfertige oder unklare Änderungen: keep.
Wenn ein Skill dependabot-pr verfügbar ist, gelten zusätzlich seine Bewertungsregeln und Repo-Besonderheiten; \
seine Schritte zum Mergen und Kommentieren führst du nicht aus.
Befehle einzeln ausführen, ohne cd, && oder Schleifen; zusammengesetzte Befehle werden ohne Rückfrage abgelehnt \
(Pipes zu jq oder head gehen).

Gib für jeden PR der Liste genau einen Eintrag zurück: reason ist ein Satz auf Deutsch, bump zum Beispiel \
„18.0.14 → 18.1.0“, wenn erkennbar.";

/// Asks Claude to judge the open pull requests of one repository.
pub fn review_prompt(plan: &crate::repocare::Plan) -> String {
    let mut text = format!(
        "Bewerte die offenen Pull Requests auf {} im Repository {}, bevor die App sie mergt. Du entscheidest nur; \
         mergen, schließen und kommentieren übernimmt die App. Ändere keine Dateien.\n",
        plan.default_branch, plan.repo
    );
    text.push_str(&match &plan.clone {
        Some(dir) => format!("Das aktuelle Verzeichnis ist der lokale Klon ({dir}); prüfe dort, wie der Code die Abhängigkeiten nutzt.\n\n"),
        None => format!("Es gibt keinen lokalen Klon; Code liest du bei Bedarf mit gh api repos/{}/contents/<pfad> oder gh pr diff.\n\n", plan.repo),
    });
    text.push_str(REVIEW_RULES);
    text.push_str("\n\nOffene Pull Requests:\n");
    for pr in &plan.prs {
        let gate = if pr.ready { "mergebar, Checks grün".to_owned() } else { pr.gate.clone() };
        let more = pr.files.len().saturating_sub(15);
        let files = pr.files.iter().take(15).cloned().collect::<Vec<_>>().join(", ");
        text.push_str(&format!(
            "- #{} „{}“ von {}; {}; GitHub: {}; Dateien: {}{}\n",
            pr.number,
            pr.title,
            pr.author,
            pr.size,
            gate,
            files,
            if more > 0 { format!(" … und {more} weitere") } else { String::new() }
        ));
    }
    text
}

/// Read-only review run with the answer as JSON (`Outcome::structured`).
pub fn review_args(prompt: &str, model: Option<&str>, effort: Option<&str>, max_budget_usd: Option<f64>) -> Vec<String> {
    let mut args: Vec<String> =
        vec!["-p".into(), prompt.into(), "--output-format".into(), "stream-json".into(), "--verbose".into(), "--json-schema".into(), REVIEW_SCHEMA.into()];
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        args.extend(["--model".into(), model.to_owned()]);
    }
    if let Some(effort) = effort.filter(|e| !e.is_empty()) {
        args.extend(["--effort".into(), effort.to_owned()]);
    }
    if let Some(budget) = max_budget_usd {
        args.extend(["--max-budget-usd".into(), budget.to_string()]);
    }
    args.push("--allowedTools".into());
    args.extend(REVIEW_ALLOWED.iter().map(|s| s.to_string()));
    args.push("--disallowedTools".into());
    args.extend(REVIEW_DISALLOWED.iter().map(|s| s.to_string()));
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
    /// The answer as JSON when the run had `--json-schema`.
    #[serde(skip)]
    pub structured: Option<Value>,
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
            structured: json.get("structured_output").filter(|v| !v.is_null()).cloned(),
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
