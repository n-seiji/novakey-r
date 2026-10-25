#!/usr/bin/env bash
# Copy output/Shizuku.app to dist/Shizuku.app and check the bundle is complete:
# the judge model files (verified against tools/judge.sha256), the licenses and the dictionary.
# Usage: scripts/stage-app.sh
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD
SRC=output/Shizuku.app
DST=dist/Shizuku.app

fail() {
  echo "stage-app: $*" >&2
  exit 1
}

[[ -d "$SRC" ]] || fail "$SRC not found (run mise run app first)"

rm -rf "$DST"
mkdir -p dist
cp -R "$SRC" "$DST"

res=$DST/Contents/Resources
missing=()
for f in judge/model_q8_0.gguf judge/tokenizer.json LICENSE NOTICE THIRD_PARTY_LICENSES dictionary; do
  [[ -e "$res/$f" ]] || missing+=("$f")
done
if ((${#missing[@]} > 0)); then
  fail "missing ${missing[*]}"
fi

(cd "$res" && shasum -a 256 -c "$ROOT/tools/judge.sha256") >&2 \
  || fail "judge files do not match tools/judge.sha256"

echo "staged $DST"
