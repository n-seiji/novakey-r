#!/usr/bin/env bash
# Sign dist/Shizuku.app with the hardened runtime and verify it.
# Usage: scripts/sign-app.sh <identity>   (identity "-" = ad-hoc, no timestamp)
set -euo pipefail

cd "$(dirname "$0")/.."
APP=dist/Shizuku.app

fail() {
  echo "sign-app: $*" >&2
  exit 1
}

(($# == 1)) || fail "usage: scripts/sign-app.sh <identity>"
identity=$1
[[ -d "$APP" ]] || fail "$APP not found (run scripts/stage-app.sh first)"

args=(--force --options runtime)
if [[ "$identity" != "-" ]]; then
  args+=(--timestamp)
fi
codesign "${args[@]}" --sign "$identity" "$APP"
codesign --verify --strict --deep --verbose=2 "$APP"
