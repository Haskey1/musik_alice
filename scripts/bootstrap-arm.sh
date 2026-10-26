#!/bin/sh
# Готовит кросс-сборку под станции: компилятор Linaro (glibc 2.23, как на Mini 1)
# и alsa-lib 1.1.8 без версионирования символов.
set -eu

MUSIKALICE_CACHE=${MUSIKALICE_CACHE:-"${XDG_CACHE_HOME:-$HOME/.cache}/musik-alice"}
MUSIKALICE_TOOLCHAIN_NAME=gcc-linaro-6.5.0-2018.12-x86_64_arm-linux-gnueabihf
MUSIKALICE_TOOLCHAIN=${MUSIKALICE_TOOLCHAIN:-"$MUSIKALICE_CACHE/$MUSIKALICE_TOOLCHAIN_NAME"}
MUSIKALICE_ALSA_SYSROOT=${MUSIKALICE_ALSA_SYSROOT:-"$MUSIKALICE_CACHE/alsa-sysroot-glibc223"}
MUSIKALICE_JOBS=${MUSIKALICE_JOBS:-4}
mkdir -p "$MUSIKALICE_CACHE/downloads"

download() {
    archive=$1
    url=$2
    checksum=$3
    if [ ! -f "$archive" ]; then
        curl --fail --location --retry 3 --output "$archive.part" "$url"
        mv "$archive.part" "$archive"
    fi
    printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check --status
}

if [ ! -x "$MUSIKALICE_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc" ]; then
    download "$MUSIKALICE_CACHE/downloads/$MUSIKALICE_TOOLCHAIN_NAME.tar.xz" \
        "https://developer.arm.com/-/cdn-downloads/permalink/legacy-linaro-gnu-toolchains/6.5-2018.12/$MUSIKALICE_TOOLCHAIN_NAME.tar.xz" \
        a84b1239765617f3192014cb259c05e32771a64a4c831cf89577fcd87918eacd
    mkdir -p "$MUSIKALICE_TOOLCHAIN"
    tar -xJf "$MUSIKALICE_CACHE/downloads/$MUSIKALICE_TOOLCHAIN_NAME.tar.xz" --strip-components=1 -C "$MUSIKALICE_TOOLCHAIN"
fi

if [ ! -f "$MUSIKALICE_ALSA_SYSROOT/.musikalice-alsa-glibc223-no-versioned" ]; then
    download "$MUSIKALICE_CACHE/downloads/alsa-lib-1.1.8.tar.bz2" \
        https://www.alsa-project.org/files/pub/lib/alsa-lib-1.1.8.tar.bz2 \
        3cdc3a93a6427a26d8efab4ada2152e64dd89140d981f6ffa003e85be707aedf
    work=$(mktemp -d "$MUSIKALICE_CACHE/alsa-build.XXXXXX")
    trap 'rm -rf "$work"' EXIT HUP INT TERM
    tar -xjf "$MUSIKALICE_CACHE/downloads/alsa-lib-1.1.8.tar.bz2" -C "$work"
    (
        cd "$work/alsa-lib-1.1.8"
        CC="$MUSIKALICE_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc" \
        AR="$MUSIKALICE_TOOLCHAIN/bin/arm-linux-gnueabihf-ar" \
        ./configure --host=arm-linux --prefix=/usr --disable-python --disable-static --without-versioned
        make -j "$MUSIKALICE_JOBS"
        make DESTDIR="$MUSIKALICE_ALSA_SYSROOT" install
    )
    touch "$MUSIKALICE_ALSA_SYSROOT/.musikalice-alsa-glibc223-no-versioned"
    rm -rf "$work"
    trap - EXIT HUP INT TERM
fi

printf 'Toolchain: %s\nALSA sysroot: %s\n' "$MUSIKALICE_TOOLCHAIN" "$MUSIKALICE_ALSA_SYSROOT"
