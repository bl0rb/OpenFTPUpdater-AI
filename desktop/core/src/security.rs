//! The security check of a managed repository: how the default branch is protected (direct
//! pushes, force pushes, deletion, required checks), secret and dependency scanning with their open
//! alerts, the rights of the workflow token, how vulnerabilities can be reported, deploy keys.
//!
//! Reading needs no Claude and writes nothing. Many settings are only visible to admins; what
//! cannot be read is shown as "not checkable", never as fine. A finding may come with a fix: a
//! single, named change of a GitHub setting that `apply` carries out only after the user confirmed
//! exactly that change. Fixes only add protection; they never loosen or remove anything.

use crate::github::{encode_ref, get, message, probe, send, Forbidden};
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// The repository ruleset the fixes for the default branch create or extend.
pub const RULESET_NAME: &str = "OpenReleasePilot: Default-Branch";

/// In this order: the worst first. `Unknown` ranks below findings: not readable is not a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    High,
    Medium,
    Low,
    Unknown,
    Ok,
}

/// A change of one GitHub setting the app can make for a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Fix {
    BlockForcePush,
    RequirePullRequest,
    DependabotAlerts,
    DependabotUpdates,
    SecretScanning,
    PushProtection,
    CodeScanning,
    WorkflowReadOnly,
    PrivateReporting,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FixOffer {
    pub fix: Fix,
    pub label: String,
    /// What exactly changes on GitHub.
    pub effect: String,
    /// What the change can break.
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub id: &'static str,
    pub title: String,
    pub level: Level,
    pub detail: String,
    /// Only for an admin of a repository that is not archived.
    pub fix: Option<FixOffer>,
    /// Where on GitHub to look or change it by hand.
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// Unix seconds.
    pub checked_at: u64,
    pub default_branch: String,
    pub private: bool,
    pub archived: bool,
    /// `None` without a gh login: GitHub only reports permissions to a signed-in user.
    pub admin: Option<bool>,
    /// Worst first.
    pub checks: Vec<Check>,
}

impl Report {
    /// The worst level of all checks; `Ok` when nothing was found and everything was read.
    pub fn worst(&self) -> Level {
        self.checks.iter().map(|c| c.level).min().unwrap_or(Level::Unknown)
    }
}

/// One GitHub answer, sorted by what it means for a check.
enum Read {
    Found(Value),
    /// 404: switched off, not set up, or not visible to this login.
    Missing,
    /// 403 for another reason than the rate limit, with GitHub's reason.
    Denied(String),
    Failed(String),
}

fn read(path: &str) -> Read {
    match get(path) {
        Ok(Some(json)) => Read::Found(json),
        Ok(None) => Read::Missing,
        Err(e) => match e.downcast_ref::<Forbidden>() {
            Some(Forbidden(reason)) => Read::Denied(reason.clone()),
            None => Read::Failed(format!("{e:#}")),
        },
    }
}

/// How the default branch is protected, from rulesets and classic branch protection together.
/// Each `None` means "could not be read completely".
#[derive(Debug, Default)]
struct Protection {
    pull_request: Option<bool>,
    force_blocked: Option<bool>,
    deletion_blocked: Option<bool>,
    checks_required: Option<bool>,
    /// Classic protection requires pull requests but does not apply to admins.
    admins_exempt: bool,
    /// Where the protection comes from, e.g. `Ruleset „main“`.
    sources: Vec<String>,
    /// Why it could not be read completely.
    gaps: Vec<String>,
}

/// What classic branch protection enforces; all false when the branch has none.
#[derive(Debug, Default, Clone, Copy)]
struct Classic {
    pull_request: bool,
    force_blocked: bool,
    deletion_blocked: bool,
    checks_required: bool,
    admins_enforced: bool,
}

fn protection(repo: &str, branch: &str, admin: Option<bool>) -> Protection {
    let mut p = Protection::default();
    // Active rules of all rulesets that apply to the branch; readable with read access.
    let rules: Option<BTreeSet<String>> = match read(&format!("repos/{repo}/rules/branches/{}", encode_ref(branch))) {
        Read::Found(json) => {
            let list = json.as_array().cloned().unwrap_or_default();
            let mut names = BTreeSet::new();
            for rule in &list {
                if let Some(source) = rule["ruleset_id"].as_u64() {
                    names.insert(format!("Ruleset {source}"));
                }
            }
            if !list.is_empty() {
                p.sources.push(if names.len() == 1 { "Ruleset".to_owned() } else { format!("{} Rulesets", names.len()) });
            }
            Some(list.iter().filter_map(|r| r["type"].as_str().map(str::to_owned)).collect())
        }
        Read::Missing => Some(BTreeSet::new()),
        Read::Denied(why) | Read::Failed(why) => {
            p.gaps.push(format!("Rulesets nicht lesbar: {why}"));
            None
        }
    };
    let classic: Option<Classic> = match read(&format!("repos/{repo}/branches/{}", encode_ref(branch))) {
        Read::Found(json) if json["protected"].as_bool() == Some(false) => Some(Classic::default()),
        Read::Found(json) => {
            // The branch reports required checks to everyone, the rest only an admin can read.
            let summary = &json["protection"]["required_status_checks"];
            let checks_visible = summary["contexts"].as_array().is_some_and(|c| !c.is_empty()) || summary["checks"].as_array().is_some_and(|c| !c.is_empty());
            if admin == Some(true) {
                match read(&format!("repos/{repo}/branches/{}/protection", encode_ref(branch))) {
                    Read::Found(rules) => Some(Classic::from_json(&rules)),
                    // Protected by rulesets only.
                    Read::Missing => Some(Classic::default()),
                    Read::Denied(why) | Read::Failed(why) => {
                        p.gaps.push(format!("Branch-Schutz nicht lesbar: {why}"));
                        None
                    }
                }
            } else {
                if checks_visible {
                    p.checks_required = Some(true);
                }
                p.gaps.push("Einzelheiten des klassischen Branch-Schutzes sieht nur ein Admin".to_owned());
                None
            }
        }
        Read::Missing => {
            p.gaps.push(format!("Branch {branch} nicht gefunden"));
            None
        }
        Read::Denied(why) | Read::Failed(why) => {
            p.gaps.push(format!("Branch nicht lesbar: {why}"));
            None
        }
    };
    if classic.is_some_and(|c| c.pull_request || c.force_blocked || c.deletion_blocked || c.checks_required) {
        p.sources.push("klassischer Branch-Schutz".to_owned());
    }
    let has = |types: &[&str]| rules.as_ref().map(|r| types.iter().any(|t| r.contains(*t)));
    let combine = |rule: Option<bool>, classic: Option<bool>, known: Option<bool>| match (rule, classic) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        _ if known == Some(true) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    };
    // `update` (restrict updates) refuses every push, so it protects at least as much as a PR rule.
    p.pull_request = combine(has(&["pull_request", "update"]), classic.map(|c| c.pull_request), None);
    p.force_blocked = combine(has(&["non_fast_forward", "update"]), classic.map(|c| c.force_blocked), None);
    p.deletion_blocked = combine(has(&["deletion"]), classic.map(|c| c.deletion_blocked), None);
    p.checks_required = combine(has(&["required_status_checks"]), classic.map(|c| c.checks_required), p.checks_required);
    // Admins pass classic protection without "include administrators", unless a ruleset holds them.
    p.admins_exempt = classic.is_some_and(|c| c.pull_request && !c.admins_enforced) && has(&["pull_request", "update"]) == Some(false);
    p
}

