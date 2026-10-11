//! "Repos pflegen" for the selected products' repositories on GitHub: merge open pull requests that
//! GitHub reports mergeable with green checks, tag the result with the next patch version, delete
//! branches whose commits are all in the default branch (on GitHub and in the local clone).
//!
//! `plan` only looks. The run checks every item again before it acts, so a preview that has aged
//! cannot merge or delete something that changed since.

use crate::github::{encode_ref, get, get_pages, message, send};
use crate::localrepo;
use crate::shell::output;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::thread;
use std::time::Duration;

/// What the run will do in one repository, as shown in the preview.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub name: String,
    /// `owner/name`
    pub repo: String,
    pub default_branch: String,
    /// `merge`, `squash` or `rebase`, whichever the repository allows first.
    pub merge_method: String,
    pub tag_prefix: String,
    /// Open pull requests into the default branch that are ready, merged in this order.
    pub merge: Vec<PullRequest>,
    /// Open pull requests that stay open, with the reason.
    pub skip: Vec<PullRequest>,
    /// Newest release tag `x.y.z` before the run.
    pub last_tag: Option<String>,
    /// Next patch version, if the default branch moved since `last_tag` (or will by the merges).
    pub tag: Option<String>,
    /// Why there is no tag.
    pub tag_note: Option<String>,
    /// Branches on GitHub to delete.
    pub branches: Vec<Branch>,
    /// Absolute path of the local clone, if it exists and belongs to the repository.
    pub clone: Option<String>,
    pub local_branches: Vec<Branch>,
    /// Why the local clone was left out.
    pub clone_note: Option<String>,
    /// The repository could not be looked at; nothing happens there.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head_sha: String,
    /// Head branch, if it lives in this repository (not in a fork).
    pub head_branch: Option<String>,
    pub url: String,
    /// Why it is not merged (only in `skip`).
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Branch {
    pub name: String,
    /// Commit the plan saw; the branch is only deleted while it still points there.
    pub sha: String,
    /// e.g. `in main enthalten`, `PR #12 gemergt`.
    pub reason: String,
    /// Head of a pull request in `merge`: deleted only if that merge succeeds.
    #[serde(default)]
    pub after_merge: Option<u64>,
}

/// What happened with one item of the plan.
pub enum Done {
    Ok(String),
    Skipped(String),
}

/// Looks at one repository. Errors end up in `Plan::error`, so one unreachable repository does not
/// hide the others.
pub fn plan(name: &str, repo: &str, tag_prefix: &str, clone: Option<&Path>, merge: bool, cleanup: bool) -> Plan {
    let mut plan = Plan { name: name.to_owned(), repo: repo.to_owned(), tag_prefix: tag_prefix.to_owned(), ..Plan::default() };
    if let Err(e) = look(&mut plan, clone, merge, cleanup) {
        plan.merge.clear();
        plan.tag = None;
        plan.branches.clear();
        plan.local_branches.clear();
        plan.error = Some(format!("{e:#}"));
    }
    plan
}

