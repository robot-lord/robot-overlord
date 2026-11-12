**Status:** in progress

# Linux support, packaging, release channels

Goal: make rlord installable by anyone (the repo goes public), on Apple
Silicon Macs and Linux, with one command; keep releasing from the laptop (no
CI for releases).

## Decisions

- License: MIT OR Apache-2.0.
- Targets: `aarch64-apple-darwin` only for macOS (no Intel). Linux:
  `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` (static: one
  binary for Ubuntu, Debian, Fedora, RHEL, Amazon Linux, Arch, Alpine).
- Built on the Mac: Linux via `cargo zigbuild`. Smoke-tested in Docker
  (colima) before upload.
- Install paths, first release: `curl -fsSL …/get.sh | sh` (detects
  platform, verifies the checksum, offers to install tmux with the system's
  package manager after asking), Homebrew tap (`ayersb/homebrew-tap`,
  `depends_on "tmux"`), `.deb` / `.rpm` (depend on tmux), `SHA256SUMS`.
  crates.io prepared, published once the owner has a token. AUR: later.
- Release channels:
  - **stable** `X.Y.Z`: every platform, `release.sh` on main.
  - **dev** `X.Y.Z-dev.N`: macOS only, a GitHub pre-release; what ordinary
    PRs ship. `[update] channel = "dev"` makes `rlord update` take the
    newest release of either kind.

## Progress

- [ ] version channels: config, update, release.sh, version-bump check
- [ ] Linux code: clipboard, appearance, autostart, update target
- [ ] cross builds + .deb/.rpm + SHA256SUMS + smoke tests
- [ ] get.sh installer
- [ ] Homebrew tap repo + formula updated by release.sh
- [ ] OSS files: licenses, README install, CONTRIBUTING; crates.io metadata
- [ ] scrub for private/Mac-only content before going public

## Owed / open

- tmux minimum version: audit features used (display-popup ≥3.2,
  client_termtype, send-keys -N); Ubuntu 22.04 / Amazon Linux 2023 ship 3.2a.
