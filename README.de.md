[English](README.md) · **Deutsch**

<img src="desktop/assets/icon.svg" width="96" alt="OpenFTPUpdater-AI-Icon">

# OpenFTPUpdater-AI

Desktop-App, die eine statische Website auf dem Stand der Releases hält, die sie beschreibt,
und sie veröffentlicht: Claude Code aktualisiert die Inhalte, die App baut, committet, pusht und
lädt das Ergebnis per SFTP hoch und prüft danach die Live-Seite Byte für Byte.

Vier Schaltflächen, einzeln oder nacheinander:

| Schritt | Was passiert | Ändert etwas? |
|---|---|---|
| **Precheck** | Projektdatei, Werkzeuge (git, node, npm, claude, gh), sauberes Arbeitsverzeichnis und Remote-Stand, neuestes GitHub-Release jedes beobachteten Produkts gegen den dokumentierten Stand, SFTP-Anmeldung und Zielordner, Live-Seite, letzter Upload | nein |
| **Update** | `git pull --ff-only`, dann läuft Claude Code headless im Projekt mit dem Skill des Projekts und der Liste veralteter Produkte (plus optionaler Zusatzanweisung), danach Build und Commit | Dateien im Projekt, ein Commit |
| **Upload** | Frischer Build, `git push`, dann nur neue oder geänderte Dateien per SFTP; aus dem Build entfernte Dateien werden auf dem Server gelöscht | Server, Remote-Repository |
| **Verify** | Lädt jede Datei des letzten Uploads von der Live-Seite und vergleicht SHA-256 mit dem Build | nein |

**Neues Produkt** (zweiter Knopf in der Update-Kachel): eines deiner GitHub-Repositories wählen, Claude
Code nimmt es mit dem `addProductSkill` des Projekts auf (Produktseite, Listen, Icon, Ablaufgrafik,
Eintrag in `site.json`), danach baut und committet die App. Live geht es mit Upload.

## Repositories wählen

Die App listet deine GitHub-Repositories über die angemeldete `gh`-CLI, auch private, und markiert
jedes: lokaler Klon gefunden, schon auf der Seite, Website-Repository. Ohne `gh`-Anmeldung zeigt sie die
öffentlichen Repositories der Besitzer, die schon auf der Seite stehen.

- **Website-Repo** (Einrichtung): aus der Liste wählen; der Projektordner ergibt sich aus dem lokalen
  Klon. Gibt es keinen, klont die App das Repo in den Repo-Ordner.
- **Repo-Ordner** (Einrichtung): hier werden lokale Klone gesucht und angelegt; standardmäßig der
  Elternordner des Projektordners. Dort gefundene Produkt-Klone bekommt Claude als `localPath`.

## Voraussetzungen

