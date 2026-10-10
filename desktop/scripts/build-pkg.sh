#!/bin/bash
# Builds dist/OpenFTPUpdater-AI-<version>.pkg (Apple silicon) and its .sha256; the package installs
# OpenFTPUpdater-AI.app to /Applications and replaces an installed version (same bundle ID).
#   VERSION             app/package version (default: version in src-tauri/tauri.conf.json)
#   SIGN_IDENTITY       codesign identity (default "-" = ad hoc; "Developer ID Application: …" for release)
#   INSTALLER_IDENTITY  optional "Developer ID Installer: …" to sign the package
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION="${VERSION:-$(node -p "require('./src-tauri/tauri.conf.json').version")}"
IDENTITY="${SIGN_IDENTITY:--}"

mkdir -p target dist
printf '{"version":"%s"}' "$VERSION" > target/version.conf.json
npx tauri build --bundles app --config target/version.conf.json
APP=target/release/bundle/macos/OpenFTPUpdater-AI.app

OPTS=(--force --timestamp=none)
[ "$IDENTITY" != "-" ] && OPTS=(--force --timestamp --options runtime)
codesign "${OPTS[@]}" -s "$IDENTITY" "$APP"
codesign --verify --strict --deep "$APP"

ROOT="$(mktemp -d)"
mkdir -p "$ROOT/Applications"
ditto --noextattr --noqtn "$APP" "$ROOT/Applications/OpenFTPUpdater-AI.app"
COMPONENTS="$(mktemp -d)/components.plist"
pkgbuild --analyze --root "$ROOT" "$COMPONENTS" >/dev/null
# Always install to /Applications, even if another copy with the same bundle ID exists elsewhere.
plutil -replace 0.BundleIsRelocatable -bool NO "$COMPONENTS"

SIGN=()
[ -n "${INSTALLER_IDENTITY:-}" ] && SIGN=(--sign "$INSTALLER_IDENTITY")
NAME="OpenFTPUpdater-AI-$VERSION.pkg"
rm -f "dist/$NAME" "dist/$NAME.sha256"
pkgbuild --root "$ROOT" --component-plist "$COMPONENTS" --install-location / \
  --identifier io.github.bl0rb.openftpupdater.pkg --version "$VERSION" ${SIGN[@]+"${SIGN[@]}"} "dist/$NAME"
(cd dist && shasum -a 256 "$NAME" > "$NAME.sha256")
echo "Built dist/$NAME"
