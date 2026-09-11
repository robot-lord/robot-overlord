#!/usr/bin/env bash
# Dev loop: rebuild the binary and restart the daemon on it.
# (`cargo test` does not refresh target/debug/rlord — always build first.)
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --workspace
if pid=$(pgrep -x rlord); then
  kill "$pid" && sleep 0.3
fi
log=${RLORD_LOG:-/tmp/rlordd.log}
nohup ./target/debug/rlord daemon >>"$log" 2>&1 &
sleep 0.5
./target/debug/rlord ping && echo "daemon restarted (log: $log)"
