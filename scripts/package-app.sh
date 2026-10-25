#!/usr/bin/env bash
# Zip the already staged and signed dist/Shizuku.app into dist/Shizuku-<version>.app.zip
# plus a .sha256 file. Never modifies the app; safe to run repeatedly.
# Usage: scripts/package-app.sh
set -euo pipefail

cd "$(dirname "$0")/.."
APP=dist/Shizuku.app

fail() {
  echo "package-app: $*" >&2
  exit 1
}

[[ -d "$APP" ]] || fail "$APP not found (run stage-app.sh and sign-app.sh first)"

version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")
zip=Shizuku-$version.app.zip

cd dist
rm -f "$zip" "$zip.sha256"
ditto -c -k --keepParent Shizuku.app "$zip"
shasum -a 256 "$zip" > "$zip.sha256"
echo "packaged dist/$zip"
