#!/usr/bin/env bash
# Download the web plugin's release files for `rlord web install`:
#
#   web-install.sh VERSION TARGET DIR
#
# Puts rlord-web-vVERSION-TARGET.tar.gz and SHA256SUMS from the GitHub
# release vVERSION into DIR. Uses the GitHub CLI when it's logged in
# (needed while the repo is private), else plain curl, as update.sh does.
# For a stable release with `gh` at hand it also checks the tarball's build
# provenance (`gh attestation verify`); dev builds carry none.
#
# Download only: rlord checks the tarball against SHA256SUMS, unpacks it
# and checks the plugin against the hash built into rlord itself before
# installing it. Compiled into rlord (crates/rlord/src/web_install.rs).
set -euo pipefail
repo=robot-lord/robot-overlord
version=${1:?VERSION}
target=${2:?TARGET}
dir=${3:?DIR}
tag=v$version
asset="rlord-web-$tag-$target.tar.gz"

use_gh=0
if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then use_gh=1; fi

if [[ $use_gh == 1 ]]; then
  gh release download "$tag" -R "$repo" -p "$asset" -p SHA256SUMS -D "$dir"
else
  base="https://github.com/$repo/releases/download/$tag"
  curl -fsSL -o "$dir/$asset" "$base/$asset"
  curl -fsSL -o "$dir/SHA256SUMS" "$base/SHA256SUMS"
fi

if [[ $use_gh == 1 && $version != *-* ]]; then
  gh attestation verify "$dir/$asset" -R "$repo" >/dev/null ||
    { echo "$asset has no valid build provenance for $repo" >&2; exit 1; }
fi
