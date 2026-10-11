**English** · [Deutsch](README.de.md)

<img src="desktop/assets/icon.svg" width="96" alt="OpenReleasePilot icon">

# OpenReleasePilot

Desktop app that keeps a static website in step with the releases of the products it
describes, and publishes it: Claude Code updates the content, the app builds, commits,
pushes and uploads the result via SFTP, then checks the live site byte for byte. A second
area, **Repositories**, shows the state of the products' repositories and takes care of them:
merge reviewed pull requests, tag the next patch version, delete merged branches.

The app has two main areas, switched in the toolbar: **Übersicht** (the website) and
**Repositories**; Settings stay under **Einrichtung**. The overview has four buttons, run one by one
or all in a row:

| Step | What happens | Changes anything? |
|---|---|---|
| **Precheck** | Project file, tools (git, node, npm, claude, gh), clean working tree and remote state, latest GitHub release of every watched product against the documented one, SFTP login and target folder, live site, last upload | no |
| **Update** | `git pull --ff-only`, then Claude Code runs headless in the project with the project's skill and the list of outdated products (plus optional extra instructions), then build and commit | files in the project, one commit |
| **Upload** | Fresh build, `git push`, then only new or changed files via SFTP; files removed from the build are deleted on the server | server, remote repository |
| **Verify** | Downloads every file of the last upload from the live site and compares SHA-256 with the build | no |

![What happens when and where: Precheck, update tasks run by Claude Code with their skills, build and commit, upload via SFTP, verify against the live site](docs/diagrams/openreleasepilot-flow.png)

<sub>Diagram source: [docs/diagrams/openreleasepilot-flow.html](docs/diagrams/openreleasepilot-flow.html) (also as SVG).</sub>

**New product** (button in the toolbar): pick one of your GitHub repositories, and Claude
Code adds it to the site with the project's `addProductSkill` (product page, listings, icon, diagram,
`site.json` entry), then the app builds and commits. It goes live with Upload.

## Selecting products

The main window lists the products from `site.json` with a checkbox each. Precheck and Update only
handle selected products: Precheck does not look up the others, and Update names them to Claude as "not
selected, leave unchanged". Every row shows the documented version, after a Precheck also whether a newer
release exists, and "kein Klon" if `localPath` is missing. The selection is stored in the app's settings,
not in the website repository. It does not hide anything in **Repositories**: turned-off products stay
listed there.

## Repositories

The **Repositories** area lists the repository of every product in `site.json`, each once even when
several products share it (`owner/name`, case-insensitive), including products turned off in the
overview. Each row shows a state in words with its main reason, open pull requests (with how many
come from Dependabot), the CI of the default branch, the latest published release (or, without one, the
highest matching tag, marked as tag), the local clone and when it was last read. The website's own
repository is named in the header.

**Status aktualisieren** reads GitHub only: no Claude, no cost, no write, no `git fetch`. It runs when
the area is first opened and on request, never in the background; a failed read keeps the older state
but marks it as old, and data older than five minutes is shown as old. Without a `gh` login public
repositories can still be read (GitHub then allows 60 requests per hour; a repository needs about eight).

| State | When |
|---|---|
| **Blockiert** | the default branch's CI failed, or the repository cannot be read (GitHub answers 404 or refuses access; a 404 can also mean private without access) |
| **Aufmerksamkeit nötig** | open Dependabot pull requests, commits since the last release or tag, the website documents an older version than the latest release, or (after the detail view read them) pull requests with conflicts or failed CI |
| **Unbekannt** | nothing of the above, but required facts are missing, incomplete or old: CI still running, no checks found, unclear result, query failed |
| **In Ordnung** | everything required was read successfully and recently, and nothing needs doing |

Missing release data alone does not prevent "In Ordnung"; missing or incomplete data never produces it.
An open pull request from a person is no finding by itself, and a red pull request does not block the
repository. Archived repositories and those without write access stay readable, but care is locked.

The detail view (click a row) shows default branch and head commit, CI with its commit, release and tag,
commits since that base (only where it is an ancestor of the head, otherwise "nicht bestimmbar"),
tag prefix, local clone, the products with their documented version, and every open pull request with
target branch, author, CI and mergeability. Only pull requests into the default branch go into care.

## Repository care

Care starts from the detail view (**Dieses Repository prüfen**) or for the repositories of the selected
products (**Ausgewählte pflegen …**, which names them first and leaves out locked ones), through the
signed-in `gh` CLI, in two parts:

1. **Prüfen** reads the state on GitHub (read only, no fetch), then Claude Code judges each open pull
   request into the default branch, read-only (in the local clone if there is one), by the rules of the
   `dependabot-pr` skill: risk class (green, yellow, red; below 1.0 a change of the first non-zero digit
   counts as major), release notes from the upstream repository, only manifest/lock/workflow files in
   Dependabot pull requests, SHA pins of Actions, related dependencies only together, majors checked
   against the actual use in the code. This costs Claude usage, roughly a few cents up to about a dollar
   per repository depending on the model and the number of pull requests.
2. The preview under **Pflege** lists per repository what will happen: merge, close, rebase, tag, clean
   up, left open, each with Claude's risk and reason and what the app checks right before. **Ausführen**
   carries out exactly this preview, **Verwerfen** drops it. A new check, discarding it or switching the
   project drops it too; a change of tag prefix or local clone in `site.json` makes the run refuse it.

