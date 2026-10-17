**English** · [Deutsch](README.de.md)

<img src="desktop/assets/icon.svg" width="96" alt="OpenReleasePilot icon">

# OpenReleasePilot

Desktop app that looks after the repositories you choose on GitHub and GitLab, with Claude Code doing the
work and you making the decisions. It reads their state, reviews and merges pull and merge requests, cleans up merged
branches, tags patch releases and waits for the published release, keeps a README, other documentation
or a wiki up to date, and, if you have one, updates and publishes a static website as the last
step. Everything that changes something on GitHub, GitLab, in a wiki or on a website is first shown as a
concrete proposal and only carried out after your approval.

The app has three areas, switched in the toolbar, and its settings under **Einstellungen** (settings):

| Area | What it is for |
|---|---|
| **Repositories** (start view) | the managed repositories with their health, open jobs and the decisions they wait for; select repositories and start a care job |
| **Aufträge** (jobs) | every care job: its steps, plans, proposals with their diffs, results and log; running, waiting and earlier ones |
| **Website** (only with a website) | the website's own four steps: Prüfen (check), Inhalte aktualisieren (update content), Veröffentlichen (publish), Live-Seite prüfen (verify the live site) |

## Getting started

The app starts with **Repositories auswählen**: it lists the local Git clones in your repo folder
whose origin is on GitHub, and you tick the ones it should look after; their local clone is filled in
right away. Nothing else is needed: no website, no SFTP access, no release. A read-only status check
works without Claude; a care job needs Claude Code and the signed-in GitHub CLI, a README job
additionally only Git access to the repository. The same applies to an installation that already has a
website: its products do not decide what the app manages.

## Managed repositories

