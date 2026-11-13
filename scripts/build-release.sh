#!/usr/bin/env bash
# Build rlord's release files for one platform into DIST. The one place the
# release build commands live: the release workflow
# (.github/workflows/release.yml) and scripts/release.sh both call it, so CI
# and a local build can't drift.
#
#   scripts/build-release.sh macos DIST   aarch64-apple-darwin tarball
#                                         (on an Apple Silicon Mac)
#   scripts/build-release.sh linux DIST   x86_64 + aarch64 static musl
#                                         tarballs, .deb and .rpm
#
# Tarballs: rlord-v<ver>-<target>.tar.gz (rlord, install.sh, licenses).
# Linux needs rustup targets x86_64/aarch64-unknown-linux-musl, zig,
# cargo-zigbuild, cargo-deb, cargo-generate-rpm. Writes no SHA256SUMS: the
# caller sums the full set of files.
set -euo pipefail
cd "$(dirname "$0")/.."
platform=${1:?usage: build-release.sh macos|linux DIST}
mkdir -p "${2:?usage: build-release.sh macos|linux DIST}"
dist=$(cd "$2" && pwd)

ver=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
[[ -n "$ver" ]] || { echo "no version in Cargo.toml" >&2; exit 1; }
tag="v$ver"

mkdir -p target
stage=$(mktemp -d "$PWD/target/pack.XXXXXX")
trap 'rm -rf "$stage"' EXIT

# One tarball per target: the binary, the installer, the licenses.
pack() {
  local target=$1 bin=$2
  local name="rlord-$tag-$target"
  mkdir "$stage/$name"
  cp "$bin" scripts/install.sh LICENSE-MIT LICENSE-APACHE README.md "$stage/$name/"
  tar -czf "$dist/$name.tar.gz" -C "$stage" "$name"
}

case $platform in
  macos)
    [[ "$(uname -s)-$(uname -m)" == "Darwin-arm64" ]] || { echo "build macOS on Apple Silicon" >&2; exit 1; }
    mac=aarch64-apple-darwin
    cargo build --release --locked -p robot-overlord --target "$mac"
    bin=target/$mac/release/rlord
    # The linker ad-hoc signs, but post-link strip can leave the signature
    # stale; re-sign so the binary runs on other Macs (arm64 refuses
    # unsigned code).
    codesign --force --sign - "$bin"
    codesign --verify "$bin"
    [[ "$("$bin" --version)" == "rlord $ver" ]] || { echo "built binary reports the wrong version" >&2; exit 1; }
    pack "$mac" "$bin"
    ;;
  linux)
    linux=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)
    for t in "${linux[@]}"; do
      cargo zigbuild --release --locked -p robot-overlord --target "$t"
      pack "$t" "target/$t/release/rlord"
    done
    # Distro packages: the same static binaries, requiring tmux.
    # RPM forbids '-' in a version; '~' sorts as a pre-release (only a dry
    # run of a dev version gets here).
    rpmver=(); [[ $ver == *-* ]] && rpmver=(-s "version = \"${ver//-/"~"}\"")
    for t in "${linux[@]}"; do
      cargo deb -p robot-overlord --target "$t" --no-build --no-strip --output "$dist/" >/dev/null
      cargo generate-rpm -p crates/rlord --target "$t" --auto-req disabled ${rpmver[@]+"${rpmver[@]}"} -o "$dist/" >/dev/null
    done
    ;;
  *) echo "unknown platform $platform (macos|linux)" >&2; exit 2 ;;
esac
