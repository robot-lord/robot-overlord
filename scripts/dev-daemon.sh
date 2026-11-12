#!/usr/bin/env bash
# Dev loop: rebuild the binary and restart the daemon on it.
# (`cargo test` does not refresh target/debug/rlord — always build first.)
# If a service manager owns the daemon (the launchd agent from
# scripts/install-launchd.sh on macOS, the systemd user unit from
# scripts/install-systemd.sh on Linux), we restart it there; otherwise we run
# it under nohup.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --workspace

# Mirror rlord_core::paths::runtime_dir().
if [[ -n "${XDG_RUNTIME_DIR:-}" ]]; then
  run_dir="$XDG_RUNTIME_DIR/robot-overlord"
else
  run_dir="${XDG_DATA_HOME:-$HOME/.local/share}/robot-overlord/run"
fi
pidfile="$run_dir/daemon.pid"
old=""
if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
  old=$(cat "$pidfile")
fi

wait_new() {
  for _ in $(seq 1 50); do
    new=$(cat "$pidfile" 2>/dev/null || true)
    [[ -n "$new" && "$new" != "$old" ]] && kill -0 "$new" 2>/dev/null && return 0
    sleep 0.1
  done
  return 1
}

# Mirror rlord_daemon::autostart::log_path().
if [[ "$(uname -s)" == Darwin ]]; then
  default_log="$HOME/Library/Logs/rlordd.log"
else
  default_log="${XDG_STATE_HOME:-$HOME/.local/state}/robot-overlord/rlordd.log"
fi

target="gui/$(id -u)/robot-overlord.daemon"
if [[ "$(uname -s)" == Darwin ]] && launchctl print "$target" >/dev/null 2>&1; then
  log=$default_log
  launchctl kickstart -k "$target"
elif [[ "$(uname -s)" != Darwin ]] && command -v systemctl >/dev/null &&
  [[ "$(systemctl --user show rlord.service -p LoadState --value 2>/dev/null)" == loaded ]]; then
  log=$default_log
  systemctl --user restart rlord.service
else
  if [[ -z "$old" ]]; then
    # No usable pidfile: find a process whose argv is exactly `[.../]rlord daemon`
    # (anchored, so it never matches the shell running this script).
    old=$(pgrep -f '(^|/)rlord daemon$' | head -1 || true)
  fi
  if [[ -n "$old" ]]; then
    kill "$old"
    for _ in $(seq 1 30); do kill -0 "$old" 2>/dev/null || break; sleep 0.1; done
    kill -0 "$old" 2>/dev/null && { echo "old daemon $old did not exit" >&2; exit 1; }
  fi
  # Launch through the PATH symlink when it points at this build, so the paths
  # the daemon embeds in hooks/tmux bindings are the stable ones.
  bin=./target/debug/rlord
  if linked=$(command -v rlord 2>/dev/null) && [[ "$(readlink -f "$linked")" == "$(readlink -f "$bin")" ]]; then
    bin=$linked
  fi
  log=${RLORD_LOG:-$default_log}
  mkdir -p "$(dirname "$log")"
  nohup "$bin" daemon >>"$log" 2>&1 &
fi

wait_new || { echo "daemon failed to start (see $log)" >&2; tail -3 "$log" >&2; exit 1; }
./target/debug/rlord ping >/dev/null && echo "daemon restarted (pid $new, log: $log)"