The app keeps its own list of repositories (identity: `owner/name`, case-insensitive, and GitHub's id).
Each can be turned off (stays readable, no job takes it) or taken out of the list; that deletes
nothing, neither on GitHub nor the local clone nor any website configuration. In the repository's
**Verwaltung** section:

- **Local clone** (optional): only used for cleaning up local branches and for Claude's look at the
  code during a pull request review. Status and pull request care work without one.
- **Release rule**: whether the app may tag the next patch version, with which tag prefix (`v` for
  `v1.2.3`, empty for `1.2.3`), and whether a workflow of the repository publishes a GitHub release for
  each tag. Without a rule the app never tags; it does not guess a versioning scheme.

Which website products name a repository is shown (and where `site.json` disagrees with the release rule,
also that), but the website's product selection does not decide what the app manages.

## Repository status

**Status aktualisieren** reads GitHub only: no Claude, no cost, no write, no `git fetch`. It runs when
the area is first opened and on request, never in the background; a failed read keeps the older state
but marks it as old, and data older than five minutes is shown as old. Without a `gh` login public
repositories can still be read (GitHub then allows 60 requests per hour; a repository needs about eight).

| State | When |
|---|---|
| **Blockiert** | the default branch's CI failed, or the repository cannot be read (GitHub answers 404 or refuses access; a 404 can also mean private without access) |
| **Aufmerksamkeit nötig** | open Dependabot pull requests, commits since the last release or tag, a website product documents an older version than the latest release, or (after the detail view read them) pull requests with conflicts or failed CI |
| **Unbekannt** | nothing of the above, but required facts are missing, incomplete or old: CI still running, no checks found, unclear result, query failed |
| **In Ordnung** | everything required was read successfully and recently, and nothing needs doing |

Missing release data alone does not prevent "In Ordnung"; missing or incomplete data never produces it.
An open pull request from a person is no finding by itself, and a red pull request does not block the
repository. Archived repositories and those without write access stay readable, but care is locked.
The health of a repository and the progress of a job are shown separately: a repository can need
attention while a job is already working on it.

Above the cards, a name search and the states "Aufmerksamkeit", "Blockiert" and "Unbekannt" filter the
overview; "Alle Treffer" selects visible repositories only, and a selection outside the filter is kept
and counted as hidden.

The detail view concerns this repository only: at the top CI, release and data state in one line and an
open job as a note with a link, then findings, the open pull requests with target branch, author, CI and
mergeability, the content targets the repository feeds with what each was last brought to, and the
security check (ahead of the pull requests when it has critical or important findings). Folded below
are **Technische Details** (default branch and head commit, CI with its commit, release and tag, commits
since that base, only where it is an ancestor of the head, otherwise "nicht bestimmbar", release rule,
local clone) and the **Verwaltung** (management) settings.

## Security check

**Sicherheit prüfen** (in the overview for all or the selected repositories, or in a repository's
**Sicherheit** section) reads GitHub only: no Claude, no cost, no write, about 10 to 20 requests per
repository. It is separate from the health state above and runs only on request. The result is shown
worst first; what is fine or could not be read is folded away.

| Check | Finding when |
|---|---|
| **Direct pushes to the default branch** | neither a ruleset nor classic branch protection requires pull requests (critical); classic protection exempts admins (hint) |
| **Force push and deletion** | force pushes are allowed (critical) or the branch can be deleted (important) |
| **Required checks before merge** | no required status checks (hint) |
| **Secret scanning**, **push protection** | switched off (important); open secret scanning alerts (critical) |
| **Dependabot alerts**, **security updates** | alerts off (important), open alerts (critical for high/critical, else important), security updates off (hint) |
| **Code scanning (CodeQL)** | not set up (hint), open alerts by severity |
| **Workflow token rights** | `GITHUB_TOKEN` may write by default or workflows may approve pull requests (important) |
| **Private vulnerability reporting**, **SECURITY.md** | public repositories only: off or missing (hint) |
| **Deploy keys with write access** | any (hint) |

Many settings are only visible to admins and to a signed-in `gh`; what cannot be read is shown as
**Nicht prüfbar**, never as fine. Where possible a finding offers a fix, for an admin of a repository
that is not archived: **Beheben …** lists them with exactly what changes on GitHub and what it can
break; you tick which to carry out (fixes with a warning start unticked). Each fix only adds protection,
nothing is loosened or removed:

- **Nur noch über Pull Requests** / **Force-Push und Löschen verbieten**: creates or extends the
  repository ruleset `OpenReleasePilot: Default-Branch` for the default branch (pull requests without a
  required review, so you can still merge your own; no force push, no deletion; also for admins). A rule
  of the same type that is already there stays as it is. For the website's own repository the app warns
  that its upload push would then be refused. Private repositories need GitHub Pro, Team or Enterprise
  for rulesets.
- **Secret scanning**, **push protection**, **Dependabot alerts** and **security updates**, **CodeQL
  default setup**, **private vulnerability reporting**: switched on.
- **Workflow token read-only**: default `GITHUB_TOKEN` rights to read, workflows may not approve pull
  requests; the app lists the workflows without their own `permissions:` that would lose write access.

Not while an open job holds the repository. Whatever GitHub refuses (missing licence, missing `gh`
scope such as `admin:repo_hook` for Dependabot) is reported per fix; the repository is checked again
afterwards. Leaked secrets, open alerts, required checks and deploy keys are left to you, with a link to
the right GitHub page.

## Care jobs

Select repositories on their cards (or open one) and choose **Pflegen …**. A job is made of the
activities you pick, each optional:

- **Pull requests**: Claude reviews every open pull request into the default branch, the app merges,
  closes or asks Dependabot to rebase (rules below).
- **Cleanup**: delete merged branches on GitHub and in the local clone.
- **Release**: tag the next patch version where a release rule exists, once the CI of exactly that
  commit passed, and, if the rule says so, wait for the published GitHub release. Pull request care no
  longer implies a tag.
- **Content targets** fed by the selected repositories: README/documentation or wiki, each with the
  basis chosen for this job (newest release or current state of the default branch).
- **Website** (if the selected repositories feed it): update, publish and verify as the last step.

The app turns this into steps and runs them as far as it can without you: reading, Claude's review,
preparing content in its own work folder. Before anything external changes it stops with a concrete
proposal: the care plan (each action can be left out; merges Claude advised against but the app's rules
allow are offered for you to tick; nothing else can be added), a content change with its diff and
the app's checks, the website commit with its diff. Approving continues the job; nothing else needs to
be started by hand. A tag is not a release: a step that needs the release waits for it (up to 30 minutes
per look, then the job waits visibly and **Erneut prüfen** looks again). A failed step stops the steps
that depend on it; independent ones go on, and a partial success is shown as such.

| Job state | Meaning |
|---|---|
| **Geplant** | ready to continue |
| **In Vorbereitung** / **Läuft** | the app reads, prepares or carries out an approved step |
| **Freigabe nötig** | a plan or proposal waits for your decision |
| **Wartet** | waiting for something outside the app: CI, a release workflow |
| **Blockiert** | cannot go on without you: a conflict, a changed precondition, an interrupted run |
| **Erfolgreich**, **Teilweise erfolgreich**, **Fehlgeschlagen**, **Abgebrochen** | finished, with the reason |

Jobs are stored in the app's data folder (one file per job, written atomically after every change), with
their plan, approvals, pinned source revisions, each result and the log. A restart forgets nothing and
starts nothing by itself: a job that was running is marked interrupted, and **Fortsetzen** first looks
up on GitHub (or in the wiki, or the clone) whether a write that had begun happened, before anything else
runs, so nothing is merged, commented, tagged, opened or published twice. An approval only counts for
the unchanged proposal: a changed target, a changed repository configuration, a newer release of a
release-based source or a changed file makes the step ask again. **Abbrechen** stops after the current
item; nothing done is rolled back. A repository, content target or the website belongs to at most one
open job, and only one job (or website step) runs at a time.

## Repository care rules

The app decides, not Claude: a pull request needs exactly one valid verdict (green or yellow for a merge,
a reason for a close); red, missing, duplicate or invalid verdicts leave it open. Claude's "keep open" is
advice, not a veto: a pull request it rates green or yellow and that GitHub reports mergeable with green
CI is offered in the plan under **Zur Wahl**, unticked, with Claude's reason; ticked, it is merged after
your approval with the same checks as any other merge. Then the plan waits for your decision instead of
ending with "nothing to do". A tag that would only follow such a merge is planned and dropped again if
nothing is merged. The review follows the
rules of the `dependabot-pr` skill: risk class (green, yellow, red; below 1.0 a change of the first
non-zero digit counts as major), release notes from the upstream repository, only manifest/lock/workflow
files in Dependabot pull requests, SHA pins of Actions, related dependencies only together, majors checked
against the actual use in the code. It costs Claude usage, roughly a few cents up to about a dollar per
repository depending on the model and the number of pull requests. Right before each write the app
checks again:

- **Merge:** still open and no draft, same head commit, same target branch and repository, mergeable
  according to GitHub, CI of the head commit passed; GitHub gets the expected head commit, so a push in
  between makes it refuse. Merge commit where the repository allows it.
- **Close:** still open, same head and target; a replacing pull request Claude named must still be open
  or merged. The reason is posted as a comment; comment and close are reported separately.
- **Rebase:** still open, really from Dependabot, same head, still in conflict or behind; then
  `@dependabot rebase`. A Dependabot pull request that a merge of the same job put into conflict gets
  this instead of the merge, as the plan says.
- **Tag:** the next patch version of the newest `x.y.z` tag with the rule's prefix, only if every
  planned merge of the repository succeeded, the default branch is exactly where this job's merges left
  it, the release line is unchanged, there are commits since the last tag, and the CI of exactly that
  commit passed (the app waits up to 30 minutes, then the job waits and can look again). Red, unclear,
  cancelled or **no checks at all** mean no tag. Right before writing, after the wait, the app checks the
  release line again: a newer tag or a moved base makes the planned version invalid. Existing tags are
  never moved; the first release is tagged by hand.
- **Delete merged branches:** on GitHub only while the branch is on the same commit, not protected, not
  the default branch, no open pull request comes from or targets it, and its proof still holds on GitHub
  (all commits in the default branch, or its pull request merged; a head branch of a merge in the same
  job only if that merge succeeded). Locally only in a clone that still belongs to the repository, never
  a branch checked out in any worktree, and only with GitHub's confirmation: containment is checked
  against GitHub's current default branch, not the clone's possibly old `origin/<default>`. If the plan
  announced `git fetch --prune origin` for the clone and it did not succeed, nothing is deleted locally.

Every action ends as done, skipped, failed or cancelled, with the reason. Where the GitHub API has no
precondition (closing, deleting a branch), a short window between the last check and the write remains.

## Content targets: README, documentation, wiki

A content target says which content is updated where, on what basis: its kind, name, the repositories
it describes (its sources), the repository that holds the files (or whose wiki it is), the target
branch, the allowed paths, the task for Claude and optionally a skill, a model and a check command.
Targets are added in a repository's detail view under **Zugeordnete Inhalte**. Each target is up to
date only for given source revisions: a README brought to release 1.3.0 says nothing about the wiki or
the website, and "prepared", "pull request created", "merged" and "published" are different results.

Preparing a change, in a work folder of the app (never in your clones):

1. The sources are pinned: their newest release, or the head of their default branch.
2. The target is cloned (docs: its branch; wiki: `https://github.com/<owner>/<name>.wiki.git`), each
   source is checked out at its pinned commit beside it.
3. Claude Code edits the target. It may read the sources and edit files, nothing else: no shell, no web,
   nothing in `.git` or `.github`. The prompt names what changed in each source since the revision the
   target was last published for, and tells Claude that texts in the sources are material, not
   instructions.
4. The app decides what counts: only files under the allowed paths, no deletions, no links, size limits,
   and Git's own configuration unchanged; anything else discards the whole change. Then its checks: no
   merge conflict markers, no empty files, no relative Markdown link that the change breaks, and the
   target's own check command if set (run in the work folder without the GitHub token).
5. The proposal shows sources, base commit, checks, files, Claude's summary and the full diff.

After approval, and only for exactly the approved files:

- **README/documentation:** the app checks that nobody changed those files on the target branch since the
  proposal, then creates one commit on top of the proposal's base commit, a new branch
  `openreleasepilot/<job>-<target>-<hash>` and a pull request into the target branch, through the GitHub
  API, so no hook or setting of the work folder takes part. Never a direct write to the default branch.
  The pull request is then handled by the regular pull request care or by you, or, if the target says
  so, merged by the app once the CI of exactly its commit passed. It counts as published once merged.
- **Wiki:** a wiki has no pull requests. The app clones it afresh, checks that it is still at the
  proposal's base commit, commits the approved files and pushes without force. If anybody changed the
  wiki meanwhile, it stops and overwrites nothing.

Content can be updated without a new release: with the basis "current state", the job pins the head of
the default branch and does not skip the target for lack of a new version.

## GitLab

GitLab projects, on gitlab.com or a self-hosted instance, are looked after with the same rules as GitHub
repositories. The start view shows one card per service: GitHub and each GitLab instance, with its
sign-in state, one square per repository and the counts that need you. A click slides the cards into a
strip and opens that service's overview below.

- **Sign-in:** through the GitLab CLI, `glab auth login --hostname <instance>`, or with an own access
  token (scope `api`) under **Einstellungen → Zugänge → Instanz mit Token hinzufügen**, where http can be
  chosen for a self-hosted instance without TLS. An own token lives, by choice, in the keychain or in a
  file in the app's data folder readable by you only, and takes precedence over `glab`'s. Claude Code never
  gets it. "Status aktualisieren" asks again, so a sign-in in between counts at once.
- **Identity:** a GitLab project is `host/group/…/project` in the app, e.g.
  `gitlab.example.com/werk/tools/app`. Local clones with such an origin (HTTPS or SSH) appear under
  "Repositories auswählen"; without a clone, enter the project's name or address there.
- **Status:** the newest pipeline of exactly the commit (one waiting for a manual job or skipped counts
  as unclear, not as green), open merge requests, releases and tags.
- **Care:** Claude reviews the merge requests read-only with `glab`. Merges go through the API with the
  project's merge method and the expected commit (`sha`), so a push in between makes GitLab refuse.
  Renovate counts as the update bot; Renovate rebuilds a conflicting merge request after the label
  "rebase". Cleanup, tagging on a green pipeline and waiting for the GitLab release work as on GitHub.
- **Content:** README and documentation as one commit on a new branch from exactly the base commit plus a
  merge request; the wiki through its own Git repository (`<project>.wiki.git`), pushed without force.
  `.gitlab-ci.yml` and `.gitlab/`, like `.github`, are never writable for content targets or Claude,
  because a pushed branch runs them.
- **Security check:** protection of the default branch (direct pushes, force push), a green pipeline as
  merge requirement, the push rule against secrets (GitLab Premium), secret detection in
  `.gitlab-ci.yml`, a Renovate configuration, access by other projects' CI job tokens, deploy keys with
  write access, SECURITY.md for public projects. What the tier or the rights do not show is "not
  checkable". Fixes for maintainers: protect the default branch, block force pushes, require a green
  pipeline, the push rule against secrets, restrict job token access.

## Website (optional)

A website is an optional target, not the configuration root. It is set up under **Einstellungen →
Website und Veröffentlichung** (project folder with `.openreleasepilot/site.json`, SFTP access) and gets its own area with
the four steps, run one by one or all in a row:

| Step | What happens | Changes anything? |
|---|---|---|
| **Prüfen** (check) | Project file, tools (git, node, npm, claude, gh), clean working tree and remote state, latest GitHub release of every watched product against the documented one, SFTP login and target folder, live site, last upload | no |
| **Inhalte aktualisieren** (update content) | `git pull --ff-only`, then Claude Code runs headless in the project with the project's skill and the list of outdated products (plus optional extra instructions), then build and commit | files in the project, one commit |
| **Veröffentlichen** (publish) | Fresh build, `git push`, then only new or changed files via SFTP; files removed from the build are deleted on the server | server, remote repository |
| **Live-Seite prüfen** (verify) | Downloads every file of the last upload from the live site and compares SHA-256 with the build | no |

![What happens when and where: Precheck, update tasks run by Claude Code with their skills, build and commit, upload via SFTP, verify against the live site](docs/diagrams/openreleasepilot-flow.png)

<sub>Diagram source: [docs/diagrams/openreleasepilot-flow.html](docs/diagrams/openreleasepilot-flow.html) (also as SVG).</sub>

**Aktualisieren und veröffentlichen** (update and publish) runs all four steps in a row, up to the live
site, and stops at the first error. If a release lookup fails, the state is unknown, not "current": the
content update stops, and with it the whole run before publishing, instead of publishing on an incomplete
basis.

As the last step of a care job, the same update runs for the products of the job's repositories (the
website's product selection applies, too), and several repositories feeding one website are updated
together in one step, after their releases. The resulting local commit is the proposal: the approval
view first names the affected products with their version change, Claude's summary and what approving or
rejecting does, then the technical preview with the diff (cut above 300 KiB, with the `git show` command
for the full view). After approval the app pushes, uploads and verifies. Rejecting leaves the commit
unpublished in the website project.

**Produkt zur Website hinzufügen** (add product to the website, button in the Website area): pick one of
your GitHub repositories; "Produktseite erstellen" has Claude Code add it to the site with the project's
`addProductSkill` (product page, listings, icon, diagram, `site.json` entry), then the app builds and
commits. It goes live with "Veröffentlichen".

The Website area lists the products from `site.json` with a checkbox each. The check and the content
update only handle selected products: the check does not look up the others, and the update names them to
Claude as "not selected, leave unchanged". The selection is stored in the app's settings, not in the website
repository; it does not change which repositories the app manages.

## Changed from earlier versions

- The managed repositories are the app's own list, chosen from the local clones. Product repositories of
  `site.json` no longer appear by themselves. The start view is **Repositories**.
- Pull request care, cleanup and release are chosen separately; merging no longer implies a patch tag,
  and a tag needs a release rule set up in the app (the `tagPrefix` of `site.json` alone is not enough).
- Care runs as stored jobs in **Aufträge** ("Läufe" up to 0.3.4): the plan is approved there and survives a restart.
- Local branches are only deleted with GitHub's confirmation, and not after a failed update of the clone.
- A failed release lookup stops the website's content update (and **Aktualisieren und veröffentlichen**)
  instead of counting as "nothing new".
- From 0.3.0: a tag needs CI that positively passed for exactly the tagged commit; repositories without CI
  get no automatic tag.
- From 0.4.0: GitLab projects (gitlab.com and self-hosted instances, sign-in through `glab`); the start
  view shows one card per service. See [GitLab](#gitlab).
- From 0.4.1: GitHub and GitLab also accept an own token instead of `gh` or `glab`, kept in the keychain
  or in the app (**Einstellungen → Zugänge**); self-hosted GitLab instances also over http. The empty
  `gitlab.com` entry `glab` always writes no longer counts as an instance.
- From 0.3.5: "Einrichtung" is now **Einstellungen**, "Läufe" is **Aufträge**, and the website steps are
  Prüfen, Inhalte aktualisieren, Veröffentlichen and Live-Seite prüfen. "Verbindung testen" (test
  connection) no longer saves anything; only "Sichern" (save) applies the settings, including a server
  key confirmed while testing. An invalid cost limit (0, negative, not an amount) is refused instead of
  silently removing the limit; only an empty field removes it. A branch that is already gone when it is
  to be deleted counts as done and no longer makes a job "partially successful". A finished job is removed
  with "Auftragsverlauf löschen …" (delete job history) after a confirmation.

## Choosing repositories

**Repositories auswählen** lists the Git clones directly inside the repo folder whose `origin` is a
GitHub repository, with their path, and marks those that are managed already, a product of the website,
or the website's own repository. Clones of a wiki are left out (wikis are content targets), and a
repository cloned twice is listed once, with the folder named like it. Another folder can be picked for
one selection. A repository is taken
as its clone's origin names it, with the clone as its local copy; GitHub's name and id are added when
GitHub can be asked (a private repository without `gh` login is added from its origin alone). For a
repository without a local clone, type `owner/name`; it must exist on GitHub.

- **Repo folder** (Settings): where the selection looks for clones and where new clones are made; by
  default the parent of the website project or `~/Documents/Code`.
- **Website repository** (Settings, optional): pick it from the list; the project folder follows from its
  local clone. If there is none, the app clones it into the repo folder.

## Installation

Download `OpenReleasePilot-<version>.pkg` from the
[releases](https://github.com/bl0rb/OpenReleasePilot/releases) (Apple silicon) and check it against the
`.sha256` file next to it:

```bash
shasum -a 256 -c OpenReleasePilot-*.pkg.sha256
```

The package installs `OpenReleasePilot.app` to `/Applications` and replaces an installed version. It is
not signed, so macOS blocks it on the first open; allow it once under **System Settings → Privacy &
Security → Open Anyway**, or install it in the terminal:

```bash
sudo installer -pkg OpenReleasePilot-0.4.1.pkg -target /
```

The same package can be distributed through Microsoft Intune as a "macOS app (PKG)"; then turn off
"Beim Start nach neuen Versionen suchen" in the settings.

Up to version 0.1.3 the app was called OpenFTPUpdater-AI. OpenReleasePilot does not replace
`OpenFTPUpdater-AI.app`; delete it after the switch (settings and password are taken over, see below).

## App updates

As in ClaudeKeyZapper: at start, the app quietly looks up the latest release of this repository on
GitHub once; **Nach Updates suchen** in the footer checks manually. If a newer version exists, a banner
appears at the top. **Installieren** downloads the `.pkg` from `github.com`, checks it against the SHA-256
digest GitHub publishes for the release asset and against the release's minisign signature
(`<package>.minisig`, verified with the public key built into the app,
[desktop/update-key.pub](desktop/update-key.pub)), and opens the macOS installer; without a digest or a
valid signature the update is refused; restart the app afterwards. Betas (`X.Y.Z-beta.N`) are only offered through **Beta-Version
suchen** or when a beta is installed. Development builds do not check.

## Requirements

- macOS (developed on macOS 27; the code is cross-platform Tauri, Windows is untested)
- On the machine: `git` with access to your repositories, [Claude Code](https://claude.com/claude-code)
  signed in, the GitHub CLI `gh` signed in for care jobs (it also raises the GitHub API limit from 60 to
  5000 requests per hour; the token is used by the app, and handed to the website build as
  `GITHUB_TOKEN`, but never to Claude Code), and for GitLab projects the GitLab CLI `glab` signed in
  (`brew install glab`, then `glab auth login --hostname <instance>`). Instead of the CLIs, each service
  also takes an own token under **Einstellungen → Zugänge**
- Only for a website: Node.js/npm (or whatever its build needs) and an SFTP account for the web space

The app starts from the Finder and asks the login shell for the user's `PATH` once, so tools
installed with nvm, Homebrew or into `~/.local/bin` are found.

## Website project file `.openreleasepilot/site.json`

Lives in the website repository and describes how to build and what to watch. Projects set up
before the app was renamed from OpenFTPUpdater-AI keep their `.openftpupdater/site.json`; it is read as
long as the new file does not exist.

```json
{
  "name": "example.com",
  "url": "https://example.com",
  "build": "npm run build",
  "output": "dist",
  "addProductSkill": "product-add",
  "tasks": [
    { "id": "texts", "label": "Texts", "skill": "site-texts", "effort": "high" },
    { "id": "screenshots", "label": "Screenshots", "skill": "site-screenshots", "effort": "medium" }
  ],
  "failOnOutput": ["[releases]"],
  "products": [
    { "id": "app", "name": "App", "repo": "owner/app", "tagPrefix": "v", "documented": "1.2.0", "localPath": "../app" }
  ],
  "claude": {
    "model": "opus",
    "effort": "high",
    "maxBudgetUsd": 15,
    "allowedTools": ["Bash(node scripts/screenshots/*)"],
    "disallowedTools": []
  }
}
```

| Field | Meaning |
|---|---|
| `url` | Public URL; the build output root is served there (used by Precheck and Verify) |
| `build`, `output` | Build command (run with `/bin/sh -c` in the project) and its output folder (a subfolder of the project: no absolute path, no `..`) |
| `tasks` | Update tasks, offered as checkboxes in the app; each has `id`, `label`, `skill` (`.claude/skills/<name>/SKILL.md`), optional `description`, `default` (selected unless turned off, default `true`), `model` and `effort` |
| `skill` | Single update skill for sites without `tasks` |
| `addProductSkill` | Claude Code skill that adds a repository as a new product |
| `failOnOutput` | Build output lines that fail the build even with exit code 0, e.g. a warning that versions fell back to stale values |
| `products[].repo` | GitHub repository whose latest release (no pre-releases) is watched |
| `products[].documented` | Release the site content describes; the skill raises it after updating |
| `products[].tagPrefix` | `v` for tags like `v1.2.0`, empty for `1.2.0` |
| `products[].localPath` | Local clone, relative to the project; Claude reads it through `git -C`, never edits it |
| `claude.model`, `claude.effort` | Default model alias (`opus`, `sonnet`, `haiku`, `fable`) and effort (`low` … `max`) for every Claude run |
| `claude.maxBudgetUsd` | Cost limit per Claude run (`--max-budget-usd`) |
| `claude.allowedTools` | Added to the app's allowlist (see below); CLI flags and an unrestricted `Bash` are rejected |

The skill is the actual know-how: where the product texts live, how screenshots are made,
which statements must never be made. Claude reads it on every update.

## Update tasks and models

Each selected task runs as its own Claude Code session, in the order of `tasks`, with its own skill,
model and effort; the app commits once at the end. Small task skills keep each run focused, and rules
that apply to all of them (sources, style, what must never be claimed) belong in the project's
`CLAUDE.md`, which Claude Code reads on every run. In the app, every task has a checkbox and a model
choice; both are remembered.

The model is a Claude Code alias that points to the newest model of its family, as far as the
installed Claude Code knows it. So the app runs `claude update` before Claude runs (at most every
six hours); the log shows the version and, at the start of every run, the exact model:

| Alias | Model (Oct 2026) | API price per 1M tokens (input / output) | Use |
|---|---|---|---|
| `opus` | Claude Opus 5.5 | $4 / $20 | Default: public product claims in two languages need accuracy |
| `sonnet` | Claude Sonnet 5.5 | $2 / $10 | Worth trying for mechanical tasks such as screenshots and diagrams |
| `haiku` | Claude Haiku 5.5 | see Anthropic's price list | Small, clearly scripted tasks |
| `fable` | Claude Fable 5.1 | $10 / $50 | Only if Opus results fall short, e.g. for a new product page |

Effort is the first lever: `high` for texts and the consistency check, `medium` for screenshots and
diagrams. With a Claude subscription login, the cost shown per run is a notional API price; the runs
count against the subscription's usage limits instead.

Care jobs use the model chosen for the job (a content target can set its own), else the default under
**Einstellungen → Allgemein → Ausführungsregeln**, else the website's `claude.model`, else Claude Code's
default; the cost limit per run likewise. The cost limit is an amount in USD above 0 (comma or point); an
empty field means no limit of the app's own.

## What Claude may do

Every Claude Code run is headless (`claude -p`), so it cannot ask; whatever is not allowed is denied, and
denied calls are listed after the run. Each kind of run has its own rights:

- **Pull request review:** read only: the clone, `gh pr view|diff|checks|list`, `gh release`, reading
  `gh api` calls, the web for release notes. No edits, no `gh pr merge|close|comment`, no write calls.
  The app merges, closes and comments.
- **Merge request review (GitLab):** read only as well: `glab mr view|diff|list`, `glab release`, reading
  `glab api projects/…` calls; no `glab mr merge|close|note|approve|update|rebase`, no `glab api` with a
  method or fields.
- **Content (README, documentation, wiki):** reading and editing files in the app's work folder (the
  target's clone) and reading the pinned sources beside it; skills. No shell, no web, nothing in `.git`,
  `.github`, `.gitlab` or `.gitlab-ci.yml`. What counts is decided afterwards by the app (see Content targets).
- **Website update:** as follows.

For the website, Claude Code runs as `claude -p … --permission-mode acceptEdits` in the project folder:

- read, search, edit and write files in the project; use skills
- read-only git in the project and in other repositories via `git -C <path> log|show|diff|tag|describe|ls-tree|fetch --tags`
- `gh release list|view`, `grep`, `ls`, the project's build command
- whatever `claude.allowedTools` in site.json adds

Always denied: `git push`, `git commit`, `git reset`, `git checkout`, `rm`. Committing, pushing
and uploading are the app's job. Denied tool calls are listed after the run, so the allowlist can
be extended deliberately.

Claude gets no `GITHUB_TOKEN`. After every run the app reloads `site.json` and stops before build,
commit and upload if `build`, `output` or `claude.allowedTools`/`disallowedTools` changed, so a run
cannot widen its own rights; adding products is not affected.

## Upload

- The password (or key passphrase) is stored in the system keychain, never in a file or repository.
  SSH keys are supported as well.
- The server's host key is confirmed once (`SHA256:…`, compare with `ssh-keyscan -p 22 host | ssh-keygen -lf -`);
  if it changes later, every connection stops. "Verbindung testen" in the settings tests the dialog's
  entries without saving them; a key confirmed there is only stored with "Sichern".
- The app keeps a manifest of the last upload (path, size, SHA-256) per target in its data folder.
  It uploads only new or changed files, assets before pages, each to a temporary name and then
  renamed, so visitors never get half-written files. It deletes only files that it uploaded itself
  before; other files on the server (other domains, `.htaccess`, …) are never touched.
- Only a committed, clean working tree is uploaded, and only if it is not behind `origin`.

Settings live in `~/Library/Application Support/OpenReleasePilot/config.json` (managed repositories,
content targets, website and SFTP settings; no secrets). Next to it: `jobs/` (one file and one log per
care job; the newest 50 finished jobs are kept), `content.json` (what each content target was last
brought to) and `work/` (the work folders of open jobs, removed when a job is finished). On the first
start after the rename the app takes over the folder `OpenFTPUpdater-AI` and the SFTP password saved
under the old name; macOS may ask once whether OpenReleasePilot may use that keychain entry.

## Building

```bash
cd desktop
npm ci
npm run build
```

`scripts/build-pkg.sh` builds the installer as CI does: `dist/OpenReleasePilot-<version>.pkg` with a
`.sha256` file (`VERSION=0.2.0 scripts/build-pkg.sh`). `SIGN_IDENTITY` (Developer ID Application) and
`INSTALLER_IDENTITY` (Developer ID Installer) sign app and package; without them the app is ad-hoc signed
and the package unsigned.

## Continuous integration and releases

- `.github/workflows/ci.yml`: every pull request and every push to `main` runs the core tests and the
  release build (`build.yml`, macOS 15, Apple silicon).
- `.github/workflows/release.yml`: a tag `X.Y.Z` (or `X.Y.Z-beta.N` for a pre-release) runs the same
  build with that version, signs the `.pkg` with minisign and publishes a GitHub release with the
  `.pkg`, its checksum and its `.minisig`. Publishing only starts after tests and build are green, and
  needs the repository secret `MINISIGN_SECRET_KEY` (the secret key matching `desktop/update-key.pub`);
  without it the workflow stops instead of publishing an unsigned installer.
- All actions are pinned to commit SHAs (version as a comment); Dependabot proposes updates for actions,
  crates and the Tauri CLI every Monday.

```bash
git tag 0.2.0 && git push origin 0.2.0
```

The app ends up in `desktop/target/release/bundle/macos/OpenReleasePilot.app` (ad-hoc signed).
During development: `npm run dev`. Tests: `cargo test --workspace`; tests that read GitHub or run
Claude (a few cents) are marked `ignored` and run with `cargo test -p openreleasepilot-core -- --ignored`;
none of them writes to a real repository.

`bundle.macOS.minimumSystemVersion` is deliberately unset: with `MACOSX_DEPLOYMENT_TARGET=14.0`,
Rust 1.94 on macOS 27 produced proc-macro libraries that fail to load in release builds
("mis-aligned LINKEDIT string pool").

```
desktop/core       Rust library: config, repositories and their health, care rules, content targets,
                   jobs, site.json, shell/PATH, git, GitHub, Claude, SFTP, manifest, verify
desktop/src-tauri  Tauri app: state, commands, job passes, the website's four steps
desktop/ui         Web UI (HTML, JS, CSS), no build step
```

## License

[MIT](LICENSE) © 2026 bl0rb
