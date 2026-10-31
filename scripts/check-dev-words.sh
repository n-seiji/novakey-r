#!/usr/bin/env bash
# resources/dev_words.txt の全語を 1 語ずつ打って Enter を押し、その語がそのまま確定されることを確かめる。
# 審判は読まない (SHIZUKU_JUDGE_DIR を外す) ので結果は決まっている。
set -euo pipefail
cd "$(dirname "$0")/.."

# main でも外れる語。ここに無い語が外れたらこのスクリプトが失敗する。
# base64: 数字を含む打鍵列は英語判定の対象外 (小文字の英字だけを英語として扱う)。
EXEMPT=" base64 "

scenarios="$(mktemp)"
trap 'rm -f "$scenarios"' EXIT
while read -r word; do
  case "$EXEMPT" in *" $word "*) continue ;; esac
  printf '%s<enter>\t%s\n' "$word" "$word" >>"$scenarios"
done < <(grep -Ev '^(#|$)' resources/dev_words.txt)

unset SHIZUKU_JUDGE_DIR
out="$(SHIZUKU_DICTIONARY="${SHIZUKU_DICTIONARY:-$PWD/dictionary}" cargo run --release --quiet -- --simulate-file "$scenarios")" || {
  printf '%s\n' "$out" | grep -v '^ok'
  exit 1
}
echo "dev_words: $(grep -c . "$scenarios") words ok"