The app decides, not Claude: a pull request needs exactly one valid verdict (green or yellow for a merge,
a reason for a close); red, missing, duplicate or invalid verdicts leave it open. Right before each write
the app checks again:

- **Merge:** still open and no draft, same head commit, same target branch and repository, mergeable
  according to GitHub, CI of the head commit passed; GitHub gets the expected head commit, so a push in
  between makes it refuse. Merge commit where the repository allows it.
- **Close:** still open, same head and target; a replacing pull request Claude named must still be open
  or merged. The reason is posted as a comment; comment and close are reported separately.
- **Rebase:** still open, really from Dependabot, same head, still in conflict or behind; then
  `@dependabot rebase`. A Dependabot pull request that a merge of the same run put into conflict gets
  this instead of the merge, as the preview says.
- **Tag:** the next patch version of the newest `x.y.z` tag with the product's `tagPrefix`, only if every
  planned merge of the repository succeeded, the default branch is exactly where this run's merges left
  it, the release line is unchanged, there are commits since the last tag, and the CI of exactly that
  commit passed (the app waits up to 30 minutes). Red, still running, unclear, cancelled or **no checks
  at all** mean no tag. Existing tags are never moved. In repositories with a release workflow the tag
  starts it; the release itself is that workflow's business. The first release is tagged by hand.
- **Delete merged branches** (optional): on GitHub only while the branch is on the same commit, not
  protected, not the default branch, no open pull request comes from or targets it, and its proof still
  holds (all commits in the default branch, or its pull request merged; a head branch of a merge in the
  same run only if that merge succeeded). Locally only in a clone that still belongs to the repository,
  never a branch checked out in any worktree. If the clone's `origin/<default>` differs from GitHub, the
  preview announces `git fetch --prune origin` and the run does it before deleting; only branches named
  in the preview are deleted.

Every action ends as done, skipped, failed or cancelled, with the reason. Done stays done after an error
or a cancel; nothing is rolled back, and the run never tries a write twice (after an unclear answer it
reads the state first). Afterwards the app reads the touched repositories again. Where the GitHub API has
no precondition (closing, deleting a branch), a short window between the last check and the write remains.

**Changed from earlier versions:** a tag now needs CI that positively passed for exactly the tagged commit. Before,
the app tagged after three minutes without any checks; repositories without CI now get no automatic tag.

## Choosing repositories

The app lists your GitHub repositories through the signed-in `gh` CLI, private ones included, and marks
each one: local clone found, already on the site, website repository. Without a `gh` login it falls back
to the public repositories of the owners already on the site.

- **Website repository** (Settings): pick it from the list; the project folder follows from its local
  clone. If there is none, the app clones it into the repo folder.
- **Repo folder** (Settings): where local clones are searched and created; by default the parent of the
  project folder. Product clones found there are handed to Claude as `localPath`.

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
sudo installer -pkg OpenReleasePilot-0.3.0.pkg -target /
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
- On the machine: `git`, Node.js/npm (for the site build), [Claude Code](https://claude.com/claude-code) signed in, recommended the GitHub CLI `gh` signed in (raises the GitHub API limit from 60 to 5000 requests per hour; the token is also handed to the build the app runs as `GITHUB_TOKEN`, but never to Claude Code)
- An SFTP account for the web space

The app starts from the Finder and asks the login shell for the user's `PATH` once, so tools
installed with nvm, Homebrew or into `~/.local/bin` are found.

## Project file `.openreleasepilot/site.json`

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
| `haiku` | Claude Haiku 4.5 | $1 / $5 | Small, clearly scripted tasks |
| `fable` | Claude Fable 5.1 | $10 / $50 | Only if Opus results fall short, e.g. for a new product page |

Effort is the first lever: `high` for texts and the consistency check, `medium` for screenshots and
diagrams. With a Claude subscription login, the cost shown per run is a notional API price; the runs
count against the subscription's usage limits instead.

## What Claude may do

Claude Code runs as `claude -p … --permission-mode acceptEdits` in the project folder. Headless
runs cannot ask, so everything not on the allowlist is denied:

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
  if it changes later, every connection stops.
- The app keeps a manifest of the last upload (path, size, SHA-256) per target in its data folder.
  It uploads only new or changed files, assets before pages, each to a temporary name and then
  renamed, so visitors never get half-written files. It deletes only files that it uploaded itself
  before; other files on the server (other domains, `.htaccess`, …) are never touched.
- Only a committed, clean working tree is uploaded, and only if it is not behind `origin`.

Settings live in `~/Library/Application Support/OpenReleasePilot/config.json` (no secrets). On the
first start after the rename the app takes over the folder `OpenFTPUpdater-AI` and the SFTP password
saved under the old name; macOS may ask once whether OpenReleasePilot may use that keychain entry.

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
During development: `npm run dev`. Core tests: `cargo test -p openreleasepilot-core`.

`bundle.macOS.minimumSystemVersion` is deliberately unset: with `MACOSX_DEPLOYMENT_TARGET=14.0`,
Rust 1.94 on macOS 27 produced proc-macro libraries that fail to load in release builds
("mis-aligned LINKEDIT string pool").

```
desktop/core       Rust library: config, site.json, shell/PATH, git, GitHub, Claude, SFTP, manifest, verify
desktop/src-tauri  Tauri app: state, commands, the four steps
desktop/ui         Web UI (HTML, JS, CSS), no build step
```

## License

[MIT](LICENSE) © 2026 bl0rb
