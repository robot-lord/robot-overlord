#!/usr/bin/env bash
# Install (or reinstall) the rlord systemd user unit (Linux).
#   scripts/install-systemd.sh            # install + enable + start
#   scripts/install-systemd.sh uninstall  # stop + disable + remove
# The unit runs `rlord` from your PATH (e.g. ~/.local/bin/rlord), so a new
# binary there is picked up by the next restart. Without the unit, rlord
# starts its daemon on demand; the unit just makes systemd the owner.
set -euo pipefail
cd "$(dirname "$0")/.."

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

bin=$(command -v rlord) || { echo "rlord not on PATH (see README quickstart)" >&2; exit 1; }
log="${XDG_STATE_HOME:-$HOME/.local/state}/robot-overlord/rlordd.log"
mkdir -p "$dir" "$(dirname "$log")"
# Stop a daemon started on demand first: it holds the instance lock.
systemctl --user stop "$unit" 2>/dev/null || true
pkill -f '(^|/)rlord daemon$' 2>/dev/null || true
sed -e "s|^ExecStart=.*|ExecStart=$bin daemon|" \
  -e "s|^StandardOutput=.*|StandardOutput=append:$log|" \
  -e "s|^StandardError=.*|StandardError=append:$log|" \
  -e "s|^Environment=RUST_LOG=info|Environment=RUST_LOG=info\nEnvironment=\"PATH=$PATH\"|" \
  deploy/rlord.service >"$dest"
systemctl --user daemon-reload
systemctl --user enable --now "$unit"
echo "installed $unit (bin: $bin, log: $log)"
