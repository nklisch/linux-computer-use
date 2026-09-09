#!/bin/sh
# Explicit optional companion installation. Never start/restart LCU services.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
prefix=${LCU_INSTALL_ROOT:-$HOME/.local}
build=${LCU_MONITOR_BUILD_DIR:-$root/.local/monitor-build}
cmake -S "$root/monitor" -B "$build" -DBUILD_TESTING=OFF -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$prefix"
cmake --build "$build" --parallel 4
cmake --install "$build"
printf '%s\n' 'Monitor companion installed. No LCU services were started or restarted.'
