#!/usr/bin/env bash
# Publish a stable release's crates to crates.io (users: cargo install
# robot-overlord). Run by scripts/release.sh after the GitHub release; needs
# `cargo login` with a token that can publish.
#
#   scripts/publish-crates.sh [--dry-run]
#
# The workspace's internal dependencies are path-only in git (so PRs don't
# have to bump them); crates.io needs a version on each, so this pins them
# to the release version for the publish and puts the files back after.
set -euo pipefail
cd "$(dirname "$0")/.."
dry=()
[[ ${1:-} == --dry-run ]] && dry=(--dry-run)

ver=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
[[ $ver != *-* ]] || { echo "publish-crates: $ver is a dev build; only stable releases go to crates.io" >&2; exit 1; }

manifests=(crates/daemon/Cargo.toml crates/tui/Cargo.toml crates/rlord/Cargo.toml)
restore() { git checkout -q -- "${manifests[@]}"; }
trap restore EXIT
for m in "${manifests[@]}"; do
  sed -i '' -E "s|(package = \"robot-overlord-[a-z]+\", path = \"[^\"]+\")|\1, version = \"=$ver\"|" "$m"
done

# Dependencies first; each must be on the index before the next.
for p in robot-overlord-core robot-overlord-daemon robot-overlord-tui robot-overlord; do
  if [[ ${#dry[@]} -eq 0 ]] && cargo info "$p@$ver" >/dev/null 2>&1; then
    echo "publish-crates: $p $ver already published"
    continue
  fi
  cargo publish -p "$p" --allow-dirty ${dry[@]+"${dry[@]}"}
done
