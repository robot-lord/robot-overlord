#!/usr/bin/env bash
# Smoke-test release files on Linux distros, in Docker (colima on the Mac):
# install with get.sh from the local files (it installs tmux too), then run
# the daemon and a few commands on a scratch tmux socket.
#
#   scripts/smoke-linux.sh DIST_DIR [IMAGE…]
#
# Native (aarch64) images by default; amd64 images run the x86_64 build
# under emulation when Docker has it.
set -euo pipefail
dist=$(cd "${1:?usage: smoke-linux.sh DIST_DIR [IMAGE…]}" && pwd)
shift
images=("$@")
if [[ ${#images[@]} -eq 0 ]]; then
  # Arch: only as amd64 (emulated), where its mirrors often fail to sync;
  # run it by name when needed. The x86_64 build is tested on Ubuntu.
  images=(ubuntu:22.04 ubuntu:24.04 debian:12 fedora:41 amazonlinux:2023 linux/amd64:ubuntu:24.04)
fi
here=$(cd "$(dirname "$0")" && pwd)
docker info >/dev/null 2>&1 || { echo "Docker isn't running (colima start)" >&2; exit 1; }

test_in() {
  local platform=() image=$1
  if [[ $image == linux/*:* ]]; then
    platform=(--platform "${image%%:*}")
    image=${image#*:}
  fi
  echo "== $image ${platform[*]:-}"
  docker run --rm ${platform[@]+"${platform[@]}"} \
    -v "$dist:/dist:ro" -v "$here/get.sh:/get.sh:ro" \
    -e RLORD_DIST=/dist -e RLORD_YES=1 "$image" sh -ec '
      # Minimal images may lack what get.sh itself needs.
      if ! command -v tar >/dev/null || ! command -v gzip >/dev/null; then
        (dnf install -y tar gzip || yum install -y tar gzip) >/dev/null 2>&1 || true
      fi
      if command -v pacman >/dev/null; then pacman -Sy --noconfirm >/dev/null 2>&1; fi
      sh /get.sh
      export PATH="$HOME/.local/bin:$PATH"
      rlord --version
      tmux -V
      export RLORD_TMUX_SOCKET=smoke
      tmux -L smoke new-session -d -s keep "sleep 600"
      rlord ls          # starts the daemon on demand
      rlord ping
      rlord search nothing-here || true
      echo OK
    ' 2>&1 | tail -6
}

fail=0
for i in "${images[@]}"; do
  out=$(test_in "$i") || true
  echo "$out"
  [[ $(tail -1 <<<"$out") == OK ]] || { echo "FAILED: $i" >&2; fail=1; }
done
exit $fail
