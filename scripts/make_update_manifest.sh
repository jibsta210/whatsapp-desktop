#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 7 ]]; then
  echo "usage: $0 BINARY CHANNEL VERSION BUILD DOWNLOAD_URL PRIVATE_KEY OUTPUT_JSON" >&2
  exit 2
fi

BINARY=$1
CHANNEL=$2
VERSION=$3
BUILD=$4
DOWNLOAD_URL=$5
PRIVATE_KEY=$6
OUTPUT_JSON=$7

case "$CHANNEL" in
  stable|canary) ;;
  *) echo "channel must be stable or canary" >&2; exit 2 ;;
esac

[[ -f "$BINARY" ]] || { echo "binary not found: $BINARY" >&2; exit 2; }
[[ -f "$PRIVATE_KEY" ]] || { echo "private key not found: $PRIVATE_KEY" >&2; exit 2; }
[[ "$BUILD" =~ ^[0-9]+$ ]] || { echo "build must be numeric" >&2; exit 2; }
[[ "$DOWNLOAD_URL" == https://* ]] || { echo "download URL must use HTTPS" >&2; exit 2; }

SHA256=$(sha256sum "$BINARY" | awk '{print $1}')
SIZE=$(stat -c '%s' "$BINARY")
PUBLISHED_AT=$(date -u +'%Y-%m-%dT%H:%M:%SZ')
PAYLOAD=$(printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s' \
  1 "$CHANNEL" linux-x86_64 "$VERSION" "$BUILD" "$PUBLISHED_AT" \
  "$DOWNLOAD_URL" "$SHA256" "$SIZE")

PAYLOAD_FILE=$(mktemp)
SIGNATURE_FILE=$(mktemp)
trap 'rm -f "$PAYLOAD_FILE" "$SIGNATURE_FILE"' EXIT
printf '%s' "$PAYLOAD" > "$PAYLOAD_FILE"
openssl pkeyutl -sign -rawin -inkey "$PRIVATE_KEY" \
  -in "$PAYLOAD_FILE" -out "$SIGNATURE_FILE"
SIGNATURE=$(base64 -w0 < "$SIGNATURE_FILE")

jq -n \
  --arg channel "$CHANNEL" \
  --arg version "$VERSION" \
  --arg published_at "$PUBLISHED_AT" \
  --arg url "$DOWNLOAD_URL" \
  --arg sha256 "$SHA256" \
  --arg signature "$SIGNATURE" \
  --argjson build "$BUILD" \
  --argjson size "$SIZE" \
  '{
    schema: 1,
    channel: $channel,
    platform: "linux-x86_64",
    version: $version,
    build: $build,
    published_at: $published_at,
    url: $url,
    sha256: $sha256,
    size: $size,
    signature: $signature
  }' > "$OUTPUT_JSON"
