#!/bin/sh
# Build and exercise the Qt frontend against Rust's typed fixture, never a desktop.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
cargo build --locked --example monitor_fixture
target=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
build=${LCU_MONITOR_BUILD_DIR:-$root/.local/monitor-build}
cmake -S monitor -B "$build" -DBUILD_TESTING=ON -DCMAKE_BUILD_TYPE=RelWithDebInfo
cmake --build "$build" --parallel 4
LCU_MONITOR_FIXTURE="$target/debug/examples/monitor_fixture" ctest --test-dir "$build" --output-on-failure
