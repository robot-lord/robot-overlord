#!/usr/bin/env bash
# Install (or reinstall) the rlordd launchd user agent.
#   scripts/install-launchd.sh            # install + start
#   scripts/install-launchd.sh uninstall  # stop + remove
# The agent runs `rlord` from your PATH (e.g. the ~/.local/bin dev symlink),
# so rebuilding + `scripts/dev-daemon.sh` picks up new code.
set -euo pipefail
cd "$(dirname "$0")/.."

label=robot-overlord.daemon
plist="$HOME/Library/LaunchAgents/$label.plist"
domain="gui/$(id -u)"

launchctl bootout "$domain/$label" 2>/dev/null || true
if [[ "${1:-}" == "uninstall" ]]; then
  rm -f "$plist"
  echo "removed $label"
  exit 0
fi

bin=$(command -v rlord) || { echo "rlord not on PATH (see README quickstart)" >&2; exit 1; }
mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
sed -e "s|__BIN__|$bin|g" -e "s|__HOME__|$HOME|g" -e "s|__PATH__|$PATH|g" \
  deploy/robot-overlord.plist >"$plist"
plutil -lint "$plist" >/dev/null
launchctl bootstrap "$domain" "$plist"
echo "installed $label (bin: $bin, log: ~/Library/Logs/rlordd.log)"