impl Classic {
    fn from_json(json: &Value) -> Self {
        let on = |key: &str| json[key]["enabled"].as_bool() == Some(true);
        Self {
            pull_request: json["required_pull_request_reviews"].is_object() || on("lock_branch"),
            force_blocked: !on("allow_force_pushes"),
            deletion_blocked: !on("allow_deletions"),
            checks_required: json["required_status_checks"].is_object(),
            admins_enforced: on("enforce_admins"),
        }
    }
}

/// Whether to offer fixes, and why not.
struct Scope<'a> {
    repo: &'a str,
    branch: &'a str,
    private: bool,
    website: bool,
    /// Fixes are offered: admin, not archived.
    can_fix: bool,
    why_not: Option<&'static str>,
}

impl Scope<'_> {
    fn offer(&self, fix: Fix, label: &str, effect: String, warning: Option<String>) -> Option<FixOffer> {
        self.can_fix.then(|| FixOffer { fix, label: label.to_owned(), effect, warning })
    }

    fn url(&self, path: &str) -> Option<String> {
        Some(format!("https://github.com/{}/{path}", self.repo))
    }

    /// The detail, with why there is no fix if a finding could have one.
    fn detail(&self, text: String, fixable: bool) -> String {
        match (fixable, self.why_not) {
            (true, Some(why)) => format!("{text} Beheben in der App: {why}."),
            _ => text,
        }
    }

    fn plan_note(&self) -> Option<String> {
        self.private.then(|| "Bei privaten Repositories gibt es Rulesets nur mit GitHub Pro, Team oder Enterprise; sonst lehnt GitHub ab.".to_owned())
    }
}

fn check(id: &'static str, title: impl Into<String>, level: Level, detail: impl Into<String>) -> Check {
    Check { id, title: title.into(), level, detail: detail.into(), fix: None, url: None }
}

/// Reads the security state of `repo`. Errors only when the repository itself cannot be read;
/// every other part that cannot be read becomes a check with level `Unknown`.
/// `website`: it is the website's own repository, whose upload pushes to it.
pub fn check_repo(repo: &str, website: bool, now: u64) -> Result<Report> {
    let info = match read(&format!("repos/{repo}")) {
        Read::Found(info) => info,
        Read::Missing => bail!("GitHub meldet 404: gelöscht, umbenannt oder privat ohne Zugriff"),
        Read::Denied(why) => bail!("Zugriff verweigert: {why}"),
        Read::Failed(why) => bail!("{why}"),
    };
    let branch = info["default_branch"].as_str().filter(|b| !b.is_empty()).context("Antwort ohne default_branch")?.to_owned();
    let private = info["private"].as_bool().unwrap_or(false);
    let archived = info["archived"].as_bool().unwrap_or(false);
    let signed_in = info["permissions"].is_object();
    let admin = info["permissions"]["admin"].as_bool();
    let why_not = if archived {
        Some("archiviert, Einstellungen sind gesperrt")
    } else if !signed_in {
        Some("nur mit gh-Anmeldung")
    } else if admin != Some(true) {
        Some("nur mit Admin-Rechten")
    } else {
        None
    };
    let ctx = Scope { repo, branch: &branch, private, website, can_fix: why_not.is_none(), why_not };
    let mut checks = Vec::new();
    branch_checks(&ctx, &protection(repo, &branch, admin), &mut checks);
    secret_checks(&ctx, &info, signed_in, &mut checks);
    dependency_checks(&ctx, &info, signed_in, &mut checks);
    code_scanning_check(&ctx, signed_in, &mut checks);
    if admin == Some(true) {
        workflow_check(&ctx, &mut checks);
        deploy_key_check(&ctx, &mut checks);
    } else {
        let why = if signed_in { "nur mit Admin-Rechten lesbar" } else { "nur mit gh-Anmeldung und Admin-Rechten lesbar" };
        checks.push(check("workflowToken", "Rechte des Workflow-Tokens", Level::Unknown, why));
    }
    if !private {
        reporting_checks(&ctx, &mut checks);
    }
    checks.sort_by_key(|c| c.level);
    Ok(Report { checked_at: now, default_branch: branch, private, archived, admin, checks })
}

