#!/usr/bin/env bash
# Build rlord on the maintainer's Apple Silicon Mac and publish it as a GitHub
# release. No CI: Linux binaries are cross-compiled here (cargo zigbuild) and
# smoke-tested in Docker before anything is uploaded.
#
#   scripts/release.sh [--skip-smoke]
#
# The version in Cargo.toml decides the kind of release:
#   X.Y.Z-dev.N  development build: macOS only, a GitHub pre-release. What
#                ordinary PRs ship; `[update] channel = "dev"` installs them.
#   X.Y.Z        stable release: macOS + Linux (x86_64, aarch64, static musl)
#                tarballs, .deb and .rpm, the Homebrew formula, crates.io.
#
# Assets: rlord-v<ver>-<target>.tar.gz (rlord, install.sh, licenses),
# SHA256SUMS; stable also rlord_<ver>_<arch>.deb, rlord-<ver>.<arch>.rpm.
#
# Needs (stable): rustup targets x86_64/aarch64-unknown-linux-musl, zig,
# cargo-zigbuild, cargo-deb, cargo-generate-rpm; Docker (colima) for the
# smoke test; push access to ayersb/homebrew-tap; `cargo login` for crates.io.
set -euo pipefail
cd "$(dirname "$0")/.."
repo=ayersb/robot-overlord
tap=ayersb/homebrew-tap

smoke=1
for a in "$@"; do
  case $a in
    --skip-smoke) smoke=0 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done

[[ "$(uname -s)-$(uname -m)" == "Darwin-arm64" ]] || { echo "release from an Apple Silicon Mac" >&2; exit 1; }

ver=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
tag="v$ver"
[[ -n "$ver" ]] || { echo "no version in Cargo.toml" >&2; exit 1; }
dev=0
[[ $ver == *-* ]] && dev=1

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

# One tarball per target: the binary, the installer, the licenses.
pack() {
  local target=$1 bin=$2
  local name="rlord-$tag-$target"
  mkdir "$stage/$name"
  cp "$bin" scripts/install.sh LICENSE-MIT LICENSE-APACHE README.md "$stage/$name/"
  tar -czf "$dist/$name.tar.gz" -C "$stage" "$name"
}

# macOS (every release).
mac=aarch64-apple-darwin
cargo build --release --locked -p robot-overlord --target "$mac"
bin=target/$mac/release/rlord
# The linker ad-hoc signs, but post-link strip can leave the signature stale;
# re-sign so the binary runs on other Macs (arm64 refuses unsigned code).
codesign --force --sign - "$bin"
codesign --verify "$bin"
[[ "$("$bin" --version)" == "rlord $ver" ]] || { echo "built binary reports the wrong version" >&2; exit 1; }
pack "$mac" "$bin"

linux=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
if [[ $dev == 0 ]]; then
  for t in "${linux[@]}"; do
    cargo zigbuild --release --locked -p robot-overlord --target "$t"
    pack "$t" "target/$t/release/rlord"
  done
  # Distro packages: the same static binaries, requiring tmux.
  for t in "${linux[@]}"; do
    cargo deb -p robot-overlord --target "$t" --no-build --no-strip --output "$dist/" >/dev/null
    cargo generate-rpm -p crates/rlord --target "$t" --auto-req disabled -o "$dist/" >/dev/null
  done
fi

# Checksums before the smoke test: the installer it runs verifies them.
(cd "$dist" && shasum -a 256 -- * > SHA256SUMS)

if [[ $dev == 0 && $smoke == 1 ]]; then
  scripts/smoke-linux.sh "$dist"
fi

flags=(--generate-notes --title "$tag" --target "$head")
[[ $dev == 1 ]] && flags+=(--prerelease)
gh release create "$tag" -R "$repo" "$dist"/* "${flags[@]}"

if [[ $dev == 0 ]]; then
  scripts/update-tap.sh "$ver" "$dist/SHA256SUMS" "$tap"
  # crates.io publishes the source: only once the repo itself is public.
  if [[ "$(gh repo view "$repo" --json visibility -q .visibility)" != PUBLIC ]]; then
    echo "crates.io: $repo is private — skipped (publish later with scripts/publish-crates.sh)" >&2
  elif [[ -f ~/.cargo/credentials.toml ]]; then
    scripts/publish-crates.sh
  else
    echo "crates.io: not logged in (cargo login) — skipped" >&2
  fi
fi
