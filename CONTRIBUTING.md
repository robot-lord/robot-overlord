# Contributing

rlord is a small Rust workspace:

| crate (directory) | package | what |
|---|---|---|
| `crates/core` | `robot-overlord-core` | DB, tmux, agent transcripts, config, keys, search |
| `crates/daemon` | `robot-overlord-daemon` | the daemon, its socket API, the MCP server, the overlord |
| `crates/tui` | `robot-overlord-tui` | the list, forms, overlay |
| `crates/rlord` | `robot-overlord` | the `rlord` binary (CLI) |
| `xtask` | `xtask` (not published) | project automation: `cargo xtask <task>` |

`docs/architecture.md` explains how it fits together; `CLAUDE.md` holds the
rules agents (and people) follow when changing it.

## Dev loop

```sh
cargo build
ln -sfn "$PWD/target/debug/rlord" ~/.local/bin/rlord   # dev install: tracks every build
scripts/dev-daemon.sh                                  # build + (re)start the daemon
rlord claude install-hooks
rlord
```

Useful commands while working:

```sh
rlord new --project ~/src/myrepo "first prompt"   # launch a Claude session
rlord ls --sort user --tag wip                    # filters and sorts
rlord attach <ref>                                # id, id prefix, name or name prefix
rlord stop <ref> && rlord restart <ref>           # resume with full context
rlord search <words> / rlord history <ref>        # past work
```

Checking what a new user's tmux looks like while you have your own profile
imported: `RLORD_TMUX_PROFILE=default rlord` (agent sessions switch to
rlord's own bar and clickable hints for that run; a plain `rlord` switches
back). `rlord tmux profile` says which setup is active and why.

## Demo and screenshots

`scripts/demo/demo.sh` starts a throwaway rlord full of made-up sessions
(its own data, config and tmux server; stand-in `claude`, `codex` and
`opencode` that need no model), for trying the UI. (The OpenCode stand-in
runs no server, so its session's status comes from the session database
alone and never shows "needs you". Give the demo a short DIR: the daemon's
unix socket lives under it.) It sets `RLORD_NO_LOGIN_SHELL=1` (rlord then
never asks your login shell where an agent is, so only the stand-ins on the
demo's PATH exist) and a stand-in `SHELL`: set both in any scratch rlord you
build yourself, so it can never launch a real agent. `scripts/demo/screenshots.sh DIR` retakes the
README's images in `docs/images` with [vhs](https://github.com/charmbracelet/vhs).

## Tests

```sh
cargo fmt && cargo clippy --all-targets && cargo test
```

Never touch the default tmux server in tests or scripts: rlord runs on its
own socket (`tmux -L rlord`), and tests use scratch sockets
(`RLORD_TMUX_SOCKET`).

## Versions and releases

Every PR bumps `[workspace.package] version` in `Cargo.toml` (and commits
`Cargo.lock`); `cargo xtask check-version-bump` enforces it (CI, and
`.githooks/pre-push` once you `git config core.hooksPath .githooks`).

- **Development builds** `X.Y.Z-dev.N`: what ordinary PRs bump to (the next
  version with `-dev.1`, then `-dev.2`, …). Released for macOS only, as GitHub
  pre-releases; `[update] channel = "dev"` installs them.
- **Stable releases** `X.Y.Z`: a PR that drops the suffix. Released for every
  platform, with the Homebrew formula and crates.io.

After a PR merges, release from the merged `main`:

```sh
git fetch origin && git checkout --detach origin/main
cargo xtask release
```

Release automation is Rust, in the `xtask` crate (`cargo xtask help`
lists the tasks). `cargo xtask release --dry-run` publishes nothing: it
warns where a real run would refuse and prints what it would do. A dev
build or `--local` still builds here; a stable release built in CI stops
after the checks, so rehearse that with a manual dry run of the workflow
(below).

- **Dev builds** are built on the maintainer's Apple Silicon Mac and uploaded
  as a pre-release.
- **Stable releases** are built in GitHub Actions
  (`.github/workflows/release.yml`), so anyone can see how the binaries were
  made. `cargo xtask release` pushes the `vX.Y.Z` tag, which starts the workflow. A
  preflight job fails fast unless the tag is on `main`, stable and matches
  `Cargo.toml`. Then a macOS runner builds `aarch64-apple-darwin`; a Linux runner cross-compiles
  the static musl builds (`cargo zigbuild`) and packages `.deb`/`.rpm`; both
  architectures are smoke-tested in Docker on Ubuntu, Debian, Fedora and
  Amazon Linux (`scripts/smoke-linux.sh`); the last job writes `SHA256SUMS`,
  attests build provenance and creates the release as a **draft**.
  `cargo xtask release` waits for the run, checks the assets against `SHA256SUMS`
  and `gh attestation verify`, publishes the draft, then does what still needs
  local credentials: the Homebrew tap (`cargo xtask update-tap`) and crates.io
  (`cargo xtask publish-crates`, once the repo is public).

The build commands live in `cargo xtask build-release`, which CI and local
builds share; the version rules, targets and asset names in
`xtask/src/version.rs`. A manual run of the workflow with `dry_run` (the default for
manual runs) builds and tests everything and uploads only workflow
artifacts.

If the release stops after pushing the tag (a failed run you then re-ran,
a lost connection), `cargo xtask release --resume` picks up from the tag's
workflow run: watch, verify, publish the draft if it is still one, tap,
crates.io. To re-run a failed release, dispatch it on the tag itself, so
its run is titled `release vX.Y.Z` and `--resume` finds it (it ignores
dry runs):

```sh
gh workflow run release.yml --ref vX.Y.Z -f tag=vX.Y.Z -f dry_run=false
```

A re-run replaces any draft the failed run left. Every file in the release
is checked against `SHA256SUMS` and its provenance attestation
(`gh attestation verify`) before the draft is published.

If Actions is down, build a stable release locally instead: delete the tag
on origin and locally (`git push origin :refs/tags/vX.Y.Z; git tag -d
vX.Y.Z`) and any draft release CI left (`gh release delete vX.Y.Z --yes`),
then `cargo xtask release --local`. Creating that release pushes
the tag again, which starts the workflow; its preflight sees the release
is already published and skips the build, so the two never race. If no
run shows up for a pushed tag, `cargo xtask release` also cancels any run queued
for it before suggesting `--local`. One-time setup for that:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
brew install zig colima docker && colima start
cargo install --locked cargo-zigbuild cargo-deb cargo-generate-rpm
```

Stable releases also need push access to `robot-lord/homebrew-tap` and
`cargo login` (crates.io token).

## License

By contributing you agree your work is licensed under MIT OR Apache-2.0, as
the project is.
