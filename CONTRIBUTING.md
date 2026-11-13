# Contributing

rlord is a small Rust workspace:

| crate (directory) | package | what |
|---|---|---|
| `crates/core` | `robot-overlord-core` | DB, tmux, agent transcripts, config, keys, search |
| `crates/daemon` | `robot-overlord-daemon` | the daemon, its socket API, the MCP server, the overlord |
| `crates/tui` | `robot-overlord-tui` | the list, forms, overlay |
| `crates/rlord` | `robot-overlord` | the `rlord` binary (CLI) |

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
`Cargo.lock`); `scripts/check-version-bump.sh` enforces it (CI, and
`.githooks/pre-push` once you `git config core.hooksPath .githooks`).

- **Development builds** `X.Y.Z-dev.N`: what ordinary PRs bump to (the next
  version with `-dev.1`, then `-dev.2`, …). Released for macOS only, as GitHub
  pre-releases; `[update] channel = "dev"` installs them.
- **Stable releases** `X.Y.Z`: a PR that drops the suffix. Released for every
  platform, with the Homebrew formula and crates.io.

Releases are built on an Apple Silicon Mac, no CI: after the PR merges,

```sh
git fetch origin && git checkout --detach origin/main
scripts/release.sh
```

For a stable release that cross-compiles the Linux builds (`cargo zigbuild`),
packages `.deb`/`.rpm`, smoke-tests them in Docker on Ubuntu, Debian, Fedora
and Amazon Linux (`scripts/smoke-linux.sh`), uploads everything with
`SHA256SUMS`, updates the Homebrew tap (`scripts/update-tap.sh`) and publishes
to crates.io (`scripts/publish-crates.sh`). One-time setup for that:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
brew install zig colima docker && colima start
cargo install --locked cargo-zigbuild cargo-deb cargo-generate-rpm
cargo login   # crates.io token
```

## License

By contributing you agree your work is licensed under MIT OR Apache-2.0, as
the project is.
