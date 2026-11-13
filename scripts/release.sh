#!/usr/bin/env bash
# Publish rlord as a GitHub release from the merged main.
#
#   scripts/release.sh [--local] [--skip-smoke]
#
# The version in Cargo.toml decides the kind of release:
#   X.Y.Z-dev.N  development build: built here on the maintainer's Apple
#                Silicon Mac, macOS only, a GitHub pre-release. What ordinary
#                PRs ship; `[update] channel = "dev"` installs them.
#   X.Y.Z        stable release: built in GitHub Actions
#                (.github/workflows/release.yml). This script pushes the tag,
#                waits for the run, checks the release assets against the
#                CI-made SHA256SUMS, then does the steps that still need local
#                credentials: the Homebrew formula and crates.io.
#
# --local      stable only, the fallback when Actions is down: build macOS +
#              Linux here (scripts/build-release.sh, the same commands CI
#              runs), smoke-test in Docker, and create the release from here.
# --skip-smoke with --local: skip the Docker smoke test.
#
# Assets: rlord-v<ver>-<target>.tar.gz (rlord, install.sh, licenses),
# SHA256SUMS; stable also rlord_<ver>_<arch>.deb, rlord-<ver>.<arch>.rpm.
#
# Needs: gh; push access to robot-lord/homebrew-tap and `cargo login` for a
# stable release. --local also needs rustup targets
# x86_64/aarch64-unknown-linux-musl, zig, cargo-zigbuild, cargo-deb,
# cargo-generate-rpm and Docker (colima) for the smoke test.
set -euo pipefail
cd "$(dirname "$0")/.."
repo=robot-lord/robot-overlord
tap=robot-lord/homebrew-tap

smoke=1
local_build=0
for a in "$@"; do
  case $a in
    --skip-smoke) smoke=0 ;;
    --local) local_build=1 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done

ver=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
tag="v$ver"
[[ -n "$ver" ]] || { echo "no version in Cargo.toml" >&2; exit 1; }
dev=0
[[ $ver == *-* ]] && dev=1
[[ $dev == 1 && $local_build == 1 ]] && { echo "--local is for stable releases; dev builds are always local" >&2; exit 2; }
build_here=$(( dev == 1 || local_build == 1 ))
if [[ $build_here == 1 ]]; then
  [[ "$(uname -s)-$(uname -m)" == "Darwin-arm64" ]] || { echo "build from an Apple Silicon Mac" >&2; exit 1; }
fi

# Refuse anything that would make the release not match a pushed commit.
[[ -z "$(git status --porcelain)" ]] || { echo "working tree is dirty" >&2; exit 1; }
git fetch -q origin
head=$(git rev-parse HEAD)
# Only merged code ships: HEAD must be on origin/main, not just on some
# pushed branch (a PR branch would burn the version before it merges).
git merge-base --is-ancestor "$head" origin/main ||
  { echo "HEAD $head is not on origin/main — release only merged code" >&2; exit 1; }
if gh release view "$tag" -R "$repo" >/dev/null 2>&1 || git ls-remote --exit-code --tags origin "$tag" >/dev/null 2>&1; then
  echo "$tag already exists — bump the version in Cargo.toml" >&2; exit 1
fi

# Under the checkout, not the system temp dir: Docker on the Mac (colima)
# only sees the home folder, and the smoke test mounts these files. (macOS
# mktemp ignores TMPDIR.)
mkdir -p target
stage=$(mktemp -d "$PWD/target/release.XXXXXX")
trap 'rm -rf "$stage"' EXIT
dist=$stage/dist
mkdir -p "$dist"

if [[ $build_here == 1 ]]; then
  scripts/build-release.sh macos "$dist"
  [[ $dev == 0 ]] && scripts/build-release.sh linux "$dist"
  # Checksums before the smoke test: the installer it runs verifies them.
  (cd "$dist" && shasum -a 256 -- * > SHA256SUMS)
  if [[ $dev == 0 && $smoke == 1 ]]; then
    scripts/smoke-linux.sh "$dist"
  fi
  flags=(--generate-notes --title "$tag" --target "$head")
  [[ $dev == 1 ]] && flags+=(--prerelease)
  gh release create "$tag" -R "$repo" "$dist"/* "${flags[@]}"
  [[ $dev == 1 ]] && exit 0
else
  # Stable in CI: the tag push starts .github/workflows/release.yml.
  git tag "$tag" "$head"
  git push origin "refs/tags/$tag"
  echo "waiting for the release workflow on $tag…"
  run=
  for _ in $(seq 60); do
    run=$(gh run list -R "$repo" -w release.yml -e push -c "$head" -L 5 \
      --json databaseId,headBranch -q "map(select(.headBranch == \"$tag\"))[0].databaseId // empty")
    [[ -n $run ]] && break
    sleep 5
  done
  [[ -n $run ]] || { echo "no release workflow run for $tag — check Actions" >&2; exit 1; }
  gh run watch "$run" -R "$repo" --exit-status ||
    { echo "release workflow failed: https://github.com/$repo/actions/runs/$run (fallback: delete the tag, then scripts/release.sh --local)" >&2; exit 1; }
  # Check every asset against the CI-made SHA256SUMS before trusting it.
  gh release download "$tag" -R "$repo" -D "$dist"
  (cd "$dist" && shasum -a 256 -c SHA256SUMS)
  for t in aarch64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
    grep -q " rlord-$tag-$t.tar.gz\$" "$dist/SHA256SUMS" || { echo "SHA256SUMS lacks $t" >&2; exit 1; }
  done
fi

scripts/update-tap.sh "$ver" "$dist/SHA256SUMS" "$tap"
# crates.io publishes the source: only once the repo itself is public.
if [[ "$(gh repo view "$repo" --json visibility -q .visibility)" != PUBLIC ]]; then
  echo "crates.io: $repo is private — skipped (publish later with scripts/publish-crates.sh)" >&2
elif [[ -f ~/.cargo/credentials.toml ]]; then
  scripts/publish-crates.sh
else
  echo "crates.io: not logged in (cargo login) — skipped" >&2
fi
echo "released: https://github.com/$repo/releases/tag/$tag"
