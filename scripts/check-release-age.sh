#!/usr/bin/env bash
# Fail if Cargo.lock pins a crates.io version published less than MIN_AGE_DAYS ago (a guard like
# pnpm's minimumReleaseAge). Fails closed: a failed lookup is a failure, never a pass.
# Usage: scripts/check-release-age.sh
# Env:   LOCK (default Cargo.lock), MIN_AGE_DAYS (default 7), ALLOW (default tools/release-age-allow.txt),
#        CACHE (default target/release-age-cache.tsv), DELAY (seconds between requests, default 1)
# Tools: curl + jq, both on stock macOS runners and dev machines (jq via mise/brew); no Python needed.
# The allowlist lines are `name version # reason` for emergency exceptions.
set -euo pipefail

cd "$(dirname "$0")/.."

LOCK=${LOCK:-Cargo.lock}
MIN_AGE_DAYS=${MIN_AGE_DAYS:-7}
ALLOW=${ALLOW:-tools/release-age-allow.txt}
CACHE=${CACHE:-target/release-age-cache.tsv}
DELAY=${DELAY:-1}
UA='shizuku-release-age-check (github.com/n-seiji/shizuku-ime)'
REGISTRY='registry+https://github.com/rust-lang/crates.io-index'

fail() {
  echo "check-release-age: $*" >&2
  exit 1
}

[[ -f "$LOCK" ]] || fail "$LOCK not found"
[[ "$MIN_AGE_DAYS" =~ ^[0-9]+$ ]] || fail "MIN_AGE_DAYS must be a non-negative integer"
command -v curl >/dev/null || fail "curl not found"
command -v jq >/dev/null || fail "jq not found"

now=$(date -u +%s)
cutoff=$((now - MIN_AGE_DAYS * 86400))

# "name version" pairs of crates.io packages (the root package and path/git sources have another
# or no source line).
packages=$(awk -v reg="source = \"$REGISTRY\"" '
  /^\[\[package\]\]/ { name = ""; version = "" }
  /^name = /         { gsub(/"/, "", $3); name = $3 }
  /^version = /      { gsub(/"/, "", $3); version = $3 }
  $0 == reg          { print name, version }
' "$LOCK")
[[ -n "$packages" ]] || fail "no crates.io packages found in $LOCK"

allowed() {
  [[ -f "$ALLOW" ]] || return 1
  sed 's/#.*//' "$ALLOW" | awk -v n="$1" -v v="$2" '$1 == n && $2 == v { found = 1 } END { exit !found }'
}

# Prints "<epoch> <created_at>" for name@version, from the cache or crates.io. Only successes are cached.
published() {
  local name=$1 version=$2 hit body created epoch
  if [[ -f "$CACHE" ]]; then
    hit=$(awk -F'\t' -v k="$name@$version" '$1 == k { print $2, $3; exit }' "$CACHE")
    if [[ -n "$hit" ]]; then
      echo "$hit"
      return 0
    fi
  fi
  sleep "$DELAY"
  body=$(curl -fsS --retry 3 --retry-delay 5 --max-time 30 -A "$UA" \
    "https://crates.io/api/v1/crates/$name/$version") || return 1
  created=$(jq -er '.version.created_at' <<<"$body") || return 1
  epoch=$(jq -er '.version.created_at | sub("\\.[0-9]+"; "") | sub("\\+00:00$"; "Z") | fromdateiso8601' <<<"$body") || return 1
  mkdir -p "$(dirname "$CACHE")"
  printf '%s@%s\t%s\t%s\n' "$name" "$version" "$epoch" "$created" >>"$CACHE"
  echo "$epoch $created"
}

bad=0
while read -r name version; do
  if ! res=$(published "$name" "$version"); then
    echo "lookup failed: $name $version"
    bad=1
    continue
  fi
  epoch=${res%% *}
  created=${res#* }
  if ((epoch > cutoff)); then
    if allowed "$name" "$version"; then
      echo "allowed (too new): $name $version published $created"
    else
      echo "too new: $name $version published $created"
      bad=1
    fi
  fi
done <<<"$packages"

[[ "$bad" -eq 0 ]] || fail "dependency versions younger than $MIN_AGE_DAYS days or failed lookups (see above)"
echo "check-release-age: ok ($(wc -l <<<"$packages" | tr -d ' ') crates, min age $MIN_AGE_DAYS days)"
