#!/usr/bin/env bash
# Fail unless REV bumps [workspace.package] version above BASE's, and
# Cargo.lock agrees. Every PR merged to main ships a release (see CLAUDE.md),
# so every PR must bump the version.
#
#   scripts/check-version-bump.sh [BASE] [REV]    # defaults: origin/main HEAD
#
# Used by CI (pull requests) and .githooks/pre-push.
set -euo pipefail
base=${1:-origin/main}
rev=${2:-HEAD}

version_at() {
  git show "$1:Cargo.toml" | sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p'
}

base_ver=$(version_at "$(git merge-base "$base" "$rev")")
new_ver=$(version_at "$rev")
[[ -n "$base_ver" && -n "$new_ver" ]] || { echo "version-bump: could not read versions" >&2; exit 1; }

if [[ "$new_ver" == "$base_ver" ]] ||
  [[ "$(printf '%s\n%s\n' "$base_ver" "$new_ver" | sort -V | tail -1)" != "$new_ver" ]]; then
  cat >&2 <<EOF
version-bump: Cargo.toml version is $new_ver; $base has $base_ver.
Every PR ships a release, so bump [workspace.package] version in Cargo.toml
(pre-1.0: patch for fixes, minor for features) and commit Cargo.lock too.
EOF
  exit 1
fi

lock_ver=$(git show "$rev:Cargo.lock" | awk '/^name = "rlord"$/{getline; gsub(/version = |"/, ""); print; exit}')
if [[ "$lock_ver" != "$new_ver" ]]; then
  echo "version-bump: Cargo.lock has rlord $lock_ver, Cargo.toml has $new_ver — run cargo build and commit Cargo.lock" >&2
  exit 1
fi
echo "version-bump: $base_ver -> $new_ver"
