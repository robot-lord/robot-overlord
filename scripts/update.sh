#!/usr/bin/env bash
# Download a release of rlord from GitHub and install it to ~/.local/bin (via
# the install.sh inside the release). Needs the GitHub CLI logged in to an
# account that can see the (private) repo. No compiling.
#
#   scripts/update.sh [--force] [TAG]   # TAG: e.g. v0.4.0 (default: latest)
#
# `rlord update` runs this same script (compiled into the binary), so after
# the first install you don't need the checkout. --force reinstalls the same
# version and replaces a dev symlink at ~/.local/bin/rlord.
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
if [[ -n $current && "v$current" == "$tag" && ${#force[@]} -eq 0 ]]; then
  echo "rlord $current is already the latest release"
  exit 0
fi
echo "rlord ${current:-(not installed)} -> ${tag#v}"

d=$(mktemp -d)
trap 'rm -rf "$d"' EXIT
gh release download "$tag" -R "$repo" -p '*-aarch64-apple-darwin.tar.gz' -D "$d"
tar -xzf "$d"/*.tar.gz -C "$d"
"$d"/rlord-*/install.sh "${force[@]}"

# Running daemons restart on the new binary at the next rlord command; an
# open list keeps running the old one until reset.
if "$dest" ping >/dev/null 2>&1; then
  echo "run \`rlord reset\` to reload the open rlord UI on the new version"
fi