- macOS (entwickelt unter macOS 27; der Code ist plattformübergreifendes Tauri, Windows ist ungetestet)
- Auf dem Rechner: `git`, Node.js/npm (für den Build der Seite), angemeldetes [Claude Code](https://claude.com/claude-code), empfohlen die angemeldete GitHub-CLI `gh` (hebt das GitHub-API-Limit von 60 auf 5000 Anfragen pro Stunde; das Token geht auch als `GITHUB_TOKEN` an den Build)
- Ein SFTP-Zugang zum Webspace

Die App startet aus dem Finder und fragt einmal die Login-Shell nach dem `PATH`, damit
Werkzeuge aus nvm, Homebrew oder `~/.local/bin` gefunden werden.

## Projektdatei `.openftpupdater/site.json`

Liegt im Website-Repository und beschreibt Build und beobachtete Produkte:

```json
{
  "name": "example.com",
  "url": "https://example.com",
  "build": "npm run build",
  "output": "dist",
  "skill": "site-update",
  "addProductSkill": "product-add",
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

| Feld | Bedeutung |
|---|---|
| `url` | Öffentliche Adresse; dort liegt die Wurzel der Build-Ausgabe (für Precheck und Verify) |
| `build`, `output` | Build-Befehl (läuft mit `/bin/sh -c` im Projekt) und sein Ausgabeordner |
| `skill` | Claude-Code-Skill im Projekt (`.claude/skills/<name>/SKILL.md`), der weiß, wie die Seite aktualisiert wird |
| `addProductSkill` | Claude-Code-Skill, der ein Repository als neues Produkt aufnimmt |
| `failOnOutput` | Zeilen der Build-Ausgabe, die den Build trotz Exit-Code 0 scheitern lassen, z. B. eine Warnung, dass Versionen auf alte Werte zurückgefallen sind |
| `products[].repo` | GitHub-Repository, dessen neuestes Release (ohne Pre-Releases) beobachtet wird |
| `products[].documented` | Release, den die Inhalte beschreiben; der Skill setzt ihn nach dem Update hoch |
| `products[].tagPrefix` | `v` bei Tags wie `v1.2.0`, leer bei `1.2.0` |
| `products[].localPath` | Lokaler Klon relativ zum Projekt; Claude liest ihn über `git -C`, ändert ihn nie |
| `claude.allowedTools` | Ergänzt die Freigabeliste der App (siehe unten) |

Das eigentliche Wissen steckt im Skill: wo die Produkttexte stehen, wie Screenshots entstehen,
welche Aussagen nie gemacht werden dürfen. Claude liest ihn bei jedem Update.

## Was Claude darf

Claude Code läuft als `claude -p … --permission-mode acceptEdits` im Projektordner. Ein
Headless-Lauf kann nicht nachfragen, alles außerhalb der Freigabeliste wird abgelehnt:

- Dateien im Projekt lesen, durchsuchen, bearbeiten und schreiben; Skills nutzen
- git nur lesend, im Projekt und in anderen Repositories über `git -C <Pfad> log|show|diff|tag|describe|ls-tree|fetch --tags`
- `gh release list|view`, `grep`, `ls`, der Build-Befehl des Projekts
- was `claude.allowedTools` in site.json ergänzt

Immer abgelehnt: `git push`, `git commit`, `git reset`, `git checkout`, `rm -rf`. Committen,
pushen und hochladen macht die App. Abgelehnte Aufrufe stehen nach dem Lauf in der Übersicht,
damit die Freigabeliste bewusst erweitert werden kann.

## Upload

- Das Passwort (oder die Passphrase eines Schlüssels) liegt im Schlüsselbund des Systems, nie in
  einer Datei oder im Repository. SSH-Schlüssel werden ebenfalls unterstützt.
- Der Server-Schlüssel wird einmal bestätigt (`SHA256:…`, vergleichen mit
  `ssh-keyscan -p 22 host | ssh-keygen -lf -`); ändert er sich später, bricht jede Verbindung ab.
- Die App merkt sich je Ziel ein Manifest des letzten Uploads (Pfad, Größe, SHA-256) in ihrem
  Datenordner. Hochgeladen werden nur neue oder geänderte Dateien, Assets vor Seiten, jeweils unter
  einem temporären Namen und dann umbenannt, damit Besucher nie halbe Dateien bekommen. Gelöscht
  werden nur Dateien, die die App selbst hochgeladen hat; andere Dateien auf dem Server (andere
  Domains, `.htaccess`, …) bleiben unberührt.
- Hochgeladen wird nur ein committeter, sauberer Stand, und nur wenn er nicht hinter `origin` liegt.

Die Einstellungen liegen in `~/Library/Application Support/OpenFTPUpdater-AI/config.json` (ohne Geheimnisse).

## Bauen

```bash
cd desktop
npm ci
npm run build
```

Die App liegt danach unter `desktop/target/release/bundle/macos/OpenFTPUpdater-AI.app`
(ad-hoc signiert). Während der Entwicklung: `npm run dev`. Tests des Kerns:
`cargo test -p openftpupdater-core`.

`bundle.macOS.minimumSystemVersion` ist bewusst nicht gesetzt: Mit
`MACOSX_DEPLOYMENT_TARGET=14.0` erzeugte Rust 1.94 unter macOS 27 im Release-Build
Proc-Macro-Bibliotheken, die sich nicht laden ließen („mis-aligned LINKEDIT string pool“).

```
desktop/core       Rust-Bibliothek: Einstellungen, site.json, Shell/PATH, git, GitHub, Claude, SFTP, Manifest, Verify
desktop/src-tauri  Tauri-App: Zustand, Befehle, die vier Schritte
desktop/ui         Web-Oberfläche (HTML, JS, CSS), ohne Build-Schritt
```

## Lizenz

[MIT](LICENSE) © 2026 bl0rb
