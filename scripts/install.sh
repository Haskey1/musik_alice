#!/bin/sh
# Устанавливает распакованный пакет на станцию. Запускать от root на самой колонке:
#   sh scripts/install.sh
# Файлы кладутся в /data/opt/musik-alice, настройки и токен сохраняются между обновлениями.
set -eu
MUSIKALICE_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
MUSIKALICE_BASE=/data/opt/musik-alice
MUSIKALICE_INIT=/etc/init.d/S99musik-alice
MUSIKALICE_CLI=/usr/bin/musikalice
fail() { echo "musikalice: $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || fail 'Запустите установщик от root на колонке.'
[ -x "$MUSIKALICE_ROOT/musikalice" ] || fail "В пакете нет исполняемого файла: $MUSIKALICE_ROOT/musikalice"
"$MUSIKALICE_ROOT/musikalice" version >/dev/null || fail 'Бинарник не запускается на этой прошивке.'

# Корневая ФС станции обычно только для чтения: на время установки включаем запись.
MUSIKALICE_REMOUNTED=0
if grep -q ' / [^ ]* ro[ ,]' /proc/mounts; then
    mount -o remount,rw / || fail 'Не удалось перемонтировать / для записи.'
    MUSIKALICE_REMOUNTED=1
fi
restore_root() {
    if [ "$MUSIKALICE_REMOUNTED" -eq 1 ]; then
        mount -o remount,ro / 2>/dev/null || true
    fi
}
trap restore_root EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

WAS_RUNNING=0
if [ -x "$MUSIKALICE_BASE/start.sh" ]; then
    # shellcheck source=packaging/service.sh
    if (. "$MUSIKALICE_BASE/service.sh" && supervisor_pid >/dev/null); then
        WAS_RUNNING=1
    fi
    "$MUSIKALICE_BASE/stop.sh" || true
fi

mkdir -p "$MUSIKALICE_BASE/profiles" "$MUSIKALICE_BASE/state"
chmod 700 "$MUSIKALICE_BASE/state"
# Предыдущая версия остаётся рядом на случай отката.
if [ -f "$MUSIKALICE_BASE/musikalice" ]; then
    cp "$MUSIKALICE_BASE/musikalice" "$MUSIKALICE_BASE/musikalice.prev"
fi

put() { # источник назначение режим
    cp "$1" "$2.new"
    chmod "$3" "$2.new"
    mv "$2.new" "$2"
}
put "$MUSIKALICE_ROOT/musikalice" "$MUSIKALICE_BASE/musikalice" 755
for f in run.sh start.sh stop.sh service.sh cli.sh; do
    put "$MUSIKALICE_ROOT/packaging/$f" "$MUSIKALICE_BASE/$f" 755
done
for f in "$MUSIKALICE_ROOT"/profiles/*.json; do
    put "$f" "$MUSIKALICE_BASE/profiles/$(basename "$f")" 644
done
put "$MUSIKALICE_ROOT/packaging/S99musik-alice" "$MUSIKALICE_INIT" 755
ln -sf "$MUSIKALICE_BASE/cli.sh" "$MUSIKALICE_CLI"

if [ "$WAS_RUNNING" -eq 1 ] || [ ! -f "$MUSIKALICE_BASE/state/config.json" ]; then
    if ! "$MUSIKALICE_BASE/start.sh"; then
        if [ -f "$MUSIKALICE_BASE/musikalice.prev" ]; then
            echo 'Запуск не удался, возвращаю предыдущую версию.' >&2
            put "$MUSIKALICE_BASE/musikalice.prev" "$MUSIKALICE_BASE/musikalice" 755
            "$MUSIKALICE_BASE/start.sh" || true
        fi
        fail 'Служба не запустилась; см. /tmp/musik-alice.log'
    fi
fi

echo "musik-alice установлен в $MUSIKALICE_BASE."
if [ ! -f "$MUSIKALICE_BASE/state/config.json" ]; then
    echo 'Дальше: musikalice login   (адрес сервера и пароль musik)'
fi
