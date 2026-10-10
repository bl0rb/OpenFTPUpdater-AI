**English** · [Deutsch](README.de.md)

<img src="desktop/assets/icon.svg" width="96" alt="OpenFTPUpdater-AI icon">

# OpenFTPUpdater-AI

Desktop app that keeps a static website in step with the releases of the products it
describes, and publishes it: Claude Code updates the content, the app builds, commits,
pushes and uploads the result via SFTP, then checks the live site byte for byte.

Four buttons, run one by one or all in a row:

| Step | What happens | Changes anything? |
|---|---|---|
| **Precheck** | Project file, tools (git, node, npm, claude, gh), clean working tree and remote state, latest GitHub release of every watched product against the documented one, SFTP login and target folder, live site, last upload | no |
| **Update** | `git pull --ff-only`, then Claude Code runs headless in the project with the project's skill and the list of outdated products (plus optional extra instructions), then build and commit | files in the project, one commit |
| **Upload** | Fresh build, `git push`, then only new or changed files via SFTP; files removed from the build are deleted on the server | server, remote repository |
| **Verify** | Downloads every file of the last upload from the live site and compares SHA-256 with the build | no |

![What happens when and where: Precheck, update tasks run by Claude Code with their skills, build and commit, upload via SFTP, verify against the live site](docs/diagrams/openftpupdater-flow.png)

<sub>Diagram source: [docs/diagrams/openftpupdater-flow.html](docs/diagrams/openftpupdater-flow.html) (also as SVG).</sub>

**New product** (second button on the Update tile): pick one of your GitHub repositories, and Claude
Code adds it to the site with the project's `addProductSkill` (product page, listings, icon, diagram,
`site.json` entry), then the app builds and commits. It goes live with Upload.

## Choosing repositories

The app lists your GitHub repositories through the signed-in `gh` CLI, private ones included, and marks
each one: local clone found, already on the site, website repository. Without a `gh` login it falls back
to the public repositories of the owners already on the site.

- **Website repository** (Settings): pick it from the list; the project folder follows from its local
  clone. If there is none, the app clones it into the repo folder.
- **Repo folder** (Settings): where local clones are searched and created; by default the parent of the
  project folder. Product clones found there are handed to Claude as `localPath`.

## Installation

Download `OpenFTPUpdater-AI-<version>-macos-arm64.zip` from the
[releases](https://github.com/bl0rb/OpenFTPUpdater-AI/releases) (Apple silicon), check it against the
`.sha256` file next to it and move the app to `/Applications`:

```bash
shasum -a 256 -c OpenFTPUpdater-AI-*-macos-arm64.zip.sha256
```

The app is ad-hoc signed, not notarized, so macOS blocks the first launch. Allow it once under
**System Settings → Privacy & Security → Open Anyway**.

## Requirements

- macOS (developed on macOS 27; the code is cross-platform Tauri, Windows is untested)
- On the machine: `git`, Node.js/npm (for the site build), [Claude Code](https://claude.com/claude-code) signed in, recommended the GitHub CLI `gh` signed in (raises the GitHub API limit from 60 to 5000 requests per hour; the token is also handed to the build as `GITHUB_TOKEN`)
- An SFTP account for the web space

The app starts from the Finder and asks the login shell for the user's `PATH` once, so tools
installed with nvm, Homebrew or into `~/.local/bin` are found.

## Project file `.openftpupdater/site.json`

Lives in the website repository and describes how to build and what to watch:

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
| `build`, `output` | Build command (run with `/bin/sh -c` in the project) and its output folder |
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
| `claude.allowedTools` | Added to the app's allowlist (see below) |

The skill is the actual know-how: where the product texts live, how screenshots are made,
which statements must never be made. Claude reads it on every update.

## Update tasks and models

Each selected task runs as its own Claude Code session, in the order of `tasks`, with its own skill,
model and effort; the app commits once at the end. Small task skills keep each run focused, and rules
that apply to all of them (sources, style, what must never be claimed) belong in the project's
`CLAUDE.md`, which Claude Code reads on every run. In the app, every task has a checkbox and a model
choice; both are remembered.

The model is a Claude Code alias that always points to the newest model of its family:

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

Always denied: `git push`, `git commit`, `git reset`, `git checkout`, `rm -rf`. Committing, pushing
and uploading are the app's job. Denied tool calls are listed after the run, so the allowlist can
be extended deliberately.

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

Settings live in `~/Library/Application Support/OpenFTPUpdater-AI/config.json` (no secrets).

## Building

```bash
cd desktop
npm ci
npm run build
```

`scripts/build-app.sh` builds the release package as CI does: the app re-signed ad hoc, zipped as
`dist/OpenFTPUpdater-AI-<version>-macos-<arch>.zip` with a `.sha256` file (`VERSION=0.2.0 scripts/build-app.sh`).

## Continuous integration and releases

- `.github/workflows/ci.yml`: every pull request and every push to `main` runs the core tests and the
  release build (`build.yml`, macOS 15, Apple silicon).
- `.github/workflows/release.yml`: a tag `X.Y.Z` (or `X.Y.Z-beta.N` for a pre-release) runs the same
  build with that version and publishes a GitHub release with the zip and its checksum. Publishing
  only starts after tests and build are green.
- All actions are pinned to commit SHAs (version as a comment); Dependabot proposes updates for actions,
  crates and the Tauri CLI every Monday.

```bash
git tag 0.2.0 && git push origin 0.2.0
```

The app ends up in `desktop/target/release/bundle/macos/OpenFTPUpdater-AI.app` (ad-hoc signed).
During development: `npm run dev`. Core tests: `cargo test -p openftpupdater-core`.

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
