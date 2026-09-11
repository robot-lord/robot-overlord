#!/usr/bin/env bash
# Dev loop: rebuild the binary and restart the daemon on it.
# (`cargo test` does not refresh target/debug/rlord — always build first.)
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --workspace

run_dir="${XDG_RUNTIME_DIR:-$HOME/.local/share/robot-overlord/run}/robot-overlord"
pidfile="$run_dir/daemon.pid"
old=""
if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
  old=$(cat "$pidfile")
else
  # No usable pidfile: find a process whose argv is exactly `rlord daemon`
  # (never a substring match — that would catch the shell running this script).
  for d in /proc/[0-9]*; do
    if [[ "$(tr '\0' ' ' <"$d/cmdline" 2>/dev/null)" == *"/rlord daemon " ]]; then
      old=${d#/proc/}; break
    fi
  done
fi
if [[ -n "$old" ]]; then
  kill "$old"
  for _ in $(seq 1 30); do kill -0 "$old" 2>/dev/null || break; sleep 0.1; done
  kill -0 "$old" 2>/dev/null && { echo "old daemon $old did not exit" >&2; exit 1; }
fi

log=${RLORD_LOG:-/tmp/rlordd.log}
nohup ./target/debug/rlord daemon >>"$log" 2>&1 &
for _ in $(seq 1 30); do [[ -f "$pidfile" ]] && break; sleep 0.1; done
new=$(cat "$pidfile" 2>/dev/null || true)
[[ -n "$new" && "$new" != "${old:-}" ]] || { echo "daemon failed to start (see $log)" >&2; tail -3 "$log" >&2; exit 1; }
./target/debug/rlord ping >/dev/null && echo "daemon restarted (pid $new, log: $log)"
