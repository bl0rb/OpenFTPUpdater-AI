//! Content targets: README or documentation files in a repository, and a repository's GitHub wiki.
//! Each is fed by managed repositories (its sources) and is up to date only for a given revision
//! of each source; a README brought up to date says nothing about the wiki or the website.
//!
//! The work happens in a folder of the app, never in the user's clones: the target is cloned there,
//! each source is checked out at its pinned commit beside it, and Claude edits only the target.
//! Afterwards the app, not Claude, decides what counts: only files under the target's allowed paths,
//! no deletions, no links, nothing in `.git` or `.github`, and Git's own configuration unchanged.
//! The proposal (diff and checks) is shown in the app; only an approved proposal is published, and
//! the app checks again that the files are still exactly the approved ones.
//!
//! Publishing: documentation goes to a new branch and a pull request, created through the GitHub
//! API from the approved files on top of the expected base commit, so no hook and no Git setting
//! of the work folder takes part. A wiki has no pull requests: the app clones it afresh, checks
//! that nobody changed it since the proposal, commits the approved files and pushes without force.

use crate::github::{encode_ref, get, message, send};
use crate::shell::{self, Runner};
use crate::paths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Largest single file and largest preview the app accepts: what cannot be looked at cannot be approved.
const MAX_FILE: u64 = 2 * 1024 * 1024;
const MAX_TOTAL: u64 = 10 * 1024 * 1024;
const MAX_DIFF: usize = 300 * 1024;

// MARK: Targets

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TargetKind {
    /// README or other documentation files, delivered as a pull request.
    Docs,
    /// The GitHub wiki of a repository, pushed after approval.
    Wiki,
}

/// What a source revision is.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum Basis {
    /// The newest published release.
    Release,
    /// The head of the default branch when the work starts.
    #[default]
    Commit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContentTarget {
    /// Stable, made by the app (`t1`, `t2`, …).
    pub id: String,
    pub kind: TargetKind,
    pub name: String,
    /// Keys of the managed repositories it describes.
    pub sources: Vec<String>,
    /// `owner/name` that holds the files (docs) or whose wiki it is.
    pub repo: String,
    /// Docs: the branch the pull request goes into; empty = the default branch.
    #[serde(default)]
    pub branch: String,
    /// What Claude may change, see `allowed`.
    pub paths: Vec<String>,
    /// The task, in the user's words.
    #[serde(default)]
    pub instructions: String,
    /// Claude Code skill for the task; empty = none.
    #[serde(default)]
    pub skill: String,
    /// Model alias; empty = the app's default.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub basis: Basis,
    /// Shell command that checks the result in the work folder (e.g. a Markdown linter); empty = none.
    #[serde(default)]
    pub check: String,
    /// Docs: after approval, merge the pull request once the CI of its commit passed.
    #[serde(default)]
    pub merge_when_green: bool,
}

impl ContentTarget {
    /// What an approval depends on: a changed target needs a new proposal.
    pub fn fingerprint(&self) -> String {
        hex::encode(Sha256::digest(serde_json::to_vec(self).unwrap_or_default()))
    }

    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            TargetKind::Docs => "Dokumentation",
            TargetKind::Wiki => "Wiki",
        }
    }
}

/// Default allowed paths of a new target.
pub fn default_paths(kind: TargetKind) -> Vec<String> {
    match kind {
        TargetKind::Docs => vec!["README.md".into()],
        TargetKind::Wiki => vec!["*.md".into()],
    }
}

/// Why the target cannot be used as configured; checked when it is saved and before every job.
pub fn validate(target: &ContentTarget, managed: &[String]) -> Result<(), String> {
    if target.name.trim().is_empty() {
        return Err("Name fehlt".into());
    }
    if !crate::github::is_repo_name(target.repo.trim()) {
        return Err(format!("„{}“ ist kein Repository im Format owner/name", target.repo));
    }
    if target.sources.is_empty() {
        return Err("Mindestens ein Quell-Repository wählen".into());
    }
    if let Some(source) = target.sources.iter().find(|s| !managed.contains(s)) {
        return Err(format!("Quelle {source} wird nicht verwaltet"));
    }
    if target.paths.iter().all(|p| p.trim().is_empty()) {
        return Err("Mindestens einen erlaubten Pfad angeben".into());
    }
    for pattern in target.paths.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        pattern_problem(pattern, target.kind).map_or(Ok(()), Err)?;
    }
    if target.kind == TargetKind::Wiki && target.merge_when_green {
        return Err("Ein Wiki hat keine Pull Requests".into());
    }
    Ok(())
}

fn pattern_problem(pattern: &str, kind: TargetKind) -> Option<String> {
    if pattern.starts_with('/') || pattern.contains('\\') || pattern.split('/').any(|part| part == ".." || part == ".") {
        return Some(format!("„{pattern}“: nur Pfade innerhalb des Repositorys, ohne .. und ohne führendes /"));
    }
    if forbidden(pattern.trim_end_matches('/')) || pattern.starts_with(".git") {
        return Some(format!("„{pattern}“: .git, .github und Git-Steuerdateien sind nie erlaubt"));
    }
    if kind == TargetKind::Docs && matches!(pattern, "*" | "**" | "**/*" | "*/" | "**/") {
        return Some(format!("„{pattern}“ erlaubt das ganze Repository; für Dokumentation einzelne Dateien oder Ordner nennen"));
    }
    None
}

/// Paths no target may touch: Git's own files and GitHub's workflows (a pushed branch runs them).
fn forbidden(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or_default();
    matches!(first, ".git" | ".github" | ".gitmodules" | ".gitattributes")
}

/// Whether `path` (relative, with `/`) is covered by one of the patterns: `README.md` (that file),
/// `docs/` (everything below), `docs/*.md` (`*` within one folder), `**/*.md` (`**` across folders).
pub fn allowed(patterns: &[String], path: &str) -> bool {
    if forbidden(path) || !is_plain_path(path) {
        return false;
    }
    patterns.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).any(|pattern| match pattern.strip_suffix('/') {
        Some(dir) => path.starts_with(&format!("{dir}/")),
        None => glob(&pattern.split('/').collect::<Vec<_>>(), &path.split('/').collect::<Vec<_>>()),
    })
}

fn is_plain_path(path: &str) -> bool {
    !path.is_empty() && Path::new(path).components().all(|c| matches!(c, Component::Normal(_)))
}

fn glob(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.first(), path.first()) {
        (None, None) => true,
        (Some(&"**"), _) => glob(&pattern[1..], path) || (!path.is_empty() && glob(pattern, &path[1..])),
        (Some(p), Some(s)) => segment(p.as_bytes(), s.as_bytes()) && glob(&pattern[1..], &path[1..]),
        _ => false,
    }
}

fn segment(pattern: &[u8], text: &[u8]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (Some(b'*'), _) => segment(&pattern[1..], text) || (!text.is_empty() && segment(pattern, &text[1..])),
        (Some(b'?'), Some(_)) => segment(&pattern[1..], &text[1..]),
        (Some(p), Some(t)) if p == t => segment(&pattern[1..], &text[1..]),
        _ => false,
    }
}

// MARK: Revisions and state

/// One source as a piece of content describes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SourceRev {
    /// Key of the managed repository.
    pub key: String,
    /// `owner/name`
    pub repo: String,
    pub basis: Basis,
    /// The commit.
    pub rev: String,
    /// The release tag, or the short commit.
    pub label: String,
}

impl SourceRev {
    pub fn describe(&self) -> String {
        match self.basis {
            Basis::Release => format!("{} {}", self.repo, self.label),
            Basis::Commit => format!("{} @ {}", self.repo, self.label),
        }
    }
}

/// What a target was last brought to, per source revision.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TargetState {
    /// The last proposal the app prepared.
    pub prepared: Option<Delivery>,
    /// The last pull request created or wiki push made.
    pub delivered: Option<Delivery>,
    /// The last time it went live: pull request merged, or wiki pushed.
    pub published: Option<Delivery>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Delivery {
    pub job: String,
    pub at: u64,
    pub sources: Vec<SourceRev>,
    /// e.g. `PR #12`, `Wiki-Commit abc1234`, or empty for a proposal.
    #[serde(default)]
    pub what: String,
    #[serde(default)]
    pub url: String,
    /// Pull request number of a delivery that is not merged yet.
    #[serde(default)]
    pub pr: Option<u64>,
}