fn look(plan: &mut Plan, clone: Option<&Path>, merge: bool, cleanup: bool) -> Result<()> {
    let repo = plan.repo.clone();
    let info = get(&format!("repos/{repo}"))?.context("Repository nicht gefunden oder kein Zugriff")?;
    if info["archived"].as_bool() == Some(true) {
        bail!("Repository ist archiviert");
    }
    if info["permissions"]["push"].as_bool() == Some(false) {
        bail!("kein Schreibzugriff");
    }
    let default = info["default_branch"].as_str().context("Antwort ohne default_branch")?.to_owned();
    plan.default_branch = default.clone();
    plan.merge_method = merge_method(&info).to_owned();

    let open = get_pages(&format!("repos/{repo}/pulls?state=open"), 3)?;
    if merge {
        for listed in open.iter().filter(|pr| pr["base"]["ref"].as_str() == Some(default.as_str())) {
            let mut item = pull_request(listed, &repo);
            match readiness(&repo, &pull(&repo, item.number)?)? {
                Ok(()) => plan.merge.push(item),
                Err(reason) => {
                    item.reason = reason;
                    plan.skip.push(item);
                }
            }
        }
        plan_tag(plan)?;
    }
    if cleanup {
        let merged = merged_heads(&repo)?;
        // Heads of the pull requests merged now; GitHub removes them itself when the repository says so.
        if info["delete_branch_on_merge"].as_bool() != Some(true) {
            for pr in &plan.merge {
                if let Some(branch) = pr.head_branch.as_ref().filter(|b| **b != default) {
                    plan.branches.push(Branch {
                        name: branch.clone(),
                        sha: pr.head_sha.clone(),
                        reason: format!("nach dem Merge von PR #{}", pr.number),
                        after_merge: Some(pr.number),
                    });
                }
            }
        }
        let open_heads: HashSet<&str> = open.iter().filter(|pr| same_repo(pr, &repo)).filter_map(|pr| pr["head"]["ref"].as_str()).collect();
        for branch in get_pages(&format!("repos/{repo}/branches"), 5)? {
            let (Some(name), Some(sha)) = (branch["name"].as_str(), branch["commit"]["sha"].as_str()) else { continue };
            if name == default || branch["protected"].as_bool() == Some(true) || open_heads.contains(name) {
                continue;
            }
            let reason = match merged.get(sha) {
                Some(number) => format!("PR #{number} gemergt"),
                None if contained(&repo, &default, name)? => format!("in {default} enthalten"),
                None => continue,
            };
            plan.branches.push(Branch { name: name.to_owned(), sha: sha.to_owned(), reason, after_merge: None });
        }
        if let Some(dir) = clone {
            if let Err(e) = look_local(plan, dir, &merged) {
                plan.local_branches.clear();
                plan.clone_note = Some(format!("{e:#}"));
            }
        }
    }
    Ok(())
}

/// Branches of the local clone that are merged; `git fetch --prune` first, so `origin/<default>` is current.
fn look_local(plan: &mut Plan, dir: &Path, merged: &HashMap<String, u64>) -> Result<()> {
    let origin = localrepo::origin_repo(dir).context("lokaler Klon ohne GitHub-origin")?;
    if !origin.eq_ignore_ascii_case(&plan.repo) {
        bail!("lokaler Klon gehört zu {origin}");
    }
    plan.clone = Some(dir.display().to_string());
    output("git", &["fetch", "--prune", "--quiet", "origin"], dir).context("git fetch im lokalen Klon")?;
    let base = format!("refs/remotes/origin/{}", plan.default_branch);
    output("git", &["rev-parse", "--verify", "--quiet", &base], dir).with_context(|| format!("origin/{} fehlt im lokalen Klon", plan.default_branch))?;
    let current = output("git", &["branch", "--show-current"], dir).unwrap_or_default();
    for line in output("git", &["for-each-ref", "--format=%(refname:short)%09%(objectname)", "refs/heads"], dir)?.lines() {
        let Some((name, sha)) = line.split_once('\t') else { continue };
        if name == plan.default_branch || name == current.trim() {
            continue;
        }
        let reason = match merged.get(sha) {
            Some(number) => format!("PR #{number} gemergt"),
            None if output("git", &["merge-base", "--is-ancestor", sha, &base], dir).is_ok() => format!("in {} enthalten", plan.default_branch),
            None => continue,
        };
        plan.local_branches.push(Branch { name: name.to_owned(), sha: sha.to_owned(), reason, after_merge: None });
    }
    Ok(())
}

fn plan_tag(plan: &mut Plan) -> Result<()> {
    let tags: Vec<String> =
        get_pages(&format!("repos/{}/tags", plan.repo), 3)?.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect();
    let Some(last) = latest_release_tag(&tags, &plan.tag_prefix) else {
        plan.tag_note = Some(format!("noch kein Tag im Format {}x.y.z; das erste Release von Hand taggen", plan.tag_prefix));
        return Ok(());
    };
    let next = next_patch(&last, &plan.tag_prefix).context("Tag-Version unlesbar")?;
    plan.last_tag = Some(last.clone());
    if tags.contains(&next) {
        plan.tag_note = Some(format!("{next} gibt es schon"));
    } else if plan.merge.is_empty() && commits_since(&plan.repo, &last, &plan.default_branch)? == 0 {
        plan.tag_note = Some(format!("{} unverändert seit {last}", plan.default_branch));
    } else {
        plan.tag = Some(next);
    }
    Ok(())
}

