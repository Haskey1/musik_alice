#!/bin/sh
# Запускает ARM-бинарник под qemu-user: ловит сборки, которые линкуются, но не стартуют.
set -eu
MUSIKALICE_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
MUSIKALICE_CACHE=${MUSIKALICE_CACHE:-"${XDG_CACHE_HOME:-$HOME/.cache}/musik-alice"}
MUSIKALICE_TOOLCHAIN=${MUSIKALICE_TOOLCHAIN:-"$MUSIKALICE_CACHE/gcc-linaro-6.5.0-2018.12-x86_64_arm-linux-gnueabihf"}
MUSIKALICE_ALSA_SYSROOT=${MUSIKALICE_ALSA_SYSROOT:-"$MUSIKALICE_CACHE/alsa-sysroot-glibc223"}
MUSIKALICE_QEMU=${MUSIKALICE_QEMU:-qemu-arm}
MUSIKALICE_BINARY=${1:-"$MUSIKALICE_ROOT/${CARGO_TARGET_DIR:-target}/armv7-unknown-linux-gnueabihf/release/musikalice"}
MUSIKALICE_OUTPUT=$(mktemp)
MUSIKALICE_HOME=$(mktemp -d)
trap 'rm -rf "$MUSIKALICE_OUTPUT" "$MUSIKALICE_HOME"' EXIT HUP INT TERM

run() {
    "$MUSIKALICE_QEMU" -L "$MUSIKALICE_TOOLCHAIN/arm-linux-gnueabihf/libc" \
        -E "LD_LIBRARY_PATH=$MUSIKALICE_ALSA_SYSROOT/usr/lib" \
        -E "MUSIKALICE_BASE=$MUSIKALICE_HOME" -E "MUSIKALICE_CONTROL=$MUSIKALICE_HOME/c.sock" \
        "$MUSIKALICE_BINARY" "$@" >"$MUSIKALICE_OUTPUT" 2>&1
}
expect() {
    if ! grep -Fq -- "$1" "$MUSIKALICE_OUTPUT"; then
        echo "Smoke test: expected '$1' in output:" >&2
        cat "$MUSIKALICE_OUTPUT" >&2
        exit 1
    fi
}

MUSIKALICE_VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$MUSIKALICE_ROOT/Cargo.toml" | head -n 1)
run version
expect "musikalice $MUSIKALICE_VERSION"
run help
expect 'play ЗАПРОС'
run profile list
expect 'mini1'
expect 'mini2'
run config set server 192.168.1.5:8787
run config show
expect 'http://192.168.1.5:8787'
if run no-such-command; then
    echo 'Smoke test: an unknown command was accepted' >&2
    exit 1
fi
expect 'неизвестная команда'
echo "ARM smoke test passed: musikalice $MUSIKALICE_VERSION starts under $MUSIKALICE_QEMU"