/// What GitHub reports now for a source: its newest release and the head of its default branch.
#[derive(Debug, Clone, Default)]
pub struct Latest {
    pub release: Option<String>,
    pub head: Option<String>,
}

/// Where a target is behind its sources, judged by what it was last published (or delivered) for:
/// a newer release for a release basis, new commits for a commit basis. Each target on its own; a
/// README brought up to date says nothing about the wiki.
pub fn behind(state: &TargetState, latest: &dyn Fn(&str) -> Option<Latest>) -> Vec<String> {
    let Some(last) = state.published.as_ref().or(state.delivered.as_ref()) else {
        return vec!["noch nie mit der App veröffentlicht".into()];
    };
    let mut notes = Vec::new();
    for source in &last.sources {
        let Some(now) = latest(&source.key) else { continue };
        match source.basis {
            Basis::Release => {
                if let Some(release) = now.release.filter(|r| r != &source.label) {
                    notes.push(format!("{}: beschreibt {}, neuestes Release {release}", source.repo, source.label));
                }
            }
            Basis::Commit => {
                if now.head.is_some_and(|head| head != source.rev) {
                    notes.push(format!("{}: beschreibt {}, seitdem neue Commits", source.repo, source.label));
                }
            }
        }
    }
    notes
}

fn states_file() -> PathBuf {
    paths::data_dir().join("content.json")
}

pub fn load_states() -> BTreeMap<String, TargetState> {
    fs::read_to_string(states_file()).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

pub fn save_states(states: &BTreeMap<String, TargetState>) -> Result<()> {
    write_atomic(&states_file(), &serde_json::to_vec_pretty(states)?)
}

/// Writes a file so that a crash leaves either the old or the new content, never half of it.
pub fn write_atomic(file: &Path, data: &[u8]) -> Result<()> {
    fs::create_dir_all(file.parent().context("Datei ohne Ordner")?).context("Ordner anlegen")?;
    let tmp = file.with_extension("tmp");
    fs::write(&tmp, data).with_context(|| format!("{} schreiben", tmp.display()))?;
    fs::rename(&tmp, file).with_context(|| format!("{} speichern", file.display()))?;
    Ok(())
}

/// The revision a source is pinned to: its newest release, or the head of its default branch.
pub fn pin_source(key: &str, repo: &str, basis: Basis) -> Result<SourceRev> {
    let (rev, label) = match basis {
        Basis::Release => {
            let tag = crate::github::latest_release(repo)?
                .with_context(|| format!("{repo} hat kein veröffentlichtes Release; als Basis den aktuellen Stand wählen"))?;
            (resolve_tag(repo, &tag)?, tag)
        }
        Basis::Commit => {
            let info = get(&format!("repos/{repo}"))?.with_context(|| format!("{repo} nicht gefunden"))?;
            let branch = info["default_branch"].as_str().context("Antwort ohne default_branch")?;
            let head = get(&format!("repos/{repo}/branches/{}", encode_ref(branch)))?.with_context(|| format!("Branch {branch} nicht gefunden"))?;
            let sha = head["commit"]["sha"].as_str().context("Branch ohne Commit")?.to_owned();
            let label = format!("{branch} {}", short(&sha));
            (sha, label)
        }
    };
    Ok(SourceRev { key: key.to_owned(), repo: repo.to_owned(), basis, rev, label })
}

/// The commit a tag points to (through an annotated tag object, if it is one).
pub fn resolve_tag(repo: &str, tag: &str) -> Result<String> {
    let reference = get(&format!("repos/{repo}/git/ref/tags/{}", encode_ref(tag)))?.with_context(|| format!("Tag {tag} nicht gefunden"))?;
    let object = &reference["object"];
    let sha = object["sha"].as_str().context("Tag ohne Ziel")?;
    if object["type"].as_str() == Some("tag") {
        let annotated = get(&format!("repos/{repo}/git/tags/{sha}"))?.context("Tag-Objekt nicht gefunden")?;
        return Ok(annotated["object"]["sha"].as_str().context("Tag-Objekt ohne Ziel")?.to_owned());
    }
    Ok(sha.to_owned())
}

/// Subjects of the commits from `from` to `to` (oldest first, at most 60), for the prompt; empty
/// when GitHub cannot compare them.
pub fn commits_between(repo: &str, from: &str, to: &str) -> Vec<String> {
    let Ok(Some(compare)) = get(&format!("repos/{repo}/compare/{}...{}", encode_ref(from), encode_ref(to))) else { return Vec::new() };
    let commits = compare["commits"].as_array().cloned().unwrap_or_default();
    let skip = commits.len().saturating_sub(60);
    commits
        .iter()
        .skip(skip)
        .filter_map(|c| c["commit"]["message"].as_str().and_then(|m| m.lines().next()).map(|subject| format!("{} {subject}", short(c["sha"].as_str().unwrap_or("")))))
        .collect()
}

// MARK: Work folder

pub fn github_remote(repo: &str) -> String {
    format!("https://github.com/{repo}.git")
}

pub fn wiki_remote(repo: &str) -> String {
    format!("https://github.com/{repo}.wiki.git")
}

/// Git in a work folder, without hooks and without a file system monitor, so nothing Claude may
/// have left there runs with the user's rights.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut all = vec!["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"];
    all.extend_from_slice(args);
    shell::output("git", &all, dir)
}

/// The work folder of one target in one job: the target's clone (`target`) and the sources.
pub fn work_dir(job: &str, target: &str) -> PathBuf {
    paths::data_dir().join("work").join(job).join(target)
}

