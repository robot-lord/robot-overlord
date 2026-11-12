#!/usr/bin/env bash
# Download a release of rlord from GitHub and install it to ~/.local/bin (via
# the install.sh inside the release). No compiling.
#
#   scripts/update.sh [--force] [--channel stable|dev] [TAG]
#
# TAG: e.g. v0.18.0 (default: the newest on the channel). Channels:
#   stable  the newest stable release (X.Y.Z; every platform)
#   dev     the newest release of any kind: a dev build (X.Y.Z-dev.N, macOS
#           only) when it's newer than the latest stable one
# The channel defaults to $RLORD_CHANNEL (`rlord update` sets it from
# `[update] channel` in config.toml), else stable.
#
# `rlord update` runs this same script (compiled into the binary), so after
# the first install you don't need the checkout. Uses the GitHub CLI when
# it's logged in (needed while the repo is private), else plain curl.
# --force reinstalls the same version and replaces a dev symlink at
# ~/.local/bin/rlord. After installing a new version it runs `rlord reset`
# if rlord is running.
set -euo pipefail
repo=ayersb/robot-overlord
force=()
tag=
channel=${RLORD_CHANNEL:-stable}
while [[ $# -gt 0 ]]; do
  case $1 in
    --force) force=(--force) ;;
    --channel) channel=${2:-}; shift ;;
    --channel=*) channel=${1#--channel=} ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) tag=$1 ;;
  esac
  shift
done
[[ $channel == stable || $channel == dev ]] || { echo "channel must be stable or dev" >&2; exit 2; }

case "$(uname -sm)" in
  "Darwin arm64") target=aarch64-apple-darwin ;;
  "Linux x86_64") target=x86_64-unknown-linux-musl ;;
  "Linux aarch64" | "Linux arm64") target=aarch64-unknown-linux-musl ;;
  *) echo "no rlord release for $(uname -sm); build from source (cargo install rlord)" >&2; exit 1 ;;
esac

use_gh=0
if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then use_gh=1; fi

# Releases, newest first: "TAG PRERELEASE" lines.
releases() {
  if [[ $use_gh == 1 ]]; then
    gh release list -R "$repo" --limit 50 --json tagName,isPrerelease \
      -q '.[] | "\(.tagName) \(.isPrerelease)"'
  else
    curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=50" |
      awk -F'"' '/"tag_name":/ {t=$4} /"prerelease":/ {p=($0 ~ /true/) ? "true" : "false"; print t, p}'
  fi
}

# Semver order: 1 if $1 > $2. A pre-release (X.Y.Z-dev.N) sorts below X.Y.Z.
newer() {
  local a=${1#v} b=${2#v}
  local an=${a%%-*} bn=${b%%-*} ap='' bp=''
  [[ $a == *-* ]] && ap=${a#*-}
  [[ $b == *-* ]] && bp=${b#*-}
  if [[ $an != "$bn" ]]; then
    [[ "$(printf '%s\n%s\n' "$an" "$bn" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)" == "$an" ]]
    return
  fi
  [[ -z $ap && -n $bp ]] && return 0
  [[ -n $ap && -z $bp ]] && return 1
  [[ $ap == "$bp" ]] && return 1
  [[ "$(printf '%s\n%s\n' "$ap" "$bp" | sort -V | tail -1)" == "$ap" ]]
}

if [[ -z $tag ]]; then
  while read -r t pre; do
    [[ -z $t ]] && continue
    # Dev builds are macOS-only, and only on the dev channel.
    if [[ $pre == true ]] && { [[ $channel != dev ]] || [[ $target != *apple-darwin ]]; }; then
      continue
    fi
    if [[ -z $tag ]] || newer "$t" "$tag"; then tag=$t; fi
  done < <(releases)
  [[ -n $tag ]] || { echo "couldn't find a release of $repo" >&2; exit 1; }
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
  echo "rlord $current is already the newest ($channel)"
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
asset="rlord-$tag-$target.tar.gz"
if [[ $use_gh == 1 ]]; then
  gh release download "$tag" -R "$repo" -p "$asset" -D "$d"
  gh release download "$tag" -R "$repo" -p SHA256SUMS -D "$d" 2>/dev/null || true
else
  base="https://github.com/$repo/releases/download/$tag"
  curl -fsSL -o "$d/$asset" "$base/$asset"
  curl -fsSL -o "$d/SHA256SUMS" "$base/SHA256SUMS" 2>/dev/null || rm -f "$d/SHA256SUMS"
fi
# Older releases have no SHA256SUMS; every new one does.
if [[ -f $d/SHA256SUMS ]]; then
  want=$(awk -v a="$asset" '$2 == a {print $1}' "$d/SHA256SUMS")
  if command -v sha256sum >/dev/null; then
    got=$(sha256sum "$d/$asset" | awk '{print $1}')
  else
    got=$(shasum -a 256 "$d/$asset" | awk '{print $1}')
  fi
  [[ -n $want && $want == "$got" ]] || { echo "checksum mismatch for $asset" >&2; exit 1; }
fi
tar -xzf "$d/$asset" -C "$d"
installer=("$d"/rlord-*/install.sh)
# ${a[@]+...}: an empty array under `set -u` is an error in macOS's bash 3.2.
"${installer[0]}" ${force[@]+"${force[@]}"}

# A new version was installed: if rlord is running, reset it onto the new
# binary (daemon, list and tmux config). Agent sessions keep running.
reset_running