fn branch_checks(ctx: &Scope, p: &Protection, checks: &mut Vec<Check>) {
    let branch = ctx.branch;
    let gaps = if p.gaps.is_empty() { String::new() } else { p.gaps.join("; ") };
    let from = if p.sources.is_empty() { String::new() } else { format!(" ({})", p.sources.join(", ")) };
    let settings = ctx.url("settings/rules");
    let mut push_warning = format!("Danach lehnt GitHub direkte Pushes auf {branch} ab, auch deine eigenen; Änderungen kommen nur noch über Pull Requests.");
    if ctx.website {
        push_warning.push_str(" Das ist das Repository der Website: der Push beim Website-Upload dieser App scheitert dann.");
    }
    if let Some(note) = ctx.plan_note() {
        push_warning.push(' ');
        push_warning.push_str(&note);
    }
    let pr_offer = |label: &str| {
        ctx.offer(
            Fix::RequirePullRequest,
            label,
            format!(
                "Legt das Ruleset „{RULESET_NAME}“ an oder ergänzt es: Änderungen an {branch} nur über Pull Requests, ohne Pflicht-Freigabe (0 Reviews), \
                 damit du eigene PRs weiter mergen kannst. Gilt auch für Admins. Bestehende Regeln bleiben unverändert."
            ),
            Some(push_warning.clone()),
        )
    };
    let mut push = match (p.pull_request, p.admins_exempt) {
        (Some(true), true) => Check {
            fix: pr_offer("Auch für Admins erzwingen"),
            ..check("push", format!("Direkte Pushes auf {branch}"), Level::Low, ctx.detail(
                "Pull Requests sind Pflicht, aber der klassische Branch-Schutz gilt nicht für Admins: sie können weiter direkt pushen.".into(), true))
        },
        (Some(true), false) => check("push", format!("Direkte Pushes auf {branch}"), Level::Ok, format!("nur über Pull Requests{from}")),
        (Some(false), _) => Check {
            fix: pr_offer("Nur noch über Pull Requests"),
            ..check("push", format!("Direkte Pushes auf {branch}"), Level::High, ctx.detail(
                format!("Jeder mit Schreibzugriff kann ohne Pull Request, Review und CI direkt auf {branch} pushen."), true))
        },
        (None, _) => check("push", format!("Direkte Pushes auf {branch}"), Level::Unknown, format!("nicht vollständig lesbar: {gaps}")),
    };
    push.url = settings.clone();
    checks.push(push);

    let force_offer = ctx.offer(
        Fix::BlockForcePush,
        "Force-Push und Löschen verbieten",
        format!("Legt das Ruleset „{RULESET_NAME}“ an oder ergänzt es: Force-Pushes auf {branch} und das Löschen von {branch} sind verboten, auch für Admins. Normale Pushes und Merges bleiben möglich."),
        ctx.plan_note(),
    );
    let mut force = match (p.force_blocked, p.deletion_blocked) {
        (Some(true), Some(true)) => check("force", format!("Force-Push und Löschen von {branch}"), Level::Ok, format!("beides verboten{from}")),
        (Some(false), deletion) => Check {
            fix: force_offer,
            ..check("force", format!("Force-Push und Löschen von {branch}"), Level::High, ctx.detail(format!(
                "Ein Force-Push kann die Historie von {branch} überschreiben{}.",
                match deletion { Some(false) => ", und der Branch kann gelöscht werden", _ => "" }), true))
        },
        (Some(true), Some(false)) => Check {
            fix: force_offer,
            ..check("force", format!("Force-Push und Löschen von {branch}"), Level::Medium, ctx.detail(format!("Force-Pushes sind verboten, aber {branch} kann gelöscht werden."), true))
        },
        _ => check("force", format!("Force-Push und Löschen von {branch}"), Level::Unknown, format!("nicht vollständig lesbar: {gaps}")),
    };
    force.url = settings.clone();
    checks.push(force);

    let mut required = match p.checks_required {
        Some(true) => check("requiredChecks", "Pflicht-Checks vor dem Merge", Level::Ok, format!("CI-Checks müssen grün sein{from}")),
        Some(false) => check("requiredChecks", "Pflicht-Checks vor dem Merge", Level::Low,
            "Ein Pull Request lässt sich auch mit roter oder fehlender CI mergen (die App selbst mergt nur bei grüner CI). Welche Checks Pflicht sein sollen, legst du auf GitHub fest."),
        None => check("requiredChecks", "Pflicht-Checks vor dem Merge", Level::Unknown, format!("nicht vollständig lesbar: {gaps}")),
    };
    required.url = settings;
    checks.push(required);
}

fn secret_checks(ctx: &Scope, info: &Value, signed_in: bool, checks: &mut Vec<Check>) {
    let analysis = &info["security_and_analysis"];
    let status = |key: &str| analysis[key]["status"].as_str().map(|s| s == "enabled");
    let ghas = if ctx.private { Some("Bei privaten Repositories braucht GitHub dafür Secret Protection (GitHub Advanced Security); ohne lehnt GitHub ab.".to_owned()) } else { None };
    let unreadable = if signed_in { "nur mit Admin-Rechten lesbar" } else { "nur mit gh-Anmeldung und Admin-Rechten lesbar" };
    let scanning = status("secret_scanning");
    let mut c = match scanning {
        Some(true) => check("secretScanning", "Secret Scanning", Level::Ok, "an: eingecheckte Zugangsdaten werden erkannt"),
        Some(false) => Check {
            fix: ctx.offer(Fix::SecretScanning, "Secret Scanning einschalten", "Schaltet Secret Scanning ein: GitHub durchsucht Inhalt und Historie nach Zugangsdaten und meldet Funde als Alert.".into(), ghas.clone()),
            ..check("secretScanning", "Secret Scanning", Level::Medium, ctx.detail("Aus: eingecheckte Zugangsdaten (Tokens, Schlüssel) werden nicht erkannt.".into(), true))
        },
        None => check("secretScanning", "Secret Scanning", Level::Unknown, unreadable),
    };
    c.url = ctx.url("settings/security_analysis");
    checks.push(c);
    let mut c = match status("secret_scanning_push_protection") {
        Some(true) => check("pushProtection", "Push Protection für Secrets", Level::Ok, "an: Pushes mit erkannten Zugangsdaten werden abgelehnt"),
        Some(false) => Check {
            fix: ctx.offer(
                Fix::PushProtection,
                "Push Protection einschalten",
                format!("Schaltet Push Protection ein{}: GitHub lehnt Pushes ab, in denen es Zugangsdaten erkennt; umgehen geht nur mit Begründung.", if scanning == Some(true) { "" } else { " (und Secret Scanning, das sie braucht)" }),
                ghas,
            ),
            ..check("pushProtection", "Push Protection für Secrets", Level::Medium, ctx.detail("Aus: Pushes mit Zugangsdaten gehen durch und landen in der Historie.".into(), true))
        },
        None => check("pushProtection", "Push Protection für Secrets", Level::Unknown, unreadable),
    };
    c.url = ctx.url("settings/security_analysis");
    checks.push(c);
    if scanning == Some(false) || !signed_in {
        return;
    }
    let mut c = match read(&format!("repos/{}/secret-scanning/alerts?state=open&per_page=100", ctx.repo)) {
        Read::Found(json) => match json.as_array().map(Vec::len).unwrap_or(0) {
            0 => check("secretAlerts", "Offene Secret-Alerts", Level::Ok, "keine"),
            n => check("secretAlerts", "Offene Secret-Alerts", Level::High, format!(
                "{}{n} gefundene Zugangsdaten sind offen. Den Schlüssel beim Anbieter widerrufen und ersetzen; ihn nur aus dem Code zu löschen reicht nicht, die Historie behält ihn.",
                if n == 100 { "mindestens " } else { "" })),
        },
        Read::Missing => check("secretAlerts", "Offene Secret-Alerts", Level::Unknown, "Secret Scanning ist aus oder für dieses Login nicht sichtbar"),
        Read::Denied(why) | Read::Failed(why) => check("secretAlerts", "Offene Secret-Alerts", Level::Unknown, format!("nicht lesbar: {why}")),
    };
    c.url = ctx.url("security/secret-scanning");
    checks.push(c);
}

