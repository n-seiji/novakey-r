#!/usr/bin/env bash
# Fail if the build could talk to the network: no network crate in Cargo.lock, no network API
# in src/, and (with --binary) no networking framework linked into the release binary.
# Usage: scripts/check-no-network.sh [--binary]
# Env:   LOCK (default Cargo.lock), SRC (default src), BIN (default target/release/novakey_r)
set -euo pipefail

cd "$(dirname "$0")/.."

LOCK=${LOCK:-Cargo.lock}
SRC=${SRC:-src}
BIN=${BIN:-target/release/novakey_r}

fail() {
  echo "check-no-network: $*" >&2
  exit 1
}

[[ -f "$LOCK" ]] || fail "$LOCK not found"
[[ -d "$SRC" ]] || fail "$SRC not found"

crates=(
  reqwest ureq tokio hyper hyper-util h2 curl curl-sys isahc surf attohttpc minreq
  native-tls rustls openssl openssl-sys hf-hub tokio-native-tls tokio-rustls
)
for crate in "${crates[@]}"; do
  if grep -qx "name = \"$crate\"" "$LOCK"; then
    fail "network crate in Cargo.lock: $crate"
  fi
done

apis=('std::net' 'TcpStream' 'UdpSocket' 'TcpListener' 'NSURLSession' 'NSURLConnection' 'CFNetwork' 'CFStream')
for api in "${apis[@]}"; do
  if grep -rqF -- "$api" "$SRC"; then
    fail "network API in source: $api"
  fi
done

if [[ "${1:-}" == "--binary" ]]; then
  [[ -f "$BIN" ]] || fail "$BIN not found (run mise run build first)"
  libs=$(otool -L "$BIN") || fail "otool failed on $BIN"
  # otool exits 0 on a file that is not a Mach-O binary, so also require a library list.
  grep -q 'compatibility version' <<<"$libs" || fail "otool found no linked libraries in $BIN"
  pattern='CFNetwork|Network\.framework|libcurl|libssl|libcrypto'
  if grep -qiE "$pattern" <<<"$libs"; then
    grep -iE "$pattern" <<<"$libs" >&2
    fail "network library linked into $BIN"
  fi
fi

echo "check-no-network: ok"
