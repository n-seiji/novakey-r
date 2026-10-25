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

crates=(
  reqwest ureq hyper hyper-util h2 curl curl-sys isahc surf attohttpc minreq
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
  if otool -L "$BIN" | grep -qiE 'CFNetwork|Network\.framework|libcurl|libssl|libcrypto'; then
    otool -L "$BIN" | grep -iE 'CFNetwork|Network\.framework|libcurl|libssl|libcrypto' >&2
    fail "network library linked into $BIN"
  fi
fi

echo "check-no-network: ok"