/// `critical: 1, high: 2` from alerts, with the level of the worst.
fn severities(alerts: &[Value], severity: impl Fn(&Value) -> Option<String>) -> (Level, String) {
    let order = ["critical", "high", "medium", "low"];
    let names = ["kritisch", "hoch", "mittel", "niedrig"];
    let mut counts = [0usize; 5];
    for alert in alerts {
        let s = severity(alert).unwrap_or_default();
        counts[order.iter().position(|o| *o == s).unwrap_or(4)] += 1;
    }
    let level = if counts[0] + counts[1] > 0 { Level::High } else if alerts.is_empty() { Level::Ok } else { Level::Medium };
    let text = counts
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(i, n)| format!("{n} {}", names.get(i).copied().unwrap_or("ohne Einstufung")))
        .collect::<Vec<_>>()
        .join(", ");
    (level, text)
}

fn dependency_checks(ctx: &Scope, info: &Value, signed_in: bool, checks: &mut Vec<Check>) {
    let repo = ctx.repo;
    // The open alerts tell whether alerts are on: GitHub refuses the list with "disabled" when they
    // are off. `vulnerability-alerts` would say it too, but answers 404 to a token without the
    // admin:repo_hook scope, so its 404 proves nothing.
    let list = if signed_in { Some(read(&format!("repos/{repo}/dependabot/alerts?state=open&per_page=100"))) } else { None };
    let enabled = match &list {
        Some(Read::Found(_)) => Some(true),
        Some(Read::Denied(why)) if why.to_lowercase().contains("disabled") => Some(false),
        _ => None,
    };
    let alerts_offer = || ctx.offer(Fix::DependabotAlerts, "Dependabot-Alerts einschalten", "Schaltet Dependabot-Alerts ein: GitHub meldet bekannte Sicherheitslücken in den Abhängigkeiten des Repositorys.".into(), None);
    let mut c = match enabled {
        Some(true) => check("dependabotAlerts", "Dependabot-Alerts", Level::Ok, "an: bekannte Lücken in Abhängigkeiten werden gemeldet"),
        Some(false) => Check {
            fix: alerts_offer(),
            ..check("dependabotAlerts", "Dependabot-Alerts", Level::Medium, ctx.detail("Aus: verwundbare Abhängigkeiten bleiben unbemerkt.".into(), true))
        },
        None => check("dependabotAlerts", "Dependabot-Alerts", Level::Unknown, if signed_in { "nicht lesbar" } else { "nur mit gh-Anmeldung lesbar" }),
    };
    c.url = ctx.url("settings/security_analysis");
    checks.push(c);
    if let Some(Read::Found(json)) = &list {
        let alerts = json.as_array().cloned().unwrap_or_default();
        let (level, text) = severities(&alerts, |a| {
            a["security_advisory"]["severity"].as_str().or(a["security_vulnerability"]["severity"].as_str()).map(str::to_owned)
        });
        let at_least = if alerts.len() == 100 { "mindestens " } else { "" };
        let mut c = match level {
            Level::Ok => check("dependabotOpen", "Offene Dependabot-Alerts", Level::Ok, "keine"),
            level => check("dependabotOpen", "Offene Dependabot-Alerts", level, format!(
                "{at_least}{} offen ({text}). Ein Pflegeauftrag übernimmt die Dependabot-PRs dazu; ohne PR hilft das Abhängigkeits-Update von Hand.",
                alerts.len())),
        };
        c.url = ctx.url("security/dependabot");
        checks.push(c);
    }
    let mut c = match info["security_and_analysis"]["dependabot_security_updates"]["status"].as_str() {
        Some("enabled") => check("dependabotUpdates", "Dependabot-Sicherheitsupdates", Level::Ok, "an: für gemeldete Lücken öffnet Dependabot Pull Requests"),
        Some(_) => Check {
            fix: ctx.offer(
                Fix::DependabotUpdates,
                "Sicherheitsupdates einschalten",
                format!("Schaltet Dependabot-Sicherheitsupdates ein{}: für gemeldete Lücken öffnet Dependabot Pull Requests, die ein Pflegeauftrag prüfen und mergen kann.", if enabled == Some(true) { "" } else { " (und die Dependabot-Alerts, die sie brauchen)" }),
                None,
            ),
            ..check("dependabotUpdates", "Dependabot-Sicherheitsupdates", Level::Low, ctx.detail("Aus: Lücken werden höchstens gemeldet, aber nicht per Pull Request behoben.".into(), true))
        },
        None => check("dependabotUpdates", "Dependabot-Sicherheitsupdates", Level::Unknown, if signed_in { "nur mit Admin-Rechten lesbar" } else { "nur mit gh-Anmeldung und Admin-Rechten lesbar" }),
    };
    c.url = ctx.url("settings/security_analysis");
    checks.push(c);
}

fn code_scanning_check(ctx: &Scope, signed_in: bool, checks: &mut Vec<Check>) {
    let title = "Code Scanning (CodeQL)";
    let offer = || {
        ctx.offer(
            Fix::CodeScanning,
            "CodeQL einschalten",
            format!("Schaltet Code Scanning im Default-Setup ein: CodeQL analysiert den Code bei jedem Push und Pull Request auf {} und wöchentlich, Funde erscheinen als Alert.", ctx.branch),
            Some(format!(
                "Läuft über GitHub Actions{}; nur für Sprachen, die CodeQL kennt.",
                if ctx.private { " und braucht bei privaten Repositories Code Security (GitHub Advanced Security), die Minuten zählen" } else { "" }
            )),
        )
    };
    let not_set_up = || Check {
        fix: offer(),
        ..check("codeScanning", title, Level::Low, ctx.detail("Nicht eingerichtet: Schwachstellen im eigenen Code (Injection, unsichere Workflows) werden nicht gesucht.".into(), true))
    };
    let mut c = if !signed_in {
        check("codeScanning", title, Level::Unknown, "nur mit gh-Anmeldung lesbar")
    } else {
        match read(&format!("repos/{}/code-scanning/alerts?state=open&per_page=100", ctx.repo)) {
            Read::Found(json) => {
                let alerts = json.as_array().cloned().unwrap_or_default();
                let (level, text) = severities(&alerts, |a| a["rule"]["security_severity_level"].as_str().map(str::to_owned));
                match level {
                    Level::Ok => check("codeScanning", title, Level::Ok, "eingerichtet, keine offenen Alerts"),
                    level => check("codeScanning", title, level, format!("{}{} offene Alerts ({text})", if alerts.len() == 100 { "mindestens " } else { "" }, alerts.len())),
                }
            }
            // 404: no analysis yet.
            Read::Missing => not_set_up(),
            Read::Denied(why) if why.to_lowercase().contains("not enabled") || why.to_lowercase().contains("no analysis") => not_set_up(),
            Read::Denied(why) | Read::Failed(why) => check("codeScanning", title, Level::Unknown, format!("nicht lesbar: {why}")),
        }
    };
    c.url = ctx.url("security/code-scanning");
    checks.push(c);
}

