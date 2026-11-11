#!/usr/bin/env bash
# Download a release of rlord from GitHub and install it to ~/.local/bin (via
# the install.sh inside the release). Needs the GitHub CLI logged in to an
# account that can see the (private) repo. No compiling.
#
#   scripts/update.sh [--force] [TAG]   # TAG: e.g. v0.4.0 (default: latest)
#
# `rlord update` runs this same script (compiled into the binary), so after
# the first install you don't need the checkout. --force reinstalls the same
# version and replaces a dev symlink at ~/.local/bin/rlord. After installing a
# new version it runs `rlord reset` if rlord is running.
set -euo pipefail
repo=ayersb/robot-overlord
force=()
tag=
for a in "$@"; do
  case $a in
    --force) force=(--force) ;;
    -*) echo "unknown option $a" >&2; exit 2 ;;
    *) tag=$a ;;
  esac
done

[[ "$(uname -sm)" == "Darwin arm64" ]] || {
  echo "rlord releases are built for Apple Silicon Macs only; build from source instead (cargo build --release)" >&2
  exit 1
}
command -v gh >/dev/null || { echo "needs the GitHub CLI: brew install gh && gh auth login" >&2; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "the GitHub CLI isn't logged in: run gh auth login" >&2; exit 1; }

if [[ -z $tag ]]; then
  tag=$(gh release view -R "$repo" --json tagName -q .tagName)
fi
dest=${RLORD_INSTALL_DIR:-$HOME/.local/bin}/rlord
current=$("$dest" --version 2>/dev/null | awk '{print $2}' || true)
# Reset a running rlord onto the installed binary; a failed reset doesn't
# undo the install.
reset_running() {
  if "$dest" ping >/dev/null 2>&1; then
    echo "reloading rlord on the new version…"
    "$dest" reset || echo "installed, but the reset failed: run \`rlord reset\`" >&2
  fi
}

if [[ -n $current && "v$current" == "$tag" && ${#force[@]} -eq 0 ]]; then
  echo "rlord $current is already the latest release"
  # …but a running daemon may still be an older one (an earlier update
  # that didn't reset): move it over.
  running=$("$dest" ping 2>/dev/null | sed -n 's/.*daemon v\([^)]*\)).*/\1/p')
  if [[ -n $running && $running != "$current" ]]; then
    reset_running
  fi
  exit 0
fi
echo "rlord ${current:-(not installed)} -> ${tag#v}"

d=$(mktemp -d)
trap 'rm -rf "$d"' EXIT
gh release download "$tag" -R "$repo" -p '*-aarch64-apple-darwin.tar.gz' -D "$d"
tar -xzf "$d"/*.tar.gz -C "$d"
installer=("$d"/rlord-*/install.sh)
# ${a[@]+...}: an empty array under `set -u` is an error in macOS's bash 3.2.
"${installer[0]}" ${force[@]+"${force[@]}"}

# A new version was installed: if rlord is running, reset it onto the new
# binary (daemon, list and tmux config). Agent sessions keep running.
reset_running
