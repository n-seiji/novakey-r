#!/usr/bin/env bash
# Check that Cargo.toml, the built app's Info.plist and (optionally) a release tag agree.
# Usage: scripts/check-version.sh [<tag>]   e.g. scripts/check-version.sh v0.1.0
set -euo pipefail

cd "$(dirname "$0")/.."

fail() {
  echo "check-version: $*" >&2
  exit 1
}

version=$(cargo metadata --no-deps --format-version 1 |
  python3 -I -c 'import json,sys; print(json.load(sys.stdin)["packages"][0]["version"])')

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "Cargo version $version is not X.Y.Z"

plist=output/NovakeyR.app/Contents/Info.plist
[[ -f "$plist" ]] || fail "$plist not found (run mise run app first)"

for key in CFBundleShortVersionString CFBundleVersion; do
  actual=$(plutil -extract "$key" raw -o - "$plist" 2>/dev/null) || fail "$key missing in $plist"
  [[ "$actual" == "$version" ]] || fail "$key $actual does not match $version"
done

if [[ $# -ge 1 ]]; then
  [[ "$1" == "v$version" ]] || fail "tag $1 does not match $version"
fi

echo "check-version: ok ($version)"
