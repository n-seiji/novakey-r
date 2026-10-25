#!/usr/bin/env bash
# Submit each file to Apple's notary service and wait for the verdict. Fails unless every
# submission is Accepted; on failure the notary log is printed. Staple the app yourself afterwards
# (xcrun stapler staple dist/NovakeyR.app), then zip it again with scripts/package-app.sh.
# Usage: scripts/notarize.sh <file.zip|.dmg|.pkg>...
# Env:   ASC_KEY_ID, ASC_ISSUER_ID, ASC_KEY_P8 (path to the AuthKey_<id>.p8 App Store Connect API key)
set -euo pipefail
[ "$#" -gt 0 ] || { echo "usage: notarize.sh <file>..." >&2; exit 2; }
: "${ASC_KEY_ID:?ASC_KEY_ID is required}" "${ASC_ISSUER_ID:?ASC_ISSUER_ID is required}" "${ASC_KEY_P8:?ASC_KEY_P8 is required}"
[ -f "$ASC_KEY_P8" ] || { echo "notarize: $ASC_KEY_P8 not found" >&2; exit 1; }

auth=(--key "$ASC_KEY_P8" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID")
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

for f in "$@"; do
  [ -f "$f" ] || { echo "notarize: $f not found" >&2; exit 1; }
  echo "submitting $f"
  rc=0
  xcrun notarytool submit "$f" "${auth[@]}" --wait --timeout 30m --output-format json >"$tmp/result.json" || rc=$?
  id="$(plutil -extract id raw -o - "$tmp/result.json" 2>/dev/null || true)"
  status="$(plutil -extract status raw -o - "$tmp/result.json" 2>/dev/null || true)"
  echo "notarization id: ${id:-unknown} status: ${status:-unknown} ($f)"
  if [ "$rc" -ne 0 ] || [ "$status" != "Accepted" ]; then
    cat "$tmp/result.json" >&2 || true
    [ -z "$id" ] || xcrun notarytool log "$id" "${auth[@]}" >&2 || true
    echo "notarize: $f was not accepted" >&2
    exit 1
  fi
done
