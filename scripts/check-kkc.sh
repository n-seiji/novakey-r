#!/usr/bin/env bash
# Fail unless the Rust engine matches the Swift golden data on every stable row
# (top1 / top3 / top9 all 100%). Needs target/release/shizuku (mise run build).
# Usage: scripts/check-kkc.sh [<golden.jsonl.gz>...]   (default: tools/golden/fresh.jsonl.gz)
#
# tools/golden/typed.jsonl.gz is not in the default set: it is already below 100% on main
# (top3 98.27%, top9 97.22%), before this check existed. Pass it explicitly to look at it.
set -euo pipefail

cd "$(dirname "$0")/.."

BIN=${BIN:-target/release/shizuku}
[[ -x "$BIN" ]] || { echo "check-kkc: $BIN not found (run mise run build first)" >&2; exit 1; }

goldens=("$@")
[[ ${#goldens[@]} -gt 0 ]] || goldens=(tools/golden/fresh.jsonl.gz)

status=0
for golden in "${goldens[@]}"; do
  # --kkc-compare exits non-zero below 100% top3; the stable line below decides.
  out=$(SHIZUKU_DICTIONARY="$PWD/dictionary" "$BIN" --kkc-compare --golden "$golden") || true
  line=$(grep '^stable' <<<"$out" || true)
  [[ -n "$line" ]] || { echo "check-kkc: no stable line for $golden" >&2; exit 1; }
  echo "$golden: $line"
  for k in top1 top3 top9; do
    if ! grep -Eq "$k +[0-9]+ \(100\.00%\)" <<<"$line"; then
      echo "check-kkc: $golden $k is below 100%" >&2
      status=1
    fi
  done
done
exit $status
