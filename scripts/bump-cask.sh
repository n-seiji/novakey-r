#!/usr/bin/env bash
# Download NovakeyR-<version>.app.zip and its published .sha256 from the v<version> release,
# verify the zip against it and against the build attestation, then rewrite version and sha256
# in the cask. The default cask is the template in this repo; pass the tap's file to update it.
# Usage: scripts/bump-cask.sh <version> [path/to/Casks/novakey-r.rb]
# Env:   REPO (default n-seiji/novakey-r)
#        RELEASE_DIR  for testing only: take the zip and .sha256 from this directory instead of
#                     the release, and skip the attestation check
set -euo pipefail
version="${1:?usage: bump-cask.sh <version> [cask.rb]}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "bump-cask: version must look like 1.2.3" >&2; exit 1; }
root="$(cd "$(dirname "$0")/.." && pwd)"
cask="${2:-$root/packaging/homebrew/novakey-r.rb}"
repo="${REPO:-n-seiji/novakey-r}"
[ -f "$cask" ] || { echo "bump-cask: $cask not found" >&2; exit 1; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
zip="NovakeyR-$version.app.zip"
if [ -n "${RELEASE_DIR:-}" ]; then
  echo "warning: RELEASE_DIR set; attestation not verified" >&2
  cp "$RELEASE_DIR/$zip" "$RELEASE_DIR/$zip.sha256" "$tmp/"
else
  gh release download "v$version" --repo "$repo" --pattern "$zip" --pattern "$zip.sha256" --dir "$tmp"
fi

sum="$(shasum -a 256 "$tmp/$zip" | cut -d' ' -f1)"
published="$(cut -d' ' -f1 "$tmp/$zip.sha256")"
[ "$sum" = "$published" ] || { echo "bump-cask: sha256 of $zip ($sum) does not match the published $published" >&2; exit 1; }

if [ -z "${RELEASE_DIR:-}" ]; then
  if gh attestation verify --help >/dev/null 2>&1; then
    gh attestation verify "$tmp/$zip" --repo "$repo" >/dev/null \
      || { echo "bump-cask: attestation verification failed for $zip" >&2; exit 1; }
    echo "attestation verified"
  else
    echo "note: this gh has no 'attestation' command, so the build attestation was not checked" >&2
  fi
fi

out="$tmp/cask.rb"
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in
    '  version "'*) line="  version \"$version\"" ;;
    '  sha256 "'*) line="  sha256 \"$sum\"" ;;
  esac
  printf '%s\n' "$line"
done < "$cask" > "$out"
cat "$out" > "$cask"
echo "$cask: version $version sha256 $sum"
