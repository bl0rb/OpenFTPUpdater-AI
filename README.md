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
  "skill": "site-update",
  "failOnOutput": ["[releases]"],
  "products": [
    { "id": "app", "name": "App", "repo": "owner/app", "tagPrefix": "v", "documented": "1.2.0", "localPath": "../app" }
  ],
  "claude": {
    "allowedTools": ["Bash(node scripts/screenshots/*)"],
    "disallowedTools": [],
    "model": null
  }
}
```

| Field | Meaning |
|---|---|
| `url` | Public URL; the build output root is served there (used by Precheck and Verify) |
| `build`, `output` | Build command (run with `/bin/sh -c` in the project) and its output folder |
| `skill` | Claude Code skill in the project (`.claude/skills/<name>/SKILL.md`) that knows how to update the site |
| `failOnOutput` | Build output lines that fail the build even with exit code 0, e.g. a warning that versions fell back to stale values |
| `products[].repo` | GitHub repository whose latest release (no pre-releases) is watched |
| `products[].documented` | Release the site content describes; the skill raises it after updating |
| `products[].tagPrefix` | `v` for tags like `v1.2.0`, empty for `1.2.0` |
| `products[].localPath` | Local clone, relative to the project; Claude reads it through `git -C`, never edits it |
| `claude.allowedTools` | Added to the app's allowlist (see below) |

The skill is the actual know-how: where the product texts live, how screenshots are made,
which statements must never be made. Claude reads it on every update.

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
