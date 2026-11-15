#!/bin/sh
# Smoke-test release files on Linux distros, in Docker (colima on the Mac).
# The logic is `cargo xtask smoke-linux` (xtask/src/smoke.rs); this keeps
# the old command working.
#
#   scripts/smoke-linux.sh DIST_DIR [IMAGE…]
exec cargo run --quiet --locked --manifest-path "$(dirname "$0")/../Cargo.toml" \
  --package xtask -- smoke-linux "$@"