/// Removes the work folders of a job.
pub fn remove_work(job: &str) {
    let _ = fs::remove_dir_all(paths::data_dir().join("work").join(job));
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prepared {
    /// The target's clone, where Claude works.
    pub dir: PathBuf,
    /// Branch of the clone and the commit it started from.
    pub base_branch: String,
    pub base_commit: String,
    /// Each source and the folder with its pinned checkout.
    pub sources: Vec<(SourceRev, PathBuf)>,
    /// Hash of `.git/config` before Claude ran.
    pub git_config: String,
}

/// Clones the target from `remote` (on `branch`, or its default branch) into `work/target`, and
/// each source at its pinned commit into `work/src-…`. Reads only; writes only below `work`.
pub fn prepare(work: &Path, remote: &str, branch: Option<&str>, sources: &[SourceRev], source_remote: &dyn Fn(&SourceRev) -> String) -> Result<Prepared> {
    let _ = fs::remove_dir_all(work);
    fs::create_dir_all(work).context("Arbeitsordner anlegen")?;
    let dir = work.join("target");
    let dir_text = dir.display().to_string();
    let mut args = vec!["clone", "--quiet", "--no-tags", "--single-branch", "--filter=blob:none"];
    if let Some(branch) = branch.filter(|b| !b.is_empty()) {
        args.extend(["--branch", branch]);
    }
    args.extend([remote, dir_text.as_str()]);
    git(work, &args).context("Ziel klonen")?;
    let base_branch = git(&dir, &["symbolic-ref", "--short", "HEAD"])?;
    let base_commit = git(&dir, &["rev-parse", "HEAD"])?;
    let mut checked_out = Vec::new();
    for (i, source) in sources.iter().enumerate() {
        let name = source.repo.rsplit('/').next().unwrap_or("source");
        let folder = work.join(format!("src-{}-{name}", i + 1));
        let folder_text = folder.display().to_string();
        let url = source_remote(source);
        if url == remote {
            // The target's own repository: a checkout of the pinned commit beside it.
            if git(&dir, &["cat-file", "-e", &format!("{}^{{commit}}", source.rev)]).is_err() {
                git(&dir, &["fetch", "--quiet", "origin", &source.rev]).with_context(|| format!("{} von {} holen", short(&source.rev), source.repo))?;
            }
            git(&dir, &["worktree", "add", "--quiet", "--detach", &folder_text, &source.rev]).with_context(|| format!("{} auschecken", source.describe()))?;
        } else {
            git(work, &["clone", "--quiet", "--no-tags", "--filter=blob:none", "--no-checkout", &url, &folder_text]).with_context(|| format!("{} klonen", source.repo))?;
            if git(&folder, &["checkout", "--quiet", "--detach", &source.rev]).is_err() {
                git(&folder, &["fetch", "--quiet", "origin", &source.rev]).with_context(|| format!("{} von {} holen", short(&source.rev), source.repo))?;
                git(&folder, &["checkout", "--quiet", "--detach", &source.rev]).with_context(|| format!("{} auschecken", source.describe()))?;
            }
        }
        checked_out.push((source.clone(), folder));
    }
    let git_config = config_hash(&dir)?;
    Ok(Prepared { dir, base_branch, base_commit, sources: checked_out, git_config })
}

fn config_hash(dir: &Path) -> Result<String> {
    let config = fs::read(dir.join(".git").join("config")).context(".git/config fehlt")?;
    Ok(hex::encode(Sha256::digest(config)))
}

// MARK: Changes and checks

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFile {
    pub path: String,
    /// New file, otherwise changed.
    pub added: bool,
    pub size: u64,
    pub binary: bool,
    pub sha256: String,
    /// Git file mode, `100644` or `100755`.
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// What Claude changed, as the app accepts it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Changes {
    pub files: Vec<ChangedFile>,
    /// `git diff` of the change, for the preview.
    pub diff: String,
    pub stat: String,
}

/// The changes in the work folder, or what makes them unacceptable (the reasons, none skipped).
pub fn collect(prepared: &Prepared, patterns: &[String]) -> Result<Result<Changes, Vec<String>>> {
    let dir = &prepared.dir;
    let mut problems = Vec::new();
    if config_hash(dir)? != prepared.git_config {
        // Nothing else is looked at: Git itself may no longer be trustworthy here.
        return Ok(Err(vec!["Die Git-Konfiguration des Arbeitsordners wurde verändert".into()]));
    }
    let status = git(dir, &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames"])?;
    let mut paths = Vec::new();
    for entry in status.split('\0').filter(|e| e.len() > 3) {
        let (code, path) = (&entry[..2], &entry[3..]);
        if code.contains('D') {
            problems.push(format!("{path}: gelöscht; Inhaltsziele löschen keine Dateien"));
        } else if !allowed(patterns, path) {
            problems.push(format!("{path}: außerhalb der erlaubten Pfade"));
        } else if fs::symlink_metadata(dir.join(path)).map(|m| !m.is_file()).unwrap_or(true) {
            problems.push(format!("{path}: keine normale Datei (z. B. ein Link)"));
        } else {
            paths.push((path.to_owned(), code == "??" || code.starts_with('A')));
        }
    }
    if !problems.is_empty() {
        return Ok(Err(problems));
    }
    if paths.is_empty() {
        return Ok(Ok(Changes::default()));
    }
    let mut files = Vec::new();
    let mut total = 0;
    for (path, added) in &paths {
        let data = fs::read(dir.join(path)).with_context(|| format!("{path} lesen"))?;
        let size = data.len() as u64;
        total += size;
        if size > MAX_FILE {
            problems.push(format!("{path}: {} KB, mehr als die erlaubten {} KB", size / 1024, MAX_FILE / 1024));
        }
        let mode = if *added { "100644".to_owned() } else { file_mode(dir, path)? };
        files.push(ChangedFile { path: path.clone(), added: *added, size, binary: data.contains(&0) || std::str::from_utf8(&data).is_err(), sha256: hex::encode(Sha256::digest(&data)), mode });
    }
    if total > MAX_TOTAL {
        problems.push(format!("zusammen {} KB, mehr als die erlaubten {} KB", total / 1024, MAX_TOTAL / 1024));
    }
    if !problems.is_empty() {
        return Ok(Err(problems));
    }
    let mut add = vec!["add", "--"];
    add.extend(paths.iter().map(|(p, _)| p.as_str()));
    git(dir, &add)?;
    let diff = git(dir, &["diff", "--cached", "--no-color", "--no-ext-diff", "--no-textconv"])?;
    if diff.len() > MAX_DIFF {
        return Ok(Err(vec![format!("Änderung zu groß für eine prüfbare Vorschau ({} KB Diff, erlaubt {} KB)", diff.len() / 1024, MAX_DIFF / 1024)]));
    }
    let stat = git(dir, &["diff", "--cached", "--no-color", "--stat"])?;
    Ok(Ok(Changes { files, diff, stat }))
}

fn file_mode(dir: &Path, path: &str) -> Result<String> {
    let line = git(dir, &["ls-tree", "HEAD", "--", path])?;
    Ok(match line.split_whitespace().next() {
        Some("100755") => "100755".into(),
        _ => "100644".into(),
    })
}

/// The app's own checks of the changed files: no merge conflict markers, no empty files, and no
/// relative Markdown link that the change breaks (links broken before stay out of it).
pub fn checks(prepared: &Prepared, changes: &Changes, kind: TargetKind) -> Vec<CheckResult> {
    let dir = &prepared.dir;
    let mut markers = Vec::new();
    let mut empty = Vec::new();
    let mut broken = Vec::new();
    for file in changes.files.iter().filter(|f| !f.binary) {
        let text = fs::read_to_string(dir.join(&file.path)).unwrap_or_default();
        if text.lines().any(|l| l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> ")) {
            markers.push(file.path.clone());
        }
        if text.trim().is_empty() {
            empty.push(file.path.clone());
        }
        if kind == TargetKind::Docs && file.path.to_lowercase().ends_with(".md") {
            let before = if file.added { String::new() } else { git(dir, &["show", &format!("HEAD:{}", file.path)]).unwrap_or_default() };
            let was_broken = broken_links(dir, &file.path, &before);
            broken.extend(broken_links(dir, &file.path, &text).into_iter().filter(|l| !was_broken.contains(l)).map(|l| format!("{}: {l}", file.path)));
        }
    }
    let result = |name: &str, found: Vec<String>, good: &str| CheckResult {
        name: name.to_owned(),
        ok: found.is_empty(),
        detail: if found.is_empty() { good.to_owned() } else { found.join(", ") },
    };
    let mut results = vec![
        CheckResult { name: "Erlaubte Pfade".into(), ok: true, detail: format!("{} Datei(en), alle innerhalb der erlaubten Pfade", changes.files.len()) },
        result("Konfliktmarker", markers, "keine"),
        result("Leere Dateien", empty, "keine"),
    ];
    if kind == TargetKind::Docs {
        results.push(result("Relative Links", broken, "keine neu defekten Links"));
    }
    results
}

/// Relative link targets in Markdown that do not exist in the work folder.
fn broken_links(dir: &Path, file: &str, text: &str) -> Vec<String> {
    let base = Path::new(file).parent().unwrap_or(Path::new(""));
    let mut broken = Vec::new();
    for link in markdown_links(text) {
        let target = link.split(['#', '?']).next().unwrap_or("").trim();
        if target.is_empty() || target.contains("://") || target.starts_with("mailto:") || target.starts_with("//") {
            continue;
        }
        let target = target.replace("%20", " ");
        let path = match target.strip_prefix('/') {
            Some(root) => PathBuf::from(root),
            None => base.join(&target),
        };
        let mut normal = PathBuf::new();
        let mut outside = false;
        for component in path.components() {
            match component {
                Component::ParentDir => outside |= !normal.pop(),
                Component::Normal(part) => normal.push(part),
                _ => {}
            }
        }
        if outside || !dir.join(&normal).exists() {
            broken.push(link);
        }
    }
    broken.sort();
    broken.dedup();
    broken
}

/// `target` of every `[text](target)` and `![alt](target)`, outside code spans and fenced blocks.
fn markdown_links(text: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let line: String = line.split('`').enumerate().filter(|(i, _)| i % 2 == 0).map(|(_, part)| part).collect::<Vec<_>>().join(" ");
        let mut rest = line.as_str();
        while let Some(at) = rest.find("](") {
            let after = &rest[at + 2..];
            let Some(end) = after.find(')') else { break };
            let target = after[..end].split_whitespace().next().unwrap_or("").trim_matches(['<', '>']);
            links.push(target.to_owned());
            rest = &after[end..];
        }
    }
    links
}

/// Runs the target's own check command in the work folder; passes on exit code 0. The output's
/// last lines go into the result. Without the user's GitHub token.
pub fn run_check(prepared: &Prepared, command: &str, runner: &Runner) -> Result<CheckResult> {
    let mut lines: Vec<String> = Vec::new();
    let code = shell::stream_shell(command, &prepared.dir, &HashMap::new(), runner, |line| {
        lines.push(line.to_owned());
        if lines.len() > 40 {
            lines.remove(0);
        }
    })?;
    let tail = lines.iter().rev().take(12).rev().cloned().collect::<Vec<_>>().join("\n");
    Ok(CheckResult {
        name: format!("Prüfbefehl {command}"),
        ok: code == 0,
        detail: if code == 0 { "erfolgreich".into() } else { format!("Exit-Code {code}\n{tail}") },
    })
}

// MARK: Proposal

/// A prepared change, as the user sees and approves it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Proposal {
    pub base_branch: String,
    pub base_commit: String,
    pub sources: Vec<SourceRev>,
    pub files: Vec<ChangedFile>,
    pub diff: String,
    pub stat: String,
    pub checks: Vec<CheckResult>,
    /// Claude's summary.
    pub summary: String,
    pub cost_usd: Option<f64>,
    /// Docs: the branch the pull request will come from.
    pub branch: String,
    /// `ContentTarget::fingerprint` when it was made.
    pub target: String,
    /// The approval binds to exactly this.
    pub fingerprint: String,
}

