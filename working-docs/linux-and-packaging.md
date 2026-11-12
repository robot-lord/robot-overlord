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

- [x] version channels: config, update, release.sh, version-bump check
- [x] Linux code: clipboard (wl-copy/xclip/xsel/OSC 52), appearance
      (gsettings, RLORD_APPEARANCE), systemd user unit, Alt-/Ctrl- key
      labels off macOS, XDG log path, portable process scan, C.UTF-8;
      tests pass in an Ubuntu container
- [x] cross builds + .deb/.rpm + SHA256SUMS + smoke tests (Ubuntu 22.04/24.04,
      Debian 12, Fedora 41, Amazon Linux 2023 pass; amd64 via emulation)
- [x] get.sh installer (offers tmux install; apt does `update` first)
- [ ] Homebrew tap repo: `scripts/update-tap.sh` written; the repo itself
      must be created by the owner (`gh repo create` was blocked here)
- [x] OSS files: licenses, README install, CONTRIBUTING; crates.io metadata
      (packages renamed robot-overlord*; dry-run of core passes)
- [x] scrub: fixtures/paths/wording neutral
- [ ] git author emails (a tailnet hostname, a personal address): the
      owner's call (history rewrite + force-push)

## Owed / open

- Arch: its amd64 image's mirrors fail to sync under emulation; not in the
  default smoke set. AUR package: later.
- The tap formula and curl installer only work once the repo is public
  (release assets of a private repo need auth).
- crates.io: owner runs `cargo login`; `publish-crates.sh` pins internal
  dep versions at publish time.

- tmux minimum version: audit features used (display-popup ≥3.2,
  client_termtype, send-keys -N); Ubuntu 22.04 / Amazon Linux 2023 ship 3.2a.
