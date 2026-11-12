#!/usr/bin/env bash
# Install (or reinstall) the rlord systemd user unit (Linux).
#   scripts/install-systemd.sh            # install + enable + start
#   scripts/install-systemd.sh uninstall  # stop + disable + remove
# The unit runs `rlord` from your PATH (e.g. ~/.local/bin/rlord), so a new
# binary there is picked up by the next restart. Without the unit, rlord
# starts its daemon on demand; the unit just makes systemd the owner.
# deploy/rlord.service is the same unit with default paths.
set -euo pipefail

unit=rlord.service
dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
dest="$dir/$unit"

if [[ "${1:-}" == "uninstall" ]]; then
  systemctl --user disable --now "$unit" 2>/dev/null || true
  rm -f "$dest"
  systemctl --user daemon-reload
  echo "removed $unit"
  exit 0
fi

bin=$(command -v rlord) || { echo "rlord not on PATH (see the README)" >&2; exit 1; }
log="${XDG_STATE_HOME:-$HOME/.local/state}/robot-overlord/rlordd.log"
mkdir -p "$dir" "$(dirname "$log")"
# Stop a daemon started on demand first: it holds the instance lock.
systemctl --user stop "$unit" 2>/dev/null || true
pkill -f '(^|/)rlord daemon$' 2>/dev/null || true

# systemd expands % specifiers: double any % in the values written.
esc() { printf '%s' "${1//%/%%}"; }
{
  printf '%s\n' "# Installed by robot-overlord's scripts/install-systemd.sh"
  printf '%s\n' "[Unit]" "Description=Robot Overlord daemon (rlordd)" ""
  printf '%s\n' "[Service]"
  printf 'ExecStart="%s" daemon\n' "$(esc "$bin")"
  printf '%s\n' "Environment=RUST_LOG=info"
  # The daemon spawns tmux, git and the agent CLIs: give it your PATH.
  printf 'Environment="PATH=%s"\n' "$(esc "$PATH")"
  printf 'StandardOutput=append:%s\n' "$(esc "$log")"
  printf 'StandardError=append:%s\n' "$(esc "$log")"
  # Only the daemon: its tmux server and agent sessions outlive a restart.
  printf '%s\n' "Restart=on-failure" "RestartSec=2" "KillMode=process" ""
  printf '%s\n' "[Install]" "WantedBy=default.target"
} >"$dest"
systemctl --user daemon-reload
systemctl --user enable --now "$unit"
echo "installed $unit (bin: $bin, log: $log)"