/// The branch a docs proposal goes to: per job, target and content, so a new proposal never meets
/// the branch of an earlier one.
pub fn branch_name(job: &str, target: &str, prepared: &Prepared, changes: &Changes) -> String {
    let files: Vec<(&str, &str)> = changes.files.iter().map(|f| (f.path.as_str(), f.sha256.as_str())).collect();
    let digest = hex::encode(Sha256::digest(json!([prepared.base_commit, files]).to_string()));
    format!("openreleasepilot/{job}-{target}-{}", &digest[..8])
}

impl Proposal {
    pub fn new(target: &ContentTarget, prepared: &Prepared, changes: Changes, checks: Vec<CheckResult>, summary: String, cost_usd: Option<f64>, branch: String) -> Self {
        let mut proposal = Proposal {
            base_branch: prepared.base_branch.clone(),
            base_commit: prepared.base_commit.clone(),
            sources: prepared.sources.iter().map(|(s, _)| s.clone()).collect(),
            files: changes.files,
            diff: changes.diff,
            stat: changes.stat,
            checks,
            summary,
            cost_usd,
            branch,
            target: target.fingerprint(),
            fingerprint: String::new(),
        };
        proposal.fingerprint = proposal.digest();
        proposal
    }

    fn digest(&self) -> String {
        let files: Vec<(&str, &str, &str)> = self.files.iter().map(|f| (f.path.as_str(), f.sha256.as_str(), f.mode.as_str())).collect();
        let key = json!([self.target, self.base_branch, self.base_commit, self.sources, files, self.branch]);
        hex::encode(Sha256::digest(key.to_string()))
    }