/// Workflows that declare no `permissions:` and so take the default rights of the token.
fn workflows_without_permissions(repo: &str) -> Result<Vec<String>> {
    let Some(list) = get(&format!("repos/{repo}/contents/.github/workflows"))? else { return Ok(Vec::new()) };
    let mut missing = Vec::new();
    for file in list.as_array().into_iter().flatten().filter(|f| f["type"] == "file").take(30) {
        let name = file["name"].as_str().unwrap_or_default();
        if !(name.ends_with(".yml") || name.ends_with(".yaml")) {
            continue;
        }
        let path = file["path"].as_str().unwrap_or_default();
        let json = get(&format!("repos/{repo}/contents/{}", encode_ref(path)))?.with_context(|| format!("{path} nicht gefunden"))?;
        let encoded: String = json["content"].as_str().context("Antwort ohne Inhalt")?.chars().filter(|c| !c.is_whitespace()).collect();
        let text = String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(encoded).context("Inhalt unlesbar")?).into_owned();
        if !declares_permissions(&text) {
            missing.push(name.to_owned());
        }
    }
    Ok(missing)
}

fn declares_permissions(workflow: &str) -> bool {
    workflow.lines().any(|line| line.trim_start().starts_with("permissions:"))
}

fn workflow_check(ctx: &Scope, checks: &mut Vec<Check>) {
    let title = "Rechte des Workflow-Tokens";
    let mut c = match read(&format!("repos/{}/actions/permissions/workflow", ctx.repo)) {
        Read::Found(json) => {
            let write = json["default_workflow_permissions"].as_str() == Some("write");
            let approve = json["can_approve_pull_request_reviews"].as_bool() == Some(true);
            if !write && !approve {
                check("workflowToken", title, Level::Ok, "GITHUB_TOKEN liest standardmäßig nur, Workflows dürfen keine PRs freigeben")
            } else {
                let problem = match (write, approve) {
                    (true, true) => "Der GITHUB_TOKEN hat in Workflows standardmäßig Schreibrechte, und Workflows dürfen Pull Requests freigeben.",
                    (true, false) => "Der GITHUB_TOKEN hat in Workflows standardmäßig Schreibrechte.",
                    _ => "Workflows dürfen Pull Requests freigeben.",
                };
                let warning = if write {
                    Some(match workflows_without_permissions(ctx.repo) {
                        Ok(names) if names.is_empty() => "Alle Workflows deklarieren eigene `permissions:`; sie behalten, was sie dort angeben.".to_owned(),
                        Ok(names) => format!(
                            "Diese Workflows deklarieren keine `permissions:` und schreiben danach nicht mehr (z. B. Release veröffentlichen, Tags, Kommentare): {}. Dort `permissions:` ergänzen.",
                            names.join(", ")
                        ),
                        Err(e) => format!("Workflows nicht geprüft ({e:#}); Workflows ohne `permissions:` verlieren ihre Schreibrechte."),
                    })
                } else {
                    None
                };
                Check {
                    fix: ctx.offer(
                        Fix::WorkflowReadOnly,
                        "Nur Lesen als Standard",
                        "Setzt die Standardrechte des GITHUB_TOKEN auf nur Lesen und verbietet Workflows, Pull Requests freizugeben. Was ein Workflow mehr braucht, gibt er mit `permissions:` an.".into(),
                        warning,
                    ),
                    ..check("workflowToken", title, Level::Medium, ctx.detail(format!("{problem} Ein kompromittierter Schritt (etwa eine fremde Action) kann so Code pushen oder Releases ändern."), true))
                }
            }
        }
        Read::Missing => check("workflowToken", title, Level::Unknown, "GitHub Actions ist aus oder die Einstellung nicht sichtbar"),
        Read::Denied(why) | Read::Failed(why) => check("workflowToken", title, Level::Unknown, format!("nicht lesbar: {why}")),
    };
    c.url = ctx.url("settings/actions");
    checks.push(c);
}

fn deploy_key_check(ctx: &Scope, checks: &mut Vec<Check>) {
    let title = "Deploy-Keys mit Schreibrecht";
    let mut c = match read(&format!("repos/{}/keys?per_page=100", ctx.repo)) {
        Read::Found(json) => {
            let writable: Vec<String> = json
                .as_array()
                .into_iter()
                .flatten()
                .filter(|k| k["read_only"].as_bool() == Some(false))
                .map(|k| format!("„{}“", k["title"].as_str().unwrap_or("ohne Namen")))
                .collect();
            match writable.len() {
                0 => check("deployKeys", title, Level::Ok, "keine"),
                n => check("deployKeys", title, Level::Low, format!(
                    "{n} Key{} mit Schreibrecht: {}. Wer den Schlüssel hat, kann am Branch-Schutz vorbei pushen, wo der nicht gilt; nicht mehr gebrauchte Keys auf GitHub entfernen.",
                    if n == 1 { "" } else { "s" }, writable.join(", "))),
            }
        }
        Read::Missing => check("deployKeys", title, Level::Unknown, "nicht sichtbar"),
        Read::Denied(why) | Read::Failed(why) => check("deployKeys", title, Level::Unknown, format!("nicht lesbar: {why}")),
    };
    c.url = ctx.url("settings/keys");
    checks.push(c);
}