/// Merges a planned pull request if it is still open, unchanged and ready.
pub fn merge(plan: &Plan, pr: &PullRequest) -> Result<Done> {
    let current = pull(&plan.repo, pr.number)?;
    if current["head"]["sha"].as_str() != Some(pr.head_sha.as_str()) {
        return Ok(Done::Skipped("hat seit der Vorschau neue Commits".into()));
    }
    if let Err(reason) = readiness(&plan.repo, &current)? {
        return Ok(Done::Skipped(reason));
    }
    let body = json!({ "merge_method": plan.merge_method, "sha": pr.head_sha });
    match send("PUT", &format!("repos/{}/pulls/{}/merge", plan.repo, pr.number), Some(&body))? {
        (200, _) => Ok(Done::Ok(format!("gemergt ({})", plan.merge_method))),
        (_, body) => bail!("GitHub lehnt den Merge ab: {}", message(&body)),
    }
}

/// Sets the planned tag on the head of the default branch, if it moved since the last tag.
pub fn tag(plan: &Plan) -> Result<Done> {
    let Some(tag) = &plan.tag else { return Ok(Done::Skipped("kein Tag geplant".into())) };
    if version(tag, &plan.tag_prefix).is_none() {
        bail!("{tag} ist keine Version im Format {}x.y.z", plan.tag_prefix);
    }
    let branch = get(&format!("repos/{}/branches/{}", plan.repo, encode_ref(&plan.default_branch)))?.context("Zielbranch nicht gefunden")?;
    let sha = branch["commit"]["sha"].as_str().context("Zielbranch ohne Commit")?;
    if let Some(last) = &plan.last_tag {
        if commits_since(&plan.repo, last, sha)? == 0 {
            return Ok(Done::Skipped(format!("{} unverändert seit {last}", plan.default_branch)));
        }
    }
    if get(&format!("repos/{}/git/ref/tags/{}", plan.repo, encode_ref(tag)))?.is_some() {
        return Ok(Done::Skipped(format!("{tag} gibt es schon")));
    }
    let body = json!({ "ref": format!("refs/tags/{tag}"), "sha": sha });
    match send("POST", &format!("repos/{}/git/refs", plan.repo), Some(&body))? {
        (201, _) => Ok(Done::Ok(format!("gesetzt auf {}", short(sha)))),
        (_, body) => bail!("GitHub lehnt den Tag ab: {}", message(&body)),
    }
}

/// Deletes a planned branch on GitHub while it still points where the plan saw it.
pub fn delete_branch(plan: &Plan, branch: &Branch) -> Result<Done> {
    let Some(current) = get(&format!("repos/{}/branches/{}", plan.repo, encode_ref(&branch.name)))? else {
        return Ok(Done::Ok("war schon gelöscht".into()));
    };
    if current["commit"]["sha"].as_str() != Some(branch.sha.as_str()) {
        return Ok(Done::Skipped("hat seit der Vorschau neue Commits".into()));
    }
    if current["protected"].as_bool() == Some(true) {
        return Ok(Done::Skipped("ist geschützt".into()));
    }
    match send("DELETE", &format!("repos/{}/git/refs/heads/{}", plan.repo, encode_ref(&branch.name)), None)? {
        (204, _) => Ok(Done::Ok("gelöscht".into())),
        (_, body) => bail!("GitHub lehnt das Löschen ab: {}", message(&body)),
    }
}

/// Deletes a planned branch of the local clone while it still points where the plan saw it.
pub fn delete_local_branch(plan: &Plan, branch: &Branch) -> Result<Done> {
    let dir = Path::new(plan.clone.as_deref().context("kein lokaler Klon")?);
    if output("git", &["branch", "--show-current"], dir).unwrap_or_default().trim() == branch.name {
        return Ok(Done::Skipped("ist ausgecheckt".into()));
    }
    match output("git", &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", branch.name)], dir) {
        Err(_) => return Ok(Done::Ok("war schon gelöscht".into())),
        Ok(sha) if sha.trim() != branch.sha => return Ok(Done::Skipped("hat seit der Vorschau neue Commits".into())),
        Ok(_) => {}
    }
    // -D, because a squash-merged branch is not an ancestor; the plan checked it is merged.
    output("git", &["branch", "-D", &branch.name], dir)?;
    Ok(Done::Ok("gelöscht".into()))
}

