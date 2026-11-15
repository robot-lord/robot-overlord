#!/bin/sh
# Dev loop: rebuild the binary and restart the daemon on it. The logic is
# `cargo xtask dev-daemon` (xtask/src/dev_daemon.rs); this keeps the old
# command working.
cd "$(dirname "$0")/.." && exec cargo xtask dev-daemon "$@"
