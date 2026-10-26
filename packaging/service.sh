#!/bin/sh
# Общие функции start.sh/stop.sh: поиск службы и супервизора по pid-файлам.
MUSIKALICE_DIR=/data/opt/musik-alice
MUSIKALICE_EXE=$(readlink -f "$MUSIKALICE_DIR/musikalice")
MUSIKALICE_PIDFILE=/run/musikalice.pid
MUSIKALICE_SUPERVISOR_PIDFILE=/run/musikalice-supervisor.pid
# Циклы ожидания считают десятые доли секунды и прибавляют MUSIKALICE_TICK за паузу: если в BusyBox
# нет usleep и дробного sleep, сроки сохраняются при паузах в секунду.
# shellcheck disable=SC2034
if command -v usleep >/dev/null 2>&1; then
    MUSIKALICE_TICK=1
    pause_briefly() { usleep 100000; }
elif sleep 0.1 2>/dev/null; then
    MUSIKALICE_TICK=1
    pause_briefly() { sleep 0.1; }
else
    MUSIKALICE_TICK=10
    pause_briefly() { sleep 1; }
fi
# Pid живого процесса именно нашего бинарника (защита от переиспользованных pid).
live_pid() {
    [ -f "$1" ] || return 1
    pid=$(cat "$1" 2>/dev/null) || return 1
    case "$pid" in ''|*[!0-9]*) return 1 ;; esac
    [ "$(readlink "/proc/$pid/exe" 2>/dev/null || true)" = "$MUSIKALICE_EXE" ] || return 1
    printf '%s\n' "$pid"
}
service_pid() { live_pid "$MUSIKALICE_PIDFILE"; }
supervisor_pid() {
    pid=$(live_pid "$MUSIKALICE_SUPERVISOR_PIDFILE") || return 1
    tr '\000' '\n' <"/proc/$pid/cmdline" | grep -Fxq -- 'supervise' || return 1
    printf '%s\n' "$pid"
}