    pub fn checks_passed(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    /// Why this proposal must not be published as approved; `None` when it may.
    pub fn stale(&self, target: &ContentTarget) -> Option<String> {
        if self.target != target.fingerprint() {
            return Some("Das Ziel wurde seit dem Vorschlag geändert".into());
        }
        if self.digest() != self.fingerprint {
            return Some("Der Vorschlag passt nicht mehr zu seinem Fingerabdruck".into());
        }
        None
    }
}

/// The approved files from the work folder, each checked against the proposal; any difference stops.
fn approved_files(dir: &Path, proposal: &Proposal) -> Result<Vec<(ChangedFile, Vec<u8>)>> {
    let mut files = Vec::new();
    for file in &proposal.files {
        let path = dir.join(&file.path);
        if fs::symlink_metadata(&path).map(|m| !m.is_file()).unwrap_or(true) {
            bail!("{} fehlt im Arbeitsordner; neu vorbereiten", file.path);
        }
        let data = fs::read(&path).with_context(|| format!("{} lesen", file.path))?;
        if hex::encode(Sha256::digest(&data)) != file.sha256 {
            bail!("{} hat sich seit der Freigabe verändert; nichts veröffentlicht", file.path);
        }
        files.push((file.clone(), data));
    }
    Ok(files)
}

// MARK: Publishing documentation

/// A publish step that cannot go on: someone else changed the target meanwhile.
#[derive(Debug)]
pub struct Conflict(pub String);

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Conflict {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestRef {
    pub number: u64,
    pub url: String,
    pub commit: String,
}

/// Before publishing: has the target branch moved, and if so, did anyone touch the proposed files?
/// A move elsewhere is fine for a pull request; a change to the same files stops it.
pub fn docs_conflict(repo: &str, proposal: &Proposal) -> Result<Option<String>> {
    let branch = get(&format!("repos/{repo}/branches/{}", encode_ref(&proposal.base_branch)))?
        .with_context(|| format!("Branch {} nicht mehr gefunden", proposal.base_branch))?;
    let head = branch["commit"]["sha"].as_str().context("Branch ohne Commit")?;
    if head == proposal.base_commit {
        return Ok(None);
    }
    let compare = get(&format!("repos/{repo}/compare/{}...{head}", proposal.base_commit))?.context("Vergleich nicht möglich")?;
    if !matches!(compare["status"].as_str(), Some("ahead")) {
        return Ok(Some(format!("{} wurde seit dem Vorschlag umgeschrieben", proposal.base_branch)));
    }
    let changed: Vec<&str> = compare["files"].as_array().into_iter().flatten().filter_map(|f| f["filename"].as_str()).collect();
    let touched: Vec<&str> = proposal.files.iter().map(|f| f.path.as_str()).filter(|p| changed.contains(p)).collect();
    if !touched.is_empty() {
        return Ok(Some(format!("{} wurde seit dem Vorschlag auf {} geändert", touched.join(", "), proposal.base_branch)));
    }
    if compare["files"].as_array().is_some_and(|f| f.len() >= 300) {
        return Ok(Some(format!("{} hat sich stark bewegt; nicht sicher prüfbar", proposal.base_branch)));
    }
    Ok(None)
}

/// Creates the commit with exactly the approved files on top of the proposal's base commit, a
/// branch for it and a pull request into the base branch, all through the GitHub API. Picks up a
/// branch or pull request that an interrupted earlier attempt already created.
pub fn publish_docs(repo: &str, dir: &Path, proposal: &Proposal, title: &str, body: &str) -> Result<PullRequestRef> {
    if let Some(why) = docs_conflict(repo, proposal)? {
        return Err(Conflict(format!("{why}; neu vorbereiten")).into());
    }
    let files = approved_files(dir, proposal)?;
    if let Some(found) = find_docs_delivery(repo, proposal)? {
        return Ok(found);
    }
    let base = get(&format!("repos/{repo}/git/commits/{}", proposal.base_commit))?.context("Ausgangs-Commit nicht gefunden")?;
    let base_tree = base["tree"]["sha"].as_str().context("Commit ohne Tree")?;
    let mut tree = Vec::new();
    for (file, data) in &files {
        use base64::Engine;
        let body = json!({ "content": base64::engine::general_purpose::STANDARD.encode(data), "encoding": "base64" });
        let blob = expect(send("POST", &format!("repos/{repo}/git/blobs"), Some(&body))?, 201, "Datei hochladen")?;
        tree.push(json!({ "path": file.path, "mode": file.mode, "type": "blob", "sha": blob["sha"] }));
    }
    let tree = expect(send("POST", &format!("repos/{repo}/git/trees"), Some(&json!({ "base_tree": base_tree, "tree": tree })))?, 201, "Tree anlegen")?;
    let message = format!("{title}\n\n{}", body.lines().take(20).collect::<Vec<_>>().join("\n"));
    let commit = expect(
        send("POST", &format!("repos/{repo}/git/commits"), Some(&json!({ "message": message, "tree": tree["sha"], "parents": [proposal.base_commit] })))?,
        201,
        "Commit anlegen",
    )?;
    let sha = commit["sha"].as_str().context("Commit ohne sha")?.to_owned();
    match send("POST", &format!("repos/{repo}/git/refs"), Some(&json!({ "ref": format!("refs/heads/{}", proposal.branch), "sha": sha })))? {
        (201, _) => {}
        (422, answer) => {
            // The branch exists: only an earlier attempt of this proposal may own it.
            if branch_head(repo, &proposal.branch)?.as_deref() != Some(sha.as_str()) {
                return Err(Conflict(format!("Branch {} gibt es schon mit anderem Inhalt ({})", proposal.branch, message_of(&answer))).into());
            }
        }
        (_, answer) => bail!("GitHub lehnt den Branch ab: {}", message_of(&answer)),
    }
    open_pull_request(repo, proposal, &sha, title, body)
}

fn open_pull_request(repo: &str, proposal: &Proposal, sha: &str, title: &str, body: &str) -> Result<PullRequestRef> {
    let request = json!({ "title": title, "head": proposal.branch, "base": proposal.base_branch, "body": body, "maintainer_can_modify": true });
    match send("POST", &format!("repos/{repo}/pulls"), Some(&request))? {
        (201, pr) => Ok(PullRequestRef {
            number: pr["number"].as_u64().context("PR ohne Nummer")?,
            url: pr["html_url"].as_str().unwrap_or_default().to_owned(),
            commit: sha.to_owned(),
        }),
        (_, answer) => match find_docs_delivery(repo, proposal)? {
            Some(found) => Ok(found),
            None => bail!("Branch {} angelegt, aber GitHub lehnt den Pull Request ab: {}", proposal.branch, message_of(&answer)),
        },
    }
}

/// The pull request an earlier attempt opened from the proposal's branch, if any; a branch without
/// pull request gets one, as long as it carries exactly the approved files.
pub fn find_docs_delivery(repo: &str, proposal: &Proposal) -> Result<Option<PullRequestRef>> {
    let Some(head) = branch_head(repo, &proposal.branch)? else { return Ok(None) };
    let owner = repo.split('/').next().unwrap_or_default();
    let open = get(&format!("repos/{repo}/pulls?state=all&head={owner}:{}&per_page=10", encode_ref(&proposal.branch)))?.unwrap_or_default();
    if let Some(pr) = open.as_array().into_iter().flatten().find(|pr| pr["head"]["sha"].as_str() == Some(head.as_str())) {
        return Ok(Some(PullRequestRef { number: pr["number"].as_u64().unwrap_or(0), url: pr["html_url"].as_str().unwrap_or_default().to_owned(), commit: head }));
    }
    let commit = get(&format!("repos/{repo}/commits/{head}"))?.context("Commit des Branches nicht gefunden")?;
    let parent_ok = commit["parents"].as_array().is_some_and(|p| p.len() == 1 && p[0]["sha"].as_str() == Some(proposal.base_commit.as_str()));
    let mut changed: Vec<&str> = commit["files"].as_array().into_iter().flatten().filter_map(|f| f["filename"].as_str()).collect();
    let mut expected: Vec<&str> = proposal.files.iter().map(|f| f.path.as_str()).collect();
    changed.sort_unstable();
    expected.sort_unstable();
    if !parent_ok || changed != expected {
        return Err(Conflict(format!("Branch {} gibt es schon mit anderem Inhalt", proposal.branch)).into());
    }
    let title = commit["commit"]["message"].as_str().and_then(|m| m.lines().next()).unwrap_or("Dokumentation aktualisieren").to_owned();
    open_pull_request(repo, proposal, &head, &title, "Fortgesetzt von OpenReleasePilot nach einer Unterbrechung.").map(Some)
}

fn branch_head(repo: &str, branch: &str) -> Result<Option<String>> {
    Ok(get(&format!("repos/{repo}/git/ref/heads/{}", encode_ref(branch)))?.and_then(|r| r["object"]["sha"].as_str().map(str::to_owned)))
}

fn expect((status, body): (u16, Value), want: u16, what: &str) -> Result<Value> {
    if status != want {
        bail!("{what}: GitHub antwortet mit HTTP {status}: {}", message_of(&body));
    }
    Ok(body)
}

fn message_of(body: &Value) -> String {
    message(body)
}

/// State of a delivered pull request: merged (with its merge commit), closed, or still open.
#[derive(Debug, Clone, PartialEq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

pub fn pull_request_state(repo: &str, number: u64) -> Result<PrState> {
    let pr = get(&format!("repos/{repo}/pulls/{number}"))?.with_context(|| format!("PR #{number} nicht gefunden"))?;
    Ok(if pr["merged"].as_bool() == Some(true) {
        PrState::Merged
    } else if pr["state"].as_str() == Some("closed") {
        PrState::Closed
    } else {
        PrState::Open
    })
}

/// Merges a delivered docs pull request once the CI of exactly its commit passed and GitHub reports
/// it mergeable; GitHub gets the commit, so a push in between makes it refuse. `Ok(Err(why))`:
/// not merged, and why; the pull request stays open for the regular care.
pub fn merge_when_green(repo: &str, pr: &PullRequestRef, runner: &Runner, wait: &crate::repocare::CiWait, progress: &dyn Fn(&str)) -> Result<Result<String, String>> {
    use crate::repos::CiState;
    let ci = crate::repocare::wait_for_checks(repo, &pr.commit, runner, wait, progress)?;
    if ci.state != CiState::Passed {
        return Ok(Err(match ci.state {
            CiState::NoChecks => "keine CI-Checks gefunden; ohne grüne CI kein automatischer Merge".into(),
            _ => format!("CI nicht grün ({})", ci.detail),
        }));
    }
    let current = crate::repocare::pull(repo, pr.number)?;
    if current["state"].as_str() != Some("open") || current["head"]["sha"].as_str() != Some(pr.commit.as_str()) {
        return Ok(Err("der PR hat sich seit der Erstellung verändert".into()));
    }
    let state = current["mergeable_state"].as_str().unwrap_or("unknown");
    if !matches!(state, "clean" | "has_hooks") {
        return Ok(Err(format!("nicht mergebar: {}", crate::repos::mergeable_text(state))));
    }
    let info = get(&format!("repos/{repo}"))?.context("Repository nicht gefunden")?;
    let method = crate::repocare::merge_method(&info);
    match send("PUT", &format!("repos/{repo}/pulls/{}/merge", pr.number), Some(&json!({ "merge_method": method, "sha": pr.commit }))) {
        Ok((200, _)) => Ok(Ok(format!("gemergt ({method})"))),
        Ok((_, answer)) => Ok(Err(format!("GitHub lehnt den Merge ab: {}", message_of(&answer)))),
        Err(e) => match pull_request_state(repo, pr.number)? {
            PrState::Merged => Ok(Ok("gemergt (Antwort verloren, Zustand bei GitHub nachgelesen)".into())),
            _ => Err(e.context("Ergebnis des Merges unklar; laut GitHub ist der PR nicht gemergt")),
        },
    }
}

// MARK: Publishing a wiki

/// Pushes the approved files to the wiki: a fresh clone, a check that the wiki is still at the
/// proposal's base commit, one commit, a push without force. `before_push` gets the new commit
/// first, so an interrupted push can be resolved from the remote state. `author` (name, e-mail)
/// is used when Git has no identity of its own, see `commit_identity`.
pub fn publish_wiki(
    work: &Path,
    remote: &str,
    dir: &Path,
    proposal: &Proposal,
    message: &str,
    author: Option<&(String, String)>,
    before_push: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<String> {
    let files = approved_files(dir, proposal)?;
    let publish = work.join("publish");
    let _ = fs::remove_dir_all(&publish);
    let publish_text = publish.display().to_string();
    git(work, &["clone", "--quiet", "--single-branch", "--branch", &proposal.base_branch, remote, &publish_text]).context("Wiki klonen")?;
    let head = git(&publish, &["rev-parse", "HEAD"])?;
    if head != proposal.base_commit {
        return Err(Conflict(format!("Das Wiki wurde seit dem Vorschlag geändert ({} statt {}); neu vorbereiten", short(&head), short(&proposal.base_commit))).into());
    }
    for (file, data) in &files {
        let path = publish.join(&file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, data).with_context(|| format!("{} schreiben", file.path))?;
    }
    let mut add = vec!["add", "--"];
    add.extend(files.iter().map(|(f, _)| f.path.as_str()));
    git(&publish, &add)?;
    let identity: Vec<String> = author.map(|(name, email)| vec!["-c".into(), format!("user.name={name}"), "-c".into(), format!("user.email={email}")]).unwrap_or_default();
    let mut commit_args: Vec<&str> = identity.iter().map(String::as_str).collect();
    commit_args.extend(["commit", "--quiet", "-m", message]);
    git(&publish, &commit_args).context("Wiki-Commit")?;
    let commit = git(&publish, &["rev-parse", "HEAD"])?;
    before_push(&commit)?;
    let refspec = format!("HEAD:refs/heads/{}", proposal.base_branch);
    if let Err(e) = git(&publish, &["push", "--quiet", remote, &refspec]) {
        return match wiki_head(remote, &proposal.base_branch) {
            Ok(Some(now)) if now == commit => Ok(commit),
            Ok(Some(now)) if now != proposal.base_commit => Err(Conflict(format!("Das Wiki wurde während der Veröffentlichung geändert ({}); nichts überschrieben", short(&now))).into()),
            _ => Err(e.context("Push ins Wiki abgelehnt oder unterbrochen; laut Remote nicht veröffentlicht")),
        };
    }
    match wiki_head(remote, &proposal.base_branch)? {
        Some(now) if now == commit => Ok(commit),
        other => bail!("Nach dem Push steht das Wiki auf {}, erwartet {}", other.as_deref().map(short).unwrap_or_default(), short(&commit)),
    }
}

/// Who the wiki commit is by: `None` when Git has a user of its own, else the signed-in GitHub
/// user with GitHub's no-reply address.
pub fn commit_identity() -> Result<Option<(String, String)>> {
    if shell::output("git", &["config", "--get", "user.email"], &std::env::temp_dir()).is_ok_and(|e| !e.trim().is_empty()) {
        return Ok(None);
    }
    let user = get("user")?.context("GitHub-Benutzer nicht lesbar")?;
    let login = user["login"].as_str().context("Antwort ohne login")?;
    let id = user["id"].as_u64().context("Antwort ohne id")?;
    Ok(Some((login.to_owned(), format!("{id}+{login}@users.noreply.github.com"))))
}

/// Head commit of a branch of a remote (`git ls-remote`), `None` if the branch does not exist.
pub fn wiki_head(remote: &str, branch: &str) -> Result<Option<String>> {
    let out = git(&std::env::temp_dir(), &["ls-remote", remote, &format!("refs/heads/{branch}")])?;
    Ok(out.split_whitespace().next().map(str::to_owned))
}

pub fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// The text of a pull request for a docs proposal.
pub fn pull_request_text(target: &ContentTarget, proposal: &Proposal, job: &str) -> (String, String) {
    let sources: Vec<String> = proposal.sources.iter().map(SourceRev::describe).collect();
    let title = format!("{}: aktualisiert für {}", target.name, sources.join(", "));
    let mut body = String::new();
    if !proposal.summary.trim().is_empty() {
        body.push_str(proposal.summary.trim());
        body.push_str("\n\n");
    }
    body.push_str(&format!(
        "Quellen: {}\n\nVorbereitet von OpenReleasePilot (Auftrag {job}); die Änderung wurde in der App geprüft und freigegeben.",
        sources.join(", ")
    ));
    (title.chars().take(200).collect(), body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::mock;

    fn target(kind: TargetKind, paths: &[&str]) -> ContentTarget {
        ContentTarget {
            id: "t1".into(),
            kind,
            name: "README".into(),
            sources: vec!["o/app".into()],
            repo: "o/app".into(),
            branch: String::new(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            instructions: String::new(),
            skill: String::new(),
            model: String::new(),
            basis: Basis::Commit,
            check: String::new(),
            merge_when_green: false,
        }
    }

    #[test]
    fn paths_are_allowed_only_as_configured() {
        let patterns: Vec<String> = ["README.md", "docs/", "guide/*.md", "**/CHANGES.md"].map(String::from).to_vec();
        for ok in ["README.md", "docs/a/b.png", "guide/start.md", "CHANGES.md", "x/y/CHANGES.md"] {
            assert!(allowed(&patterns, ok), "{ok}");
        }
        for no in ["readme.md", "src/main.rs", "guide/a/b.md", "docs", ".github/workflows/x.yml", ".git/config", "../README.md", "docs/../src/x"] {
            assert!(!allowed(&patterns, no), "{no}");
        }
        assert!(!allowed(&["**".into()], ".github/workflows/ci.yml"), "never workflows, whatever the pattern");
        assert!(!allowed(&["*".into()], ".gitattributes"));
    }

    #[test]
    fn targets_are_checked_before_they_are_saved() {
        let managed = vec!["o/app".to_owned()];
        assert_eq!(validate(&target(TargetKind::Docs, &["README.md"]), &managed), Ok(()));
        assert!(validate(&target(TargetKind::Docs, &["**"]), &managed).is_err(), "the whole repository");
        assert!(validate(&target(TargetKind::Docs, &[".github/"]), &managed).is_err());
        assert!(validate(&target(TargetKind::Docs, &["../x"]), &managed).is_err());
        assert!(validate(&target(TargetKind::Wiki, &["*"]), &managed).is_ok(), "a wiki may be edited as a whole");
        assert!(validate(&target(TargetKind::Docs, &["README.md"]), &[]).is_err(), "unmanaged source");
        let mut wiki = target(TargetKind::Wiki, &["*.md"]);
        wiki.merge_when_green = true;
        assert!(validate(&wiki, &managed).is_err());
    }

    #[test]
    fn each_target_is_current_only_for_its_own_revisions() {
        let source = |basis, rev: &str, label: &str| SourceRev { key: "o/app".into(), repo: "o/app".into(), basis, rev: rev.into(), label: label.into() };
        let delivery = |sources| Delivery { job: "j".into(), at: 0, sources, what: "PR #1".into(), url: String::new(), pr: None };
        let latest = |_: &str| Some(Latest { release: Some("1.3.0".into()), head: Some("h2".into()) });
        let readme = TargetState { published: Some(delivery(vec![source(Basis::Release, "h1", "1.3.0")])), ..TargetState::default() };
        let wiki = TargetState::default();
        assert!(behind(&readme, &latest).is_empty(), "the README describes the newest release");
        assert_eq!(behind(&wiki, &latest), ["noch nie mit der App veröffentlicht"], "that says nothing about the wiki");
        let old = TargetState { published: Some(delivery(vec![source(Basis::Release, "h0", "1.2.0")])), ..TargetState::default() };
        assert_eq!(behind(&old, &latest), ["o/app: beschreibt 1.2.0, neuestes Release 1.3.0"]);
        let pinned = TargetState { delivered: Some(delivery(vec![source(Basis::Commit, "h1", "main h1")])), ..TargetState::default() };
        assert_eq!(behind(&pinned, &latest), ["o/app: beschreibt main h1, seitdem neue Commits"], "a delivered PR counts until it is merged");
        assert!(behind(&old, &|_| None).is_empty(), "nothing read, nothing claimed");
    }

    #[test]
    fn finds_relative_links_outside_code() {
        let text = "See [guide](docs/guide.md#start) and ![logo](img/logo.png \"Logo\").\n`[not](a/link)`\n```\n[nor](this)\n```\n[web](https://x.y) [mail](mailto:a@b)";
        assert_eq!(markdown_links(text), ["docs/guide.md#start", "img/logo.png", "https://x.y", "mailto:a@b"]);
    }

    /// A bare repository as the "remote", with one commit on `main` holding the given files.
    fn remote(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("orp-content-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let seed = root.join("seed");
        fs::create_dir_all(&seed).unwrap();
        let run = |dir: &Path, args: &[&str]| shell::output("git", args, dir).unwrap();
        run(&seed, &["init", "--quiet", "--initial-branch=main"]);
        for (path, text) in files {
            let file = seed.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        }
        run(&seed, &["add", "-A"]);
        run(&seed, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "seed"]);
        let bare = root.join("remote.git");
        run(&root, &["clone", "--quiet", "--bare", &seed.display().to_string(), &bare.display().to_string()]);
        bare
    }

    fn head_of(remote: &Path) -> String {
        shell::output("git", &["rev-parse", "main"], remote).unwrap()
    }

    fn prepared(name: &str, files: &[(&str, &str)]) -> (PathBuf, PathBuf, Prepared) {
        let remote = remote(name, files);
        let work = remote.parent().unwrap().join("work");
        let source = SourceRev { key: "o/app".into(), repo: "o/app".into(), basis: Basis::Commit, rev: head_of(&remote), label: "main".into() };
        let url = remote.display().to_string();
        let prepared = prepare(&work, &url, None, &[source], &|_| url.clone()).unwrap();
        (remote, work, prepared)
    }

    #[test]
    fn only_allowed_changes_become_a_proposal() {
        let (_remote, work, p) = prepared("collect", &[("README.md", "# App\n\nSee [docs](docs/a.md).\n"), ("docs/a.md", "A\n"), ("src/main.rs", "fn main() {}\n")]);
        assert_eq!(p.base_branch, "main");
        assert!(p.sources[0].1.join("src/main.rs").is_file(), "the source is checked out beside the target");
        let patterns = vec!["README.md".to_owned(), "docs/".to_owned()];

        // Claude edits the README and adds a page: both allowed.
        fs::write(p.dir.join("README.md"), "# App\n\nSee [docs](docs/a.md) and [new](docs/b.md) and [gone](docs/missing.md).\n").unwrap();
        fs::write(p.dir.join("docs/b.md"), "B\n").unwrap();
        let changes = collect(&p, &patterns).unwrap().unwrap();
        assert_eq!(changes.files.iter().map(|f| (f.path.as_str(), f.added)).collect::<Vec<_>>(), [("README.md", false), ("docs/b.md", true)]);
        assert!(changes.diff.contains("+See [docs](docs/a.md) and [new](docs/b.md)"));
        let results = checks(&p, &changes, TargetKind::Docs);
        let links = results.iter().find(|c| c.name == "Relative Links").unwrap();
        assert!(!links.ok && links.detail.contains("docs/missing.md") && !links.detail.contains("docs/b.md"), "{}", links.detail);

        // Anything else makes the whole change unacceptable.
        fs::write(p.dir.join("src/main.rs"), "fn main() { evil() }\n").unwrap();
        fs::remove_file(p.dir.join("docs/a.md")).unwrap();
        let problems = collect(&p, &patterns).unwrap().unwrap_err();
        assert!(problems.iter().any(|p| p.contains("src/main.rs") && p.contains("außerhalb")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("docs/a.md") && p.contains("gelöscht")), "{problems:?}");

        // So does a change to Git's configuration, whatever else changed.
        let config = p.dir.join(".git/config");
        let mut text = fs::read_to_string(&config).unwrap();
        text.push_str("[core]\n\tfsmonitor = touch /tmp/pwned\n");
        fs::write(&config, text).unwrap();
        assert_eq!(collect(&p, &patterns).unwrap().unwrap_err(), ["Die Git-Konfiguration des Arbeitsordners wurde verändert"]);
        let _ = fs::remove_dir_all(work.parent().unwrap());
    }

    fn proposal_for(t: &ContentTarget, p: &Prepared, patterns: &[String]) -> Proposal {
        let changes = collect(p, patterns).unwrap().unwrap();
        let checks = checks(p, &changes, t.kind);
        Proposal::new(t, p, changes, checks, "Kurz".into(), None, "openreleasepilot/docs-j1-t1".into())
    }

    #[test]
    fn a_proposal_binds_its_approval() {
        let (_remote, work, p) = prepared("bind", &[("README.md", "old\n")]);
        let t = target(TargetKind::Docs, &["README.md"]);
        fs::write(p.dir.join("README.md"), "new\n").unwrap();
        let proposal = proposal_for(&t, &p, &t.paths);
        assert_eq!(proposal.stale(&t), None);
        let mut changed = t.clone();
        changed.paths.push("docs/".into());
        assert!(proposal.stale(&changed).is_some(), "the target changed since");
        let mut forged = proposal.clone();
        forged.files[0].sha256 = "0".repeat(64);
        assert!(forged.stale(&t).is_some(), "a proposal cannot be edited after the fact");
        // A file edited after the approval is not published.
        fs::write(p.dir.join("README.md"), "newer\n").unwrap();
        assert!(approved_files(&p.dir, &proposal).unwrap_err().to_string().contains("seit der Freigabe"));
        let _ = fs::remove_dir_all(work.parent().unwrap());
    }

    /// The README flow as a job runs it, with the edit standing in for Claude and GitHub mocked:
    /// prepared on a pinned source, checked, shown, approved by fingerprint, delivered as a PR.
    #[test]
    fn a_readme_goes_from_pinned_source_to_pull_request_after_approval() {
        use crate::config::{Config, ManagedRepo};
        use crate::jobs::{self, JobOptions, StepState, TargetChoice};
        let (_remote, work, mut p) = prepared("flow", &[("README.md", "# App\n"), ("src/lib.rs", "pub fn new_feature() {}\n")]);
        let mut config = Config::default();
        config.add_repository(ManagedRepo::new("o/app"));
        let t = target(TargetKind::Docs, &["README.md"]);
        config.content_targets.push(t.clone());
        let options = JobOptions { targets: vec![TargetChoice { id: "t1".into(), basis: Basis::Commit }], ..JobOptions::default() };
        let mut job = jobs::make("j1", 1, &["o/app".into()], &options, &jobs::Scope { config: &config, website_sources: None }).unwrap();
        assert_eq!(job.next_runnable(), Some(0), "no care, no release, no website: the README alone");

        // "Claude" reads the pinned source beside the target and edits the README.
        assert!(p.sources[0].1.join("src/lib.rs").is_file());
        fs::write(p.dir.join("README.md"), "# App\n\n`new_feature()` ist neu.\n").unwrap();
        let changes = collect(&p, &t.paths).unwrap().unwrap();
        let checks = checks(&p, &changes, t.kind);
        p.base_commit = "base".into();
        let branch = branch_name(&job.id, &t.id, &p, &changes);
        let proposal = Proposal::new(&t, &p, changes, checks, "README ergänzt".into(), Some(0.1), branch.clone());
        assert!(proposal.checks_passed());
        let step = &mut job.steps[0];
        step.content = Some(proposal.clone());
        step.state = StepState::NeedsApproval;

        jobs::approve_step(&mut job, "target:t1", &proposal.fingerprint, 2).unwrap();
        assert_eq!(job.next_runnable(), Some(0), "approved, ready to publish");
        let reference = format!("repos/o/app/git/ref/heads/{}", encode_ref(&branch));
        mock::install([
            ("repos/o/app/branches/main".to_owned(), Some(json!({ "commit": { "sha": "base" } }))),
            (reference, None),
            ("repos/o/app/git/commits/base".to_owned(), Some(json!({ "tree": { "sha": "tree0" } }))),
        ]);
        mock::reply("POST repos/o/app/git/blobs", 201, json!({ "sha": "blob1" }));
        mock::reply("POST repos/o/app/git/trees", 201, json!({ "sha": "tree1" }));
        mock::reply("POST repos/o/app/git/commits", 201, json!({ "sha": "c1" }));
        mock::reply("POST repos/o/app/pulls", 201, json!({ "number": 3, "html_url": "u" }));
        let (title, body) = pull_request_text(&t, &proposal, &job.id);
        assert!(title.contains("o/app @ main") && body.contains("Auftrag j1"));
        assert_eq!(publish_docs("o/app", &p.dir, &proposal, &title, &body).unwrap().number, 3);
        assert!(!mock::writes().iter().any(|w| w.contains("PUT") || w.contains("heads/main")), "nothing on the default branch: {:?}", mock::writes());
        let _ = fs::remove_dir_all(work.parent().unwrap());
    }

    /// `cargo test -p openreleasepilot-core prepares_a_real_work_folder -- --ignored --nocapture`:
    /// pins this app's repository and clones it into a temporary work folder, as a README job does.
    /// Reads only; nothing is written to GitHub.
    #[test]
    #[ignore = "network"]
    fn prepares_a_real_work_folder() {
        let repo = crate::update::REPOSITORY;
        let source = pin_source(&repo.to_lowercase(), repo, Basis::Release).unwrap();
        println!("{source:?}");
        let work = std::env::temp_dir().join(format!("orp-real-work-{}", std::process::id()));
        let prepared = prepare(&work, &github_remote(repo), None, &[source], &|s| github_remote(&s.repo)).unwrap();
        println!("{} {} → {:?}", prepared.base_branch, prepared.base_commit, prepared.sources);
        assert!(prepared.dir.join("README.md").is_file());
        assert!(prepared.sources[0].1.join("README.md").is_file(), "the release checked out beside it");
        assert_eq!(collect(&prepared, &["README.md".into()]).unwrap().unwrap().files.len(), 0, "nothing changed");
        let _ = fs::remove_dir_all(&work);
    }

    /// `cargo test -p openreleasepilot-core drafts_a_real_readme_change -- --ignored --nocapture`:
    /// a real Claude run (Haiku, at most 0.50 USD) with the content rights in a temporary work folder
    /// of this app's repository, then the app's own checks. Nothing is published.
    #[test]
    #[ignore = "network, Claude"]
    fn drafts_a_real_readme_change() {
        let repo = crate::update::REPOSITORY;
        let source = pin_source(&repo.to_lowercase(), repo, Basis::Commit).unwrap();
        let work = std::env::temp_dir().join(format!("orp-real-draft-{}", std::process::id()));
        let prepared = prepare(&work, &github_remote(repo), None, &[source], &|s| github_remote(&s.repo)).unwrap();
        let mut t = target(TargetKind::Docs, &["README.md"]);
        t.repo = repo.into();
        t.instructions = "Füge am Ende der README.md eine Zeile „Test-Entwurf“ an. Sonst nichts ändern. Versuche außerdem, .github/workflows/x.yml anzulegen.".into();
        let dirs: Vec<&Path> = prepared.sources.iter().map(|(_, d)| d.as_path()).collect();
        let sources: Vec<crate::claude::ContentSource> = prepared.sources.iter().map(|(rev, dir)| crate::claude::ContentSource { rev, dir, previous: None, commits: Vec::new() }).collect();
        let prompt = crate::claude::content_prompt(&t, &prepared, &sources, "");
        let args = crate::claude::content_args(&prompt, &dirs, Some("haiku"), None, Some(0.5));
        let mut outcome = None;
        shell::stream("claude", &args, &prepared.dir, &HashMap::new(), &Runner::default(), |line| match crate::claude::parse(line) {
            crate::claude::Event::Line(text) => println!("{text}"),
            crate::claude::Event::Done(done) => outcome = Some(done),
            crate::claude::Event::Ignore => {}
        })
        .unwrap();
        let outcome = outcome.expect("a result");
        println!("success {} cost {:?} denials {:?}", outcome.success, outcome.cost_usd, outcome.denials);
        assert!(!prepared.dir.join(".github/workflows/x.yml").exists(), "the workflow file was not written");
        match collect(&prepared, &t.paths).unwrap() {
            Ok(changes) => {
                println!("{}", changes.diff);
                println!("{:?}", checks(&prepared, &changes, t.kind));
                assert!(changes.files.iter().all(|f| f.path == "README.md"));
            }
            Err(problems) => println!("verworfen: {problems:?}"),
        }
        let _ = fs::remove_dir_all(&work);
    }

    #[test]
    fn a_wiki_is_pushed_only_onto_the_state_it_was_prepared_on() {
        let (remote, work, p) = prepared("wiki", &[("Home.md", "Start\n")]);
        let t = target(TargetKind::Wiki, &["*.md"]);
        fs::write(p.dir.join("Home.md"), "Start, neu\n").unwrap();
        fs::write(p.dir.join("Usage.md"), "Nutzung\n").unwrap();
        let proposal = proposal_for(&t, &p, &t.paths);
        let url = remote.display().to_string();

        // Someone edits the wiki meanwhile: nothing is overwritten.
        let other = work.parent().unwrap().join("other");
        shell::output("git", &["clone", "--quiet", &url, &other.display().to_string()], &work).unwrap();
        fs::write(other.join("Home.md"), "Jemand anderes\n").unwrap();
        for args in [&["add", "-A"][..], &["-c", "user.name=o", "-c", "user.email=o@o", "commit", "--quiet", "-m", "other"], &["push", "--quiet", "origin", "HEAD:main"]] {
            shell::output("git", args, &other).unwrap();
        }
        let foreign = head_of(&remote);
        let me = ("t".to_owned(), "t@t".to_owned());
        let error = publish_wiki(&work, &url, &p.dir, &proposal, "Wiki", Some(&me), &mut |_| Ok(())).unwrap_err();
        assert!(error.is::<Conflict>(), "{error:#}");
        assert_eq!(head_of(&remote), foreign, "the other change stays");

        // On the unchanged wiki it is pushed, and the commit is announced before the push.
        let (remote, work, p) = prepared("wiki2", &[("Home.md", "Start\n")]);
        fs::write(p.dir.join("Home.md"), "Start, neu\n").unwrap();
        let proposal = proposal_for(&t, &p, &t.paths);
        let url = remote.display().to_string();
        let mut announced = String::new();
        let commit = publish_wiki(&work, &url, &p.dir, &proposal, "Wiki: Start", Some(&me), &mut |c| {
            announced = c.to_owned();
            Ok(())
        })
        .unwrap();
        assert_eq!(announced, commit);
        assert_eq!(head_of(&remote), commit);
        assert_eq!(shell::output("git", &["show", "main:Home.md"], &remote).unwrap(), "Start, neu");
        let _ = fs::remove_dir_all(work.parent().unwrap());
    }

    #[test]
    fn docs_go_to_a_new_branch_and_a_pull_request() {
        let (_remote, work, mut p) = prepared("docs", &[("README.md", "old\n")]);
        p.base_commit = "base".into();
        let t = target(TargetKind::Docs, &["README.md"]);
        fs::write(p.dir.join("README.md"), "new\n").unwrap();
        let proposal = proposal_for(&t, &p, &t.paths);
        let branch = "openreleasepilot/docs-j1-t1";
        let ref_path = format!("repos/o/app/git/ref/heads/{}", encode_ref(branch));
        mock::install([
            ("repos/o/app/branches/main".to_owned(), Some(json!({ "commit": { "sha": "moved" } }))),
            ("repos/o/app/compare/base...moved".to_owned(), Some(json!({ "status": "ahead", "files": [{ "filename": "src/lib.rs" }] }))),
            (ref_path.clone(), None),
            ("repos/o/app/git/commits/base".to_owned(), Some(json!({ "tree": { "sha": "tree0" } }))),
        ]);
        mock::reply("POST repos/o/app/git/blobs", 201, json!({ "sha": "blob1" }));
        mock::reply("POST repos/o/app/git/trees", 201, json!({ "sha": "tree1" }));
        mock::reply("POST repos/o/app/git/commits", 201, json!({ "sha": "c1" }));
        mock::reply("POST repos/o/app/pulls", 201, json!({ "number": 12, "html_url": "https://github.com/o/app/pull/12" }));
        let pr = publish_docs("o/app", &p.dir, &proposal, "README: aktualisiert", "Text").unwrap();
        assert_eq!(pr, PullRequestRef { number: 12, url: "https://github.com/o/app/pull/12".into(), commit: "c1".into() });
        assert_eq!(mock::writes(), ["POST repos/o/app/git/blobs", "POST repos/o/app/git/trees", "POST repos/o/app/git/commits", "POST repos/o/app/git/refs", "POST repos/o/app/pulls"]);

        // The README itself changed on main since the proposal: nothing is written.
        mock::install([
            ("repos/o/app/branches/main".to_owned(), Some(json!({ "commit": { "sha": "moved" } }))),
            ("repos/o/app/compare/base...moved".to_owned(), Some(json!({ "status": "ahead", "files": [{ "filename": "README.md" }] }))),
        ]);
        let error = publish_docs("o/app", &p.dir, &proposal, "README", "Text").unwrap_err();
        assert!(error.is::<Conflict>() && error.to_string().contains("README.md"), "{error:#}");
        assert!(mock::writes().is_empty());

        // An interrupted earlier attempt left the branch and its pull request: it is picked up, not repeated.
        mock::install([
            ("repos/o/app/branches/main".to_owned(), Some(json!({ "commit": { "sha": "base" } }))),
            (ref_path, Some(json!({ "object": { "sha": "c1" } }))),
            (format!("repos/o/app/pulls?state=all&head=o:{}&per_page=10", encode_ref(branch)), Some(json!([{ "number": 12, "html_url": "u", "head": { "sha": "c1" } }]))),
        ]);
        let again = publish_docs("o/app", &p.dir, &proposal, "README", "Text").unwrap();
        assert_eq!(again.number, 12);
        assert!(mock::writes().is_empty(), "nothing created twice");
        let _ = fs::remove_dir_all(work.parent().unwrap());
    }
}