/// Only for public repositories: there anyone may find and report a vulnerability.
fn reporting_checks(ctx: &Scope, checks: &mut Vec<Check>) {
    let repo = ctx.repo;
    let title = "Private Meldung von Sicherheitslücken";
    let mut c = match read(&format!("repos/{repo}/private-vulnerability-reporting")) {
        Read::Found(json) => match json["enabled"].as_bool() {
            Some(true) => check("privateReporting", title, Level::Ok, "an: Lücken können vertraulich gemeldet werden"),
            Some(false) => Check {
                fix: ctx.offer(
                    Fix::PrivateReporting,
                    "Private Meldung einschalten",
                    "Schaltet „Private vulnerability reporting“ ein: wer eine Lücke findet, meldet sie vertraulich als Entwurf eines Security Advisory, statt sie in einem öffentlichen Issue zu nennen.".into(),
                    None,
                ),
                ..check("privateReporting", title, Level::Low, ctx.detail("Aus: wer eine Lücke findet, kann sie nur öffentlich melden.".into(), true))
            },
            None => check("privateReporting", title, Level::Unknown, "Antwort ohne enabled"),
        },
        Read::Missing => check("privateReporting", title, Level::Unknown, "nicht sichtbar"),
        Read::Denied(why) | Read::Failed(why) => check("privateReporting", title, Level::Unknown, format!("nicht lesbar: {why}")),
    };
    c.url = ctx.url("settings/security_analysis");
    checks.push(c);
    let mut found = None;
    for path in ["SECURITY.md", ".github/SECURITY.md", "docs/SECURITY.md"] {
        match probe(&format!("repos/{repo}/contents/{path}")) {
            Ok(200 | 204) => {
                found = Some(Ok(path));
                break;
            }
            Ok(404) => {}
            Ok(code) => found = Some(Err(format!("HTTP {code}"))),
            Err(e) => found = Some(Err(format!("{e:#}"))),
        }
    }
    let mut c = match found {
        Some(Ok(path)) => check("securityPolicy", "Sicherheitsrichtlinie", Level::Ok, path),
        Some(Err(why)) => check("securityPolicy", "Sicherheitsrichtlinie", Level::Unknown, format!("nicht lesbar: {why}")),
        None => check("securityPolicy", "Sicherheitsrichtlinie", Level::Low,
            "Keine SECURITY.md: niemand weiß, wie und wo eine Lücke gemeldet werden soll. Anlegen geht per Pull Request, etwa mit dem Hinweis auf die private Meldung."),
    };
    c.url = ctx.url("security/policy");
    checks.push(c);
}

// MARK: Fixes

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FixResult {
    pub fix: Fix,
    pub ok: bool,
    pub text: String,
}

/// Carries out the confirmed fixes in a fixed order (alerts before the updates that need them),
/// each on its own: one GitHub refuses does not stop the others. Checks first that the login is
/// still admin of a repository that is not archived.
pub fn apply(repo: &str, fixes: &[Fix]) -> Result<Vec<FixResult>> {
    let info = get(&format!("repos/{repo}"))?.with_context(|| format!("{repo} nicht gefunden"))?;
    if info["archived"].as_bool() == Some(true) {
        bail!("{repo} ist archiviert; Einstellungen sind gesperrt");
    }
    if info["permissions"]["admin"].as_bool() != Some(true) {
        bail!("Für {repo} fehlen Admin-Rechte (oder die gh-Anmeldung)");
    }
    let mut order: Vec<Fix> = fixes.to_vec();
    order.sort();
    order.dedup();
    Ok(order
        .into_iter()
        .map(|fix| match apply_one(repo, fix) {
            Ok(text) => FixResult { fix, ok: true, text },
            Err(e) => FixResult { fix, ok: false, text: format!("{e:#}") },
        })
        .collect())
}

fn apply_one(repo: &str, fix: Fix) -> Result<String> {
    match fix {
        Fix::BlockForcePush => upsert_ruleset(repo, &[json!({ "type": "non_fast_forward" }), json!({ "type": "deletion" })]),
        Fix::RequirePullRequest => upsert_ruleset(
            repo,
            &[json!({ "type": "pull_request", "parameters": {
                "required_approving_review_count": 0,
                "dismiss_stale_reviews_on_push": false,
                "require_code_owner_review": false,
                "require_last_push_approval": false,
                "required_review_thread_resolution": false,
            } })],
        ),
        Fix::DependabotAlerts => {
            dependabot(send("PUT", &format!("repos/{repo}/vulnerability-alerts"), Some(&json!({})))?, "Dependabot-Alerts")?;
            Ok("Dependabot-Alerts eingeschaltet".into())
        }
        Fix::DependabotUpdates => {
            dependabot(send("PUT", &format!("repos/{repo}/vulnerability-alerts"), Some(&json!({})))?, "Dependabot-Alerts")?;
            dependabot(send("PUT", &format!("repos/{repo}/automated-security-fixes"), Some(&json!({})))?, "Dependabot-Sicherheitsupdates")?;
            Ok("Dependabot-Alerts und -Sicherheitsupdates eingeschaltet".into())
        }
        Fix::SecretScanning => security_setting(repo, &["secret_scanning"], "Secret Scanning eingeschaltet"),
        Fix::PushProtection => security_setting(repo, &["secret_scanning", "secret_scanning_push_protection"], "Secret Scanning und Push Protection eingeschaltet"),
        Fix::CodeScanning => {
            expect(send("PATCH", &format!("repos/{repo}/code-scanning/default-setup"), Some(&json!({ "state": "configured" })))?, &[200, 202], "Code Scanning")?;
            Ok("CodeQL-Default-Setup eingeschaltet; die erste Analyse läuft jetzt auf GitHub".into())
        }
        Fix::WorkflowReadOnly => {
            let body = json!({ "default_workflow_permissions": "read", "can_approve_pull_request_reviews": false });
            expect(send("PUT", &format!("repos/{repo}/actions/permissions/workflow"), Some(&body))?, &[200, 204], "Workflow-Rechte")?;
            Ok("GITHUB_TOKEN liest jetzt standardmäßig nur; Workflows dürfen keine PRs mehr freigeben".into())
        }
        Fix::PrivateReporting => {
            expect(send("PUT", &format!("repos/{repo}/private-vulnerability-reporting"), Some(&json!({})))?, &[200, 204], "Private Meldung")?;
            Ok("Private Meldung von Sicherheitslücken eingeschaltet".into())
        }
    }
}

/// GitHub hides these endpoints with 404 from a token without the admin:repo_hook scope.
fn dependabot(answer: (u16, Value), what: &str) -> Result<Value> {
    let status = answer.0;
    expect(answer, &[200, 204], what).map_err(|e| match status {
        403 | 404 => anyhow::anyhow!("{e:#}. Der gh-Anmeldung fehlt dafür vermutlich der Scope admin:repo_hook: gh auth refresh -h github.com -s admin:repo_hook"),
        _ => e,
    })
}

fn expect((status, body): (u16, Value), ok: &[u16], what: &str) -> Result<Value> {
    if !ok.contains(&status) {
        bail!("{what}: GitHub lehnt ab (HTTP {status}): {}", message(&body));
    }
    Ok(body)
}

/// Turns on settings under `security_and_analysis` and checks that GitHub took them: for a private
/// repository without the licence GitHub may answer 200 and leave them off.
fn security_setting(repo: &str, keys: &[&str], done: &str) -> Result<String> {
    let settings: serde_json::Map<String, Value> = keys.iter().map(|k| ((*k).to_owned(), json!({ "status": "enabled" }))).collect();
    let body = expect(send("PATCH", &format!("repos/{repo}"), Some(&json!({ "security_and_analysis": settings })))?, &[200], "Einstellung")?;
    let off: Vec<&str> = keys.iter().copied().filter(|k| body["security_and_analysis"][*k]["status"].as_str() != Some("enabled")).collect();
    if !off.is_empty() {
        bail!("GitHub hat {} nicht eingeschaltet (bei privaten Repositories braucht es GitHub Advanced Security)", off.join(", "));
    }
    Ok(done.to_owned())
}

