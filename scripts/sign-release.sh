#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <SHA256SUMS> <Ed25519-private-key.pem>" >&2
  exit 2
fi

checksums=$1
key=$2
root=$(cd "$(dirname "$0")/.." && pwd)
expected=$(tr -d '\n\r' < "$root/assets/update-signing-key.hex")
# Export only the public DER (12-byte Ed25519 header followed by 32 bytes).
public=$(openssl pkey -in "$key" -pubout -outform DER | xxd -p -c 256)
if [[ "$public" != "302a300506032b6570032100$expected" ]]; then
  echo 'release signing key does not match the pinned updater public key' >&2
  exit 1
fi

signature=$(mktemp "${checksums}.sig.XXXXXX")
trap 'rm -f -- "$signature"' EXIT
openssl pkeyutl -sign -rawin -inkey "$key" -in "$checksums" -out "$signature"
test "$(wc -c < "$signature" | tr -d ' ')" = 64
mv "$signature" "${checksums}.sig"
