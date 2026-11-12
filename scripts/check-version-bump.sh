#!/usr/bin/env bash
# Fail unless REV bumps [workspace.package] version above BASE's, and
# Cargo.lock agrees. Every PR merged to main ships a release (see CLAUDE.md),
# so every PR must bump the version: a development build (X.Y.Z-dev.N, the
# usual) or a stable release (X.Y.Z). Semver order: X.Y.Z-dev.N < X.Y.Z.
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

# 0 if $1 > $2 in semver order (a pre-release sorts below its release).
newer() {
  local a=$1 b=$2
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

base_ver=$(version_at "$(git merge-base "$base" "$rev")")
new_ver=$(version_at "$rev")
[[ -n "$base_ver" && -n "$new_ver" ]] || { echo "version-bump: could not read versions" >&2; exit 1; }

if ! newer "$new_ver" "$base_ver"; then
  cat >&2 <<EOF
version-bump: Cargo.toml version is $new_ver; $base has $base_ver.
Every PR ships a release, so bump [workspace.package] version in Cargo.toml
and commit Cargo.lock too: usually the next development build (0.18.0-dev.1,
then -dev.2, …), or X.Y.Z for a stable release.
EOF
  exit 1
fi

# awk reads to the end (no early `exit`): stopping early SIGPIPEs `git show`,
# which `pipefail` turns into a failure (exit 141) once Cargo.lock is large.
lock_ver=$(git show "$rev:Cargo.lock" | awk '/^name = "robot-overlord"$/ && !done {getline; gsub(/version = |"/, ""); print; done=1}')
if [[ "$lock_ver" != "$new_ver" ]]; then
  echo "version-bump: Cargo.lock has robot-overlord $lock_ver, Cargo.toml has $new_ver — run cargo build and commit Cargo.lock" >&2
  exit 1
fi
echo "version-bump: $base_ver -> $new_ver"
