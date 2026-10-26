#!/bin/sh
# Команда `musikalice` на станции: start/stop/restart идут через init-скрипт,
# остальное — в бинарник. Командам управления нужна запущенная служба, её поднимаем сами.
set -eu
[ "$#" -gt 0 ] || set -- tui
MUSIKALICE_BASE=/data/opt/musik-alice
export MUSIKALICE_BASE
case "$1" in
    start|stop|restart)
        exec /etc/init.d/S99musik-alice "$1"
        ;;
    tui|play|radio|mix|pause|resume|toggle|next|previous|volume|like|dislike|voice|retry|release|status|quit)
        if [ "$1" != quit ] && [ "$1" != status ]; then
            "$MUSIKALICE_BASE/start.sh" >/dev/null
        fi
        ;;
esac
exec "$MUSIKALICE_BASE/musikalice" "$@"
