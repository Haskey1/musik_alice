#!/bin/sh
set -eu
umask 077
MUSIKALICE_BASE=/data/opt/musik-alice
export MUSIKALICE_BASE
exec "$MUSIKALICE_BASE/musikalice" "$@"