/// The pull request with `mergeable` computed; GitHub fills it in the background after a change.
fn pull(repo: &str, number: u64) -> Result<Value> {
    let path = format!("repos/{repo}/pulls/{number}");
    for _ in 0..5 {
        let pr = get(&path)?.with_context(|| format!("PR #{number} nicht gefunden"))?;
        if !pr["mergeable"].is_null() {
            return Ok(pr);
        }
        thread::sleep(Duration::from_secs(2));
    }
    get(&path)?.with_context(|| format!("PR #{number} nicht gefunden"))
}

/// `Ok(Ok(()))` when GitHub reports the pull request mergeable and it has checks, all green.
fn readiness(repo: &str, pr: &Value) -> Result<std::result::Result<(), String>> {
    if pr["state"].as_str() != Some("open") {
        return Ok(Err("nicht mehr offen".into()));
    }
    if pr["draft"].as_bool() == Some(true) {
        return Ok(Err("Entwurf".into()));
    }
    if let Some(reason) = blocked_reason(pr["mergeable_state"].as_str().unwrap_or("unknown")) {
        return Ok(Err(reason.into()));
    }
    let sha = pr["head"]["sha"].as_str().context("PR ohne head.sha")?;
    if !has_checks(repo, sha)? {
        return Ok(Err("keine CI-Checks, also nichts, was grün sein könnte".into()));
    }
    Ok(Ok(()))
}

/// Why GitHub's `mergeable_state` keeps a pull request open; `None` when it can be merged. `clean`
/// means mergeable with every check finished and passed (a pending or failed one makes it `unstable`).
fn blocked_reason(state: &str) -> Option<&'static str> {
    Some(match state {
        "clean" | "has_hooks" => return None,
        "unstable" => "CI nicht grün (fehlgeschlagen oder läuft noch)",
        "blocked" => "blockiert (Review oder Pflicht-Checks fehlen)",
        "behind" => "nicht auf dem Stand des Zielbranches (Branch-Schutz verlangt ein Update)",
        "dirty" => "Merge-Konflikt",
        "draft" => "Entwurf",
        _ => "GitHub hat die Mergebarkeit noch nicht bestimmt; später noch einmal versuchen",
    })
}

/// A commit has at least one check run or commit status; without any, "green" means nothing.
fn has_checks(repo: &str, sha: &str) -> Result<bool> {
    let runs = get(&format!("repos/{repo}/commits/{sha}/check-runs?per_page=1"))?.unwrap_or_default();
    if runs["total_count"].as_u64().unwrap_or(0) > 0 {
        return Ok(true);
    }
    let statuses = get(&format!("repos/{repo}/commits/{sha}/status"))?.unwrap_or_default();
    Ok(statuses["total_count"].as_u64().unwrap_or(0) > 0)
}

/// Merge commit if the repository allows it, else squash, else rebase.
fn merge_method(info: &Value) -> &'static str {
    let allowed = |key: &str| info[key].as_bool() != Some(false);
    if allowed("allow_merge_commit") {
        "merge"
    } else if allowed("allow_squash_merge") {
        "squash"
    } else {
        "rebase"
    }
}

fn pull_request(pr: &Value, repo: &str) -> PullRequest {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    PullRequest {
        number: pr["number"].as_u64().unwrap_or(0),
        title: text(&pr["title"]),
        author: text(&pr["user"]["login"]),
        head_sha: text(&pr["head"]["sha"]),
        head_branch: same_repo(pr, repo).then(|| text(&pr["head"]["ref"])),
        url: text(&pr["html_url"]),
        reason: String::new(),
    }
}

