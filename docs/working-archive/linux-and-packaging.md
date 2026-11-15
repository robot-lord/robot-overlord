# Linux support, packaging, release channels: residue

Shipped: static musl Linux builds, `.deb`/`.rpm`, `SHA256SUMS`, `get.sh`,
the Homebrew tap (`robot-lord/homebrew-tap`), stable and dev release
channels, release builds in GitHub Actions. How to build and release is
in `CONTRIBUTING.md`; targets and asset names are in
`xtask/src/version.rs`. The full working doc is in git history
(`docs/working/linux-and-packaging.md`, removed 2026-11-14). Publishing
to crates.io and the first stable release since 0.18.1 are tracked in
`docs/working/public-launch.md`.

## Owed

- **tmux minimum version**: the README says tmux 3.2+. Audit the features
  used (display-popup needs 3.2, `client_termtype`, `send-keys -N`)
  against that; Ubuntu 22.04 and Amazon Linux 2023 ship 3.2a.
- **Arch** isn't in the default smoke set: its amd64 image's mirrors fail
  to sync under emulation. AUR package: issue #57.
- **Git author emails** in history (a tailnet hostname, a personal
  address): the owner's call. Changing them means a history rewrite and
  a force-push of a public repository.