/// Creates the app's ruleset for the default branch, or adds the missing rules to it. A rule of
/// the same type that is already there stays as it is, so a stricter setting is never loosened.
fn upsert_ruleset(repo: &str, add: &[Value]) -> Result<String> {
    let list = get(&format!("repos/{repo}/rulesets?includes_parents=false&per_page=100"))?.unwrap_or_default();
    let existing = list
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["name"].as_str() == Some(RULESET_NAME) && r["source_type"].as_str().is_none_or(|s| s == "Repository"))
        .and_then(|r| r["id"].as_u64());
    let names = |rules: &[Value]| rules.iter().filter_map(|r| r["type"].as_str()).map(rule_name).collect::<Vec<_>>().join(", ");
    let Some(id) = existing else {
        let body = json!({
            "name": RULESET_NAME,
            "target": "branch",
            "enforcement": "active",
            "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
            "rules": add,
            "bypass_actors": [],
        });
        expect(send("POST", &format!("repos/{repo}/rulesets"), Some(&body))?, &[201], "Ruleset anlegen")?;
        return Ok(format!("Ruleset „{RULESET_NAME}“ angelegt: {}", names(add)));
    };
    let current = get(&format!("repos/{repo}/rulesets/{id}"))?.context("Das Ruleset gibt es nicht mehr")?;
    let mut rules = current["rules"].as_array().cloned().unwrap_or_default();
    let missing: Vec<Value> = add.iter().filter(|rule| !rules.iter().any(|r| r["type"] == rule["type"])).cloned().collect();
    let active = current["enforcement"].as_str() == Some("active");
    if missing.is_empty() && active {
        return Ok(format!("Ruleset „{RULESET_NAME}“ enthielt das schon"));
    }
    rules.extend(missing.iter().cloned());
    let mut body = json!({ "name": RULESET_NAME, "target": "branch", "enforcement": "active", "rules": rules });
    for key in ["conditions", "bypass_actors"] {
        if !current[key].is_null() {
            body[key] = current[key].clone();
        }
    }
    expect(send("PUT", &format!("repos/{repo}/rulesets/{id}"), Some(&body))?, &[200], "Ruleset ergänzen")?;
    let mut text = format!("Ruleset „{RULESET_NAME}“ ergänzt");
    if !missing.is_empty() {
        text.push_str(&format!(": {}", names(&missing)));
    }
    if !active {
        text.push_str(" und wieder aktiv geschaltet");
    }
    Ok(text)
}

