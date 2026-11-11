#!/usr/bin/env bash
# Build rlord locally and publish it as a GitHub release (no CI involved).
#
#   1. bump [workspace.package] version in Cargo.toml, commit, push
#   2. scripts/release.sh            # tags v<version> on origin and uploads
#
# Asset: rlord-v<version>-aarch64-apple-darwin.tar.gz (rlord + install.sh).
# Install on another Mac: see "Installing a release" in the README.
set -euo pipefail
cd "$(dirname "$0")/.."

target=aarch64-apple-darwin
[[ "$(uname -s)-$(uname -m)" == "Darwin-arm64" ]] || { echo "build on an Apple Silicon Mac" >&2; exit 1; }

ver=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
tag="v$ver"
[[ -n "$ver" ]] || { echo "no version in Cargo.toml" >&2; exit 1; }

# Refuse anything that would make the release not match a pushed commit.
[[ -z "$(git status --porcelain)" ]] || { echo "working tree is dirty" >&2; exit 1; }
git fetch -q origin
head=$(git rev-parse HEAD)
git branch -r --contains "$head" | grep -q . || { echo "HEAD $head is not pushed to origin" >&2; exit 1; }
if gh release view "$tag" >/dev/null 2>&1 || git ls-remote --exit-code --tags origin "$tag" >/dev/null 2>&1; then
  echo "$tag already exists — bump the version in Cargo.toml" >&2; exit 1
fi

cargo build --release --locked -p rlord --target "$target"
bin=target/$target/release/rlord
# The linker ad-hoc signs, but post-link strip can leave the signature stale;
# re-sign so the binary runs on the other Mac (arm64 refuses unsigned code).
codesign --force --sign - "$bin"
codesign --verify "$bin"
[[ "$("$bin" --version)" == "rlord $ver" ]] || { echo "built binary reports the wrong version" >&2; exit 1; }

name="rlord-$tag-$target"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$name"
cp "$bin" scripts/install.sh "$stage/$name/"
tar -czf "$stage/$name.tar.gz" -C "$stage" "$name"

gh release create "$tag" "$stage/$name.tar.gz" \
  --target "$head" --title "$tag" --generate-notes
