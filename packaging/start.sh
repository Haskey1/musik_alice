#!/bin/sh
set -eu
umask 077
# shellcheck source=packaging/service.sh
. /data/opt/musik-alice/service.sh
if MUSIKALICE_PID=$(supervisor_pid); then
    echo "musik-alice already running (supervisor pid $MUSIKALICE_PID)"
    exit 0
fi
if service_pid >/dev/null; then
    "$MUSIKALICE_DIR/stop.sh"
fi
MUSIKALICE_BASE=$MUSIKALICE_DIR
export MUSIKALICE_BASE
nohup setsid "$MUSIKALICE_DIR/musikalice" supervise </dev/null >>/tmp/musik-alice.log 2>&1 &
# Холодный старт с медленной флэш-памяти может занять несколько секунд.
MUSIKALICE_WAIT=0
until MUSIKALICE_PID=$(supervisor_pid); do
    if [ "$MUSIKALICE_WAIT" -ge 50 ]; then
        echo 'musik-alice supervisor failed; see /tmp/musik-alice.log' >&2
        exit 1
    fi
    MUSIKALICE_WAIT=$((MUSIKALICE_WAIT + MUSIKALICE_TICK))
    pause_briefly
done
echo "musik-alice started (supervisor pid $MUSIKALICE_PID)"
