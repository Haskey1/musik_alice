#!/bin/sh
set -eu
MUSIKALICE_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$MUSIKALICE_ROOT"
binary=${1:-"${CARGO_TARGET_DIR:-target}/armv7-unknown-linux-gnueabihf/release/musikalice"}
[ -f "$binary" ] || { echo "Binary not found: $binary" >&2; exit 1; }
mkdir -p dist
stage=$(mktemp -d "$MUSIKALICE_ROOT/dist/package.XXXXXX")
trap 'rm -rf "$stage"' EXIT HUP INT TERM
mkdir -p "$stage/musikalice/packaging" "$stage/musikalice/scripts" "$stage/musikalice/profiles"
cp profiles/*.json "$stage/musikalice/profiles/"
cp "$binary" "$stage/musikalice/musikalice"
cp README.md LICENSE NOTICE "$stage/musikalice/"
cp packaging/* "$stage/musikalice/packaging/"
cp scripts/install.sh "$stage/musikalice/scripts/"
tar -czf dist/musikalice-armv7-linux-gnueabihf.tar.gz -C "$stage" musikalice
cp install.sh dist/install.sh
cd dist
sha256sum install.sh > INSTALL_SHA256SUMS
sha256sum musikalice-armv7-linux-gnueabihf.tar.gz > SHA256SUMS
echo "Package ready: dist/musikalice-armv7-linux-gnueabihf.tar.gz"
