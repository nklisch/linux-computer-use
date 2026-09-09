#!/bin/sh
# Install for the current user; never change system packages or desktop permissions.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
exec cargo install --path "$root" --locked --root "${LCU_INSTALL_ROOT:-$HOME/.local}"
