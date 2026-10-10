#!/bin/bash
# Builds dist/OpenFTPUpdater-AI-<version>-macos-<arch>.zip (the app, ad-hoc signed) and its .sha256.
#   VERSION  app version (default: version in src-tauri/tauri.conf.json)
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION="${VERSION:-$(node -p "require('./src-tauri/tauri.conf.json').version")}"
ARCH="$(uname -m)"

mkdir -p target dist
printf '{"version":"%s"}' "$VERSION" > target/version.conf.json
npx tauri build --bundles app --config target/version.conf.json
APP=target/release/bundle/macos/OpenFTPUpdater-AI.app

codesign --force --deep --timestamp=none -s - "$APP"
codesign --verify --strict --deep "$APP"

NAME="OpenFTPUpdater-AI-$VERSION-macos-$ARCH.zip"
rm -f "dist/$NAME" "dist/$NAME.sha256"
ditto -c -k --keepParent --norsrc --noextattr --noqtn --noacl "$APP" "dist/$NAME"
(cd dist && shasum -a 256 "$NAME" > "$NAME.sha256")
echo "Built dist/$NAME"