fn same_repo(pr: &Value, repo: &str) -> bool {
    pr["head"]["repo"]["full_name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(repo))
}

/// Head commit → number of the recently merged pull requests (squash merges leave the branch
/// outside the default branch, but its head is still the merged one).
fn merged_heads(repo: &str) -> Result<HashMap<String, u64>> {
    Ok(get_pages(&format!("repos/{repo}/pulls?state=closed&sort=updated&direction=desc"), 1)?
        .iter()
        .filter(|pr| !pr["merged_at"].is_null())
        .filter_map(|pr| Some((pr["head"]["sha"].as_str()?.to_owned(), pr["number"].as_u64()?)))
        .collect())
}

/// Every commit of `branch` is in `base`.
fn contained(repo: &str, base: &str, branch: &str) -> Result<bool> {
    let compare = get(&format!("repos/{repo}/compare/{}...{}", encode_ref(base), encode_ref(branch)))?;
    Ok(compare.is_some_and(|c| matches!(c["status"].as_str(), Some("identical" | "behind"))))
}

/// Commits in `head` that `tag` does not have.
fn commits_since(repo: &str, tag: &str, head: &str) -> Result<u64> {
    let compare = get(&format!("repos/{repo}/compare/{}...{}", encode_ref(tag), encode_ref(head)))?.with_context(|| format!("{tag} und {head} nicht vergleichbar"))?;
    Ok(compare["ahead_by"].as_u64().unwrap_or(0))
}

/// `(1, 2, 3)` for `v1.2.3` with prefix `v`; only plain releases, no `-beta.1`.
fn version(tag: &str, prefix: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix(prefix)?.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// The highest release tag with the prefix.
fn latest_release_tag(tags: &[String], prefix: &str) -> Option<String> {
    tags.iter().filter_map(|tag| Some((version(tag, prefix)?, tag))).max_by_key(|(version, _)| *version).map(|(_, tag)| tag.clone())
}

fn next_patch(tag: &str, prefix: &str) -> Option<String> {
    let (major, minor, patch) = version(tag, prefix)?;
    Some(format!("{prefix}{major}.{minor}.{}", patch + 1))
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_latest_release_and_the_next_patch() {
        let tags: Vec<String> = ["0.1.9", "0.1.10", "0.2.0-beta.1", "v9.0.0", "0.1.2"].map(String::from).to_vec();
        assert_eq!(latest_release_tag(&tags, "").as_deref(), Some("0.1.10"));
        assert_eq!(next_patch("0.1.10", "").as_deref(), Some("0.1.11"));
        assert_eq!(latest_release_tag(&tags, "v").as_deref(), Some("v9.0.0"));
        assert_eq!(next_patch("v9.0.0", "v").as_deref(), Some("v9.0.1"));
        assert_eq!(latest_release_tag(&["beta".into(), "1.2".into(), "1.2.3.4".into()], ""), None);
    }

    #[test]
    fn only_clean_pull_requests_are_ready() {
        assert_eq!(blocked_reason("clean"), None);
        assert_eq!(blocked_reason("has_hooks"), None);
        for state in ["unstable", "blocked", "behind", "dirty", "draft", "unknown"] {
            assert!(blocked_reason(state).is_some(), "{state}");
        }
    }

    #[test]
    fn prefers_merge_commits() {
        assert_eq!(merge_method(&json!({})), "merge");
        assert_eq!(merge_method(&json!({ "allow_merge_commit": false })), "squash");
        assert_eq!(merge_method(&json!({ "allow_merge_commit": false, "allow_squash_merge": false })), "rebase");
    }

    /// `cargo test -p openreleasepilot-core plans_a_real_repository -- --ignored --nocapture`:
    /// the preview for this app's own repository; reads only, no local clone.
    #[test]
    #[ignore = "network"]
    fn plans_a_real_repository() {
        let plan = plan("OpenReleasePilot", crate::update::REPOSITORY, "", None, true, true);
        println!("{}", serde_json::to_string_pretty(&plan).unwrap());
        assert!(plan.error.is_none(), "{:?}", plan.error);
        assert_eq!(plan.default_branch, "main");
    }

    #[test]
    fn encodes_refs_for_paths() {
        assert_eq!(encode_ref("dependabot/cargo/sha2-0.11.0"), "dependabot/cargo/sha2-0.11.0");
        assert_eq!(encode_ref("fix#1 a"), "fix%231%20a");
    }
}
