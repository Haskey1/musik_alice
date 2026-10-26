#!/bin/sh
set -eu
# shellcheck source=packaging/service.sh
. /data/opt/musik-alice/service.sh
if MUSIKALICE_PARENT=$(supervisor_pid); then
    kill -TERM "$MUSIKALICE_PARENT" 2>/dev/null || true
fi
if MUSIKALICE_CHILD=$(service_pid); then
    kill -TERM "$MUSIKALICE_CHILD" 2>/dev/null || true
fi
MUSIKALICE_WAIT=0
MUSIKALICE_KILLED=0
while supervisor_pid >/dev/null || service_pid >/dev/null; do
    if [ "$MUSIKALICE_WAIT" -ge 100 ]; then
        echo 'musik-alice is still stopping' >&2
        exit 1
    elif [ "$MUSIKALICE_WAIT" -ge 80 ] && [ "$MUSIKALICE_KILLED" -eq 0 ]; then
        # Процесс, застрявший в ALSA, игнорирует SIGTERM.
        echo 'musik-alice did not stop in 8 seconds; killing it' >&2
        if MUSIKALICE_PARENT=$(supervisor_pid); then
            kill -KILL "$MUSIKALICE_PARENT" 2>/dev/null || true
        fi
        if MUSIKALICE_CHILD=$(service_pid); then
            kill -KILL "$MUSIKALICE_CHILD" 2>/dev/null || true
        fi
        MUSIKALICE_KILLED=1
    fi
    MUSIKALICE_WAIT=$((MUSIKALICE_WAIT + MUSIKALICE_TICK))
    pause_briefly
done
rm -f "$MUSIKALICE_PIDFILE" "$MUSIKALICE_SUPERVISOR_PIDFILE" /run/musikalice.control
echo 'musik-alice stopped'
