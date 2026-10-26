#!/bin/sh
set -eu
MUSIKALICE_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
MUSIKALICE_CACHE=${MUSIKALICE_CACHE:-"${XDG_CACHE_HOME:-$HOME/.cache}/musik-alice"}
MUSIKALICE_TOOLCHAIN=${MUSIKALICE_TOOLCHAIN:-"$MUSIKALICE_CACHE/gcc-linaro-6.5.0-2018.12-x86_64_arm-linux-gnueabihf"}
MUSIKALICE_ALSA_SYSROOT=${MUSIKALICE_ALSA_SYSROOT:-"$MUSIKALICE_CACHE/alsa-sysroot-glibc223"}
if [ ! -x "$MUSIKALICE_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc" ] || [ ! -f "$MUSIKALICE_ALSA_SYSROOT/.musikalice-alsa-glibc223-no-versioned" ]; then
    echo 'Run scripts/bootstrap-arm.sh first' >&2
    exit 1
fi
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER="$MUSIKALICE_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc"
export CC_armv7_unknown_linux_gnueabihf="$MUSIKALICE_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc"
export AR_armv7_unknown_linux_gnueabihf="$MUSIKALICE_TOOLCHAIN/bin/arm-linux-gnueabihf-ar"
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$MUSIKALICE_ALSA_SYSROOT"
export PKG_CONFIG_LIBDIR="$MUSIKALICE_ALSA_SYSROOT/usr/lib/pkgconfig"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_RUSTFLAGS="-C target-cpu=${MUSIKALICE_TARGET_CPU:-cortex-a53} -C link-arg=-Wl,--gc-sections -C link-arg=-Wl,-z,max-page-size=4096"
cd "$MUSIKALICE_ROOT"
cargo build --locked --release --target armv7-unknown-linux-gnueabihf
"$MUSIKALICE_ROOT/scripts/check-arm-abi.sh" "${CARGO_TARGET_DIR:-target}/armv7-unknown-linux-gnueabihf/release/musikalice"