fn rule_name(kind: &str) -> &str {
    match kind {
        "pull_request" => "nur über Pull Requests",
        "non_fast_forward" => "kein Force-Push",
        "deletion" => "nicht löschbar",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::mock;

    fn repo_json(admin: bool) -> Value {
        json!({
            "full_name": "o/app", "default_branch": "main", "private": false, "archived": false,
            "permissions": { "admin": admin, "push": true },
            "security_and_analysis": if admin { json!({
                "secret_scanning": { "status": "enabled" },
                "secret_scanning_push_protection": { "status": "disabled" },
                "dependabot_security_updates": { "status": "enabled" },
            }) } else { Value::Null },
        })
    }

    fn base(admin: bool, rules: Value, protected: bool) -> Vec<(String, Option<Value>)> {
        vec![
            ("repos/o/app".into(), Some(repo_json(admin))),
            ("repos/o/app/rules/branches/main".into(), Some(rules)),
            ("repos/o/app/branches/main".into(), Some(json!({ "protected": protected }))),
            ("repos/o/app/secret-scanning/alerts?state=open&per_page=100".into(), Some(json!([]))),
            ("repos/o/app/dependabot/alerts?state=open&per_page=100".into(), Some(json!([
                { "security_advisory": { "severity": "high" } }, { "security_advisory": { "severity": "low" } },
            ]))),
            ("repos/o/app/code-scanning/alerts?state=open&per_page=100".into(), None),
            ("repos/o/app/actions/permissions/workflow".into(), Some(json!({ "default_workflow_permissions": "write", "can_approve_pull_request_reviews": false }))),
            ("repos/o/app/contents/.github/workflows".into(), Some(json!([
                { "type": "file", "name": "ci.yml", "path": ".github/workflows/ci.yml" },
                { "type": "file", "name": "release.yml", "path": ".github/workflows/release.yml" },
            ]))),
            ("repos/o/app/contents/.github/workflows/ci.yml".into(), Some(json!({ "content": base64::engine::general_purpose::STANDARD.encode("on: push\npermissions:\n  contents: read\n") }))),
            ("repos/o/app/contents/.github/workflows/release.yml".into(), Some(json!({ "content": base64::engine::general_purpose::STANDARD.encode("on: push\njobs: {}\n") }))),
            ("repos/o/app/keys?per_page=100".into(), Some(json!([{ "title": "deploy", "read_only": false }, { "title": "ro", "read_only": true }]))),
            ("repos/o/app/private-vulnerability-reporting".into(), Some(json!({ "enabled": false }))),
            ("repos/o/app/contents/SECURITY.md".into(), None),
            ("repos/o/app/contents/.github/SECURITY.md".into(), Some(json!({}))),
        ]
    }

    fn find<'a>(report: &'a Report, id: &str) -> &'a Check {
        report.checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("no check {id}: {:?}", report.checks.iter().map(|c| c.id).collect::<Vec<_>>()))
    }

    #[test]
    fn an_unprotected_default_branch_is_a_finding_with_fixes_and_reading_writes_nothing() {
        mock::install(base(true, json!([]), false));
        let report = check_repo("o/app", true, 1000).unwrap();
        let push = find(&report, "push");
        assert_eq!(push.level, Level::High);
        let offer = push.fix.as_ref().expect("admin gets a fix");
        assert_eq!(offer.fix, Fix::RequirePullRequest);
        assert!(offer.warning.as_deref().unwrap().contains("Website-Upload"), "the website's own push would fail");
        assert_eq!(find(&report, "force").fix.as_ref().map(|f| f.fix), Some(Fix::BlockForcePush));
        assert_eq!(find(&report, "requiredChecks").level, Level::Low);
        assert_eq!(find(&report, "pushProtection").level, Level::Medium);
        assert_eq!(find(&report, "dependabotOpen").level, Level::High, "one high alert");
        assert!(find(&report, "dependabotOpen").detail.contains("1 hoch, 1 niedrig"));
        assert_eq!(find(&report, "codeScanning").fix.as_ref().map(|f| f.fix), Some(Fix::CodeScanning), "404 = not set up");
        let token = find(&report, "workflowToken");
        assert_eq!(token.level, Level::Medium);
        let warning = token.fix.as_ref().unwrap().warning.as_deref().unwrap();
        assert!(warning.contains("release.yml") && !warning.contains("ci.yml"), "{warning}");
        assert!(find(&report, "deployKeys").detail.contains("„deploy“"));
        assert_eq!(find(&report, "securityPolicy").level, Level::Ok);
        assert_eq!(report.worst(), Level::High);
        assert_eq!(report.checks[0].level, Level::High, "worst first");
        assert!(mock::writes().is_empty());
    }

    #[test]
    fn rulesets_count_without_admin_rights_and_nothing_is_offered() {
        let rules = json!([
            { "type": "pull_request", "ruleset_id": 7 }, { "type": "non_fast_forward", "ruleset_id": 7 },
            { "type": "deletion", "ruleset_id": 7 }, { "type": "required_status_checks", "ruleset_id": 7 },
        ]);
        mock::install(base(false, rules, true));
        let report = check_repo("o/app", false, 1000).unwrap();
        for id in ["push", "force", "requiredChecks"] {
            assert_eq!(find(&report, id).level, Level::Ok, "{id}");
        }
        assert_eq!(find(&report, "secretScanning").level, Level::Unknown, "only an admin sees it");
        assert_eq!(find(&report, "workflowToken").level, Level::Unknown);
        assert!(report.checks.iter().all(|c| c.fix.is_none()), "no admin, no fix");
        assert!(find(&report, "privateReporting").detail.contains("nur mit Admin-Rechten"));
    }

    #[test]
    fn classic_protection_without_admins_leaves_a_hint() {
        let mut gets = base(true, json!([]), true);
        gets.push(("repos/o/app/branches/main/protection".into(), Some(json!({
            "required_pull_request_reviews": { "required_approving_review_count": 1 },
            "enforce_admins": { "enabled": false },
            "allow_force_pushes": { "enabled": false },
            "allow_deletions": { "enabled": true },
        }))));
        mock::install(gets);
        let report = check_repo("o/app", false, 1000).unwrap();
        assert_eq!(find(&report, "push").level, Level::Low, "admins can still push");
        assert_eq!(find(&report, "force").level, Level::Medium, "deletion allowed");
        assert_eq!(find(&report, "requiredChecks").level, Level::Low);
    }

    #[test]
    fn a_fix_creates_the_ruleset_or_adds_only_what_is_missing() {
        mock::install([
            ("repos/o/app".to_owned(), Some(repo_json(true))),
            ("repos/o/app/rulesets?includes_parents=false&per_page=100".to_owned(), Some(json!([]))),
        ]);
        let results = apply("o/app", &[Fix::RequirePullRequest, Fix::DependabotUpdates, Fix::BlockForcePush]).unwrap();
        assert!(results.iter().all(|r| r.ok), "{results:?}");
        assert_eq!(results.iter().map(|r| r.fix).collect::<Vec<_>>(), [Fix::BlockForcePush, Fix::RequirePullRequest, Fix::DependabotUpdates]);
        let writes = mock::writes();
        assert_eq!(writes.iter().filter(|w| *w == "POST repos/o/app/rulesets").count(), 2, "the mock list stays empty");
        assert!(writes.contains(&"PUT repos/o/app/vulnerability-alerts".to_owned()), "alerts before updates");

        mock::install([
            ("repos/o/app".to_owned(), Some(repo_json(true))),
            ("repos/o/app/rulesets?includes_parents=false&per_page=100".to_owned(), Some(json!([{ "id": 3, "name": RULESET_NAME, "source_type": "Repository" }]))),
            ("repos/o/app/rulesets/3".to_owned(), Some(json!({
                "enforcement": "active",
                "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
                "rules": [{ "type": "pull_request", "parameters": { "required_approving_review_count": 2 } }],
            }))),
        ]);
        let results = apply("o/app", &[Fix::RequirePullRequest, Fix::BlockForcePush]).unwrap();
        assert!(results.iter().all(|r| r.ok), "{results:?}");
        let body = mock::written("PUT repos/o/app/rulesets/3").expect("extended");
        let rules = body["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 3, "pull_request stays, two rules added: {rules:?}");
        assert_eq!(rules[0]["parameters"]["required_approving_review_count"], 2, "never loosened");
        assert!(results[1].text.contains("schon"), "{:?}", results[1]);
    }

    #[test]
    fn fixes_need_admin_rights() {
        mock::install([("repos/o/app".to_owned(), Some(repo_json(false)))]);
        assert!(apply("o/app", &[Fix::DependabotAlerts]).is_err());
        assert!(mock::writes().is_empty());
    }

    #[test]
    fn a_hidden_dependabot_endpoint_names_the_missing_scope() {
        mock::install([("repos/o/app".to_owned(), Some(repo_json(true)))]);
        mock::reply("PUT repos/o/app/vulnerability-alerts", 404, json!({ "message": "Not Found" }));
        let results = apply("o/app", &[Fix::DependabotAlerts]).unwrap();
        assert!(results[0].text.contains("admin:repo_hook"), "{results:?}");
    }

    #[test]
    fn refused_settings_are_reported_not_hidden() {
        mock::install([("repos/o/app".to_owned(), Some(repo_json(true)))]);
        mock::reply("PATCH repos/o/app", 200, json!({ "security_and_analysis": { "secret_scanning": { "status": "disabled" } } }));
        mock::reply("PUT repos/o/app/private-vulnerability-reporting", 422, json!({ "message": "nope" }));
        let results = apply("o/app", &[Fix::SecretScanning, Fix::PrivateReporting]).unwrap();
        assert!(!results[0].ok && results[0].text.contains("nicht eingeschaltet"), "{results:?}");
        assert!(!results[1].ok && results[1].text.contains("nope"), "{results:?}");
    }

    /// `cargo test -p openreleasepilot-core checks_a_real_repository -- --ignored --nocapture`:
    /// the security check of this app's own repository; reads only.
    #[test]
    #[ignore = "network"]
    fn checks_a_real_repository() {
        let report = check_repo(crate::update::REPOSITORY, false, 1000).unwrap();
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        for c in &report.checks {
            println!("{:?} {}: {}{}", c.level, c.title, c.detail, c.fix.as_ref().map(|f| format!(" [Fix: {}]", f.label)).unwrap_or_default());
        }
    }
}
