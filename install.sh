#!/bin/sh
# Установка/обновление musik-alice прямо на станции: скачивает последний релиз с GitHub по HTTPS
# штатным средством прошивки (maind --gst-launch), проверяет SHA-256 и запускает установщик из пакета.
set -eu

MUSIKALICE_MODE=install
case "${1:-}" in
    --check) MUSIKALICE_MODE=check; shift ;;
    --help|-h)
        echo 'Usage: sh install.sh [--check]'
        echo 'Установить или обновить musik-alice; с --check только проверить загрузку и запуск.'
        exit 0
        ;;
esac
[ "$#" -eq 0 ] || { echo 'Usage: sh install.sh [--check]' >&2; exit 1; }
fail() { echo "musikalice: $*" >&2; exit 1; }
[ "$(id -u)" -eq 0 ] || fail 'Запустите установщик от root на колонке.'
for MUSIKALICE_TOOL in uname od tr sha256sum gzip tar mktemp df awk grep mv; do
    command -v "$MUSIKALICE_TOOL" >/dev/null 2>&1 || fail "Не найдена команда: $MUSIKALICE_TOOL"
done
case "$(uname -m)" in
    armv7l|armv8l|aarch64) ;;
    *) fail 'Нужна ARMv7 Linux с 32-битным userspace.' ;;
esac
if [ "$(od -An -tx1 -j4 -N2 /bin/sh | tr -d ' \n')" != 0101 ] ||
    [ "$(od -An -tx1 -j18 -N2 /bin/sh | tr -d ' \n')" != 2800 ]; then
    fail 'Нужен little-endian ARM32 userspace.'
fi

MUSIKALICE_MAIND=/system/vendor/quasar/maind
MUSIKALICE_CA=/etc/ssl/certs/ca-certificates.crt
if [ ! -x "$MUSIKALICE_MAIND" ] || [ ! -r "$MUSIKALICE_CA" ]; then
    fail 'Нужны штатный maind и его CA-бандл; на другой прошивке установите через компьютер (см. README).'
fi
MUSIKALICE_FREE=$(df -Pk /tmp | awk 'END { print $4 }')
case "$MUSIKALICE_FREE" in ''|*[!0-9]*) fail 'Не удалось узнать свободное место в /tmp.' ;; esac
[ "$MUSIKALICE_FREE" -ge 16384 ] || fail 'Нужно минимум 16 МиБ свободного места в /tmp.'
umask 077
MUSIKALICE_WORK=$(mktemp -d /tmp/musikalice-install.XXXXXX)
trap 'rm -rf "$MUSIKALICE_WORK"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

download() {
    LD_LIBRARY_PATH=/system/vendor/quasar/libs "$MUSIKALICE_MAIND" --gst-launch \
        "souphttpsrc location=\"$1\" ssl-strict=true ssl-ca-file=\"$MUSIKALICE_CA\" iradio-mode=false timeout=15 retries=1 ! filesink location=\"$2.part\"" \
        > "$2.log" 2>&1 || fail 'Не удалось скачать файл с GitHub по HTTPS.'
    # Эта штатная утилита возвращает 0 и после ошибок конвейера, поэтому смотрим журнал.
    if ! grep -Fq 'EOS: end of stream' "$2.log" || grep -Fq 'ERROR from ' "$2.log" || [ ! -s "$2.part" ]; then
        fail 'Загрузка не завершилась; проверьте интернет, сертификаты прошивки и системное время.'
    fi
    mv "$2.part" "$2"
}

MUSIKALICE_ASSET=musikalice-armv7-linux-gnueabihf.tar.gz
MUSIKALICE_RELEASE=https://github.com/Haskey1/musik_alice/releases/latest/download
echo 'Скачиваю последний релиз musik-alice с GitHub...'
download "$MUSIKALICE_RELEASE/SHA256SUMS" "$MUSIKALICE_WORK/SHA256SUMS"
download "$MUSIKALICE_RELEASE/$MUSIKALICE_ASSET" "$MUSIKALICE_WORK/$MUSIKALICE_ASSET"
awk -v asset="$MUSIKALICE_ASSET" '
    NF == 2 && ($2 == asset || $2 == "*" asset) { count++; hash = $1 }
    END {
        if (count != 1 || length(hash) != 64 || hash ~ /[^0-9a-fA-F]/) exit 1
        print hash "  " asset
    }
' "$MUSIKALICE_WORK/SHA256SUMS" > "$MUSIKALICE_WORK/expected.sha256" || fail 'Некорректная или дублирующаяся запись контрольной суммы.'
(cd "$MUSIKALICE_WORK" && sha256sum -c expected.sha256) ||
    fail 'Контрольная сумма не совпала, ничего не установлено. Повторите, если релиз только что обновился.'

# У штатного tar нет -z, поэтому gzip отдельно.
gzip -dc "$MUSIKALICE_WORK/$MUSIKALICE_ASSET" > "$MUSIKALICE_WORK/package.tar"
rm "$MUSIKALICE_WORK/$MUSIKALICE_ASSET"
tar -xf "$MUSIKALICE_WORK/package.tar" -C "$MUSIKALICE_WORK"
rm "$MUSIKALICE_WORK/package.tar"
"$MUSIKALICE_WORK/musikalice/musikalice" version || fail 'Бинарник релиза не запускается на этой прошивке.'
if [ "$MUSIKALICE_MODE" = check ]; then
    echo 'Загрузка, контрольная сумма и запуск на ARM в порядке. Ничего не установлено.'
    exit 0
fi
sh "$MUSIKALICE_WORK/musikalice/scripts/install.sh"
echo 'Готово. Дальше: musikalice login'
