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

`rust-toolchain.toml` pins the Rust version (with rustfmt, clippy and the
release targets) for CI, release builds and local work alike; rustup
installs it on first use.

```sh
cargo build
ln -sfn "$PWD/target/debug/rlord" ~/.local/bin/rlord   # dev install: tracks every build
cargo xtask dev-daemon                                 # build + (re)start the daemon
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

### The web UI

The web plugin (`crates/web`, binary `rlord-web`) builds with the rest of
the workspace and embeds a placeholder page until the real one is built.
The page (`crates/web/ui/`, TypeScript) needs Node (the major in
`crates/web/ui/.node-version`):

```sh
cargo xtask web check      # types, lint, format, unit tests (installs the pinned tools first)
cargo xtask web build      # the page into crates/web/ui/dist/; rebuild rlord-web to embed it
cargo xtask web watch      # rebuild on change; run a debug rlord-web with RLORD_WEB_ASSETS=crates/web/ui/dist
cargo build -p robot-overlord-web && rlord web   # a checkout's rlord runs the rlord-web beside it
```

`scripts/demo/demo.sh --web DIR` sets up the demo with the plugin
(`RLORD_DEMO_SOCKET` and `[web] port` in its config run a second demo
beside another). `cargo xtask check-deps` keeps network crates out of
`rlord`; see "Web UI" in docs/architecture.md.

### The phone app

The iOS app (`apps/ios`) is a thin SwiftUI shell around the phone build
of the page and the device engine (`crates/e2e-client`, through UniFFI in
`crates/e2e-ffi`); see "The phone app" in docs/architecture.md. It needs
Xcode (selected with `xcode-select`, or the tasks set `DEVELOPER_DIR`) and
the iOS Rust targets in `rust-toolchain.toml`. Everything is built
locally; nothing is released from the feature branch.

```sh
cargo xtask app ios                 # build, install and launch on a simulator (--sim NAME)
cargo xtask app ios --device ID     # on your phone (xcrun devicectl list devices); signs with the team
cargo xtask app test-ios            # XCTest, then the XCUITest against a loopback host stack
cargo xtask app test-ios --unit     # XCTest only
cargo xtask app dev                 # the phone page in a desktop browser, over rlord-device bridge
cargo xtask app test-e2e            # the page's store and RemoteTransport over the bridge (no simulator; CI)
cargo xtask app test-term           # the real Claude Code's terminal through the pane link, headless (CI)
cargo xtask app bindings            # regenerate the Swift bindings (the build tasks do this)
```

The generated bindings, the xcframework and the copied page are
git-ignored. `cargo xtask check-deps` fails if `apps/` names a network
API.

The first `--device` install registers the phone with the team and makes
the development certificate and profile (Xcode signed in to the team,
Developer Mode on in the phone's Privacy & Security settings, then trust
the developer under General → VPN & Device Management). Later installs
work over Wi-Fi while the phone is unlocked on the same network.

#### A relay for your phone: Tailscale Serve

The phone trusts public certificates only. In development, run the relay
on your Mac and let [Tailscale Serve](https://tailscale.com/kb/1312/serve)
give it your tailnet name and certificate. Serve is reachable from your
tailnet only, not the internet (Funnel would publish it; don't). The
phone needs the Tailscale app on the same tailnet. The host is a scratch
demo rlord, never your real install.

```sh
cargo build --release -p robot-overlord-relay
mkdir -p /tmp/rwd/relay && cat > /tmp/rwd/relay/relay.toml <<'EOF'
origin = "https://<mac>.<tailnet>.ts.net"   # tailscale status --json | jq -r .Self.DNSName, no trailing dot
listen = "127.0.0.1:8787"
data_dir = "/tmp/rwd/relay"
[tls]
mode = "proxy"
EOF
target/release/rlord-relay --config /tmp/rwd/relay/relay.toml serve   # prints the address
tailscale serve --bg --https=443 http://127.0.0.1:8787   # the first time, enable Serve at the link it prints
target/release/rlord-relay --config /tmp/rwd/relay/relay.toml token new

scripts/demo/demo.sh --web /tmp/rwd/t1 && source /tmp/rwd/t1/env.sh
rlord relay setup '<address>' --token <token>
rlord relay pair                    # scan with the app, check the code, confirm
```

`tailscale serve reset` takes it down. `relay.key` in `data_dir` is the
relay's identity: keep the folder, or every host sets the relay up again.

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

`scripts/demo/demo.sh --real-claude [--web] DIR` runs the real Claude Code
CLI in place of the stand-in `claude`, on a fake model, so its terminal
(streaming, permission prompts, hooks, transcripts) is real and nothing
calls a model. Codex and OpenCode stay stand-ins. `cargo xtask demo-claude
DIR` (which the demo runs) sets it up:

- **Pinned CLI.** `CLAUDE_CODE_VERSION` in `xtask/src/demo_claude.rs`,
  installed once with npm into `target/demo-claude/<version>` (needs
  `npm`). Your own `claude` and `~/.claude` are never used.
- **Fake model.** `cargo xtask fake-model` on a free 127.0.0.1 port,
  logging every request to `DIR/fake-model.log` (`demo.sh --stop` stops
  it). It echoes the prompt in a short stream; a prompt with the word
  "tool" gets a Bash call writing `fake-model-hello.txt` in the session's
  folder (so Claude Code asks first), and the tool's result a closing
  line. Requests with no tools (archive summaries, titles) get one line.
- **Safety env.** `DIR/bin/claude` drops every inherited `ANTHROPIC_*` /
  `CLAUDE*` variable, then sets `ANTHROPIC_BASE_URL` (the fake model), a
  dummy `ANTHROPIC_API_KEY`, `CLAUDE_CONFIG_DIR=DIR/claude`,
  `HOME=DIR/home`, `DISABLE_AUTOUPDATER`, `DISABLE_TELEMETRY`,
  `DISABLE_ERROR_REPORTING`, `DISABLE_GROWTHBOOK`,
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`,
  `CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL`,
  `CLAUDE_CODE_MODEL_CATALOG=off`, and points `HTTPS_PROXY` / `HTTP_PROXY`
  / `ALL_PROXY` at the fake model, which refuses anything sent that way
  and logs it as `BLOCKED` (`NO_PROXY` keeps 127.0.0.1 direct). A request
  with any other key or a bearer token is logged `auth=UNEXPECTED`.
- **No first-run screens.** The scratch `.claude.json` is seeded with
  `hasCompletedOnboarding` and `lastOnboardingVersion` (onboarding would
  also check api.anthropic.com), the dummy key's last 20 characters in
  `customApiKeyResponses.approved`, `projects.<DIR>.hasTrustDialogAccepted`
  (trusting DIR trusts every folder under it) and
  `hasSeenAutoDefaultNudge`. `settings.json` gets `permissions.defaultMode
  = "default"` (Claude Code's own default, auto mode, would skip the
  permission prompt), `disableRemoteControl` and the base URL in `env`.
  Bumping the pinned version: start a demo and check every session opens
  straight at its prompt; a new dialog needs another flag.

Drive it like any rlord session (`tmux -L <socket> send-keys`, the list,
the web page, `rlord-device pane <session> --state DIR` through a paired
relay) and `tail -f DIR/fake-model.log` to watch the requests.
`RLORD_FAKE_MODEL_DELAY_MS` slows its stream (`app test-term` uses it to
interrupt an answer). `cargo xtask app test-term` starts such a demo by
itself (its own tmux socket and web port, under `/tmp/rlord-term-<pid>`),
with the relay and the engine, and fails on a `BLOCKED` line.

The website's phone screenshots (`public/img/phone-*.webp` in
robot-lord/website) come from the same demo:

```sh
(cd ../website && npm ci && npx playwright install webkit)  # once
cargo xtask screenshots --website ../website
```

It starts the demo, opens the list in a 48-column terminal on a scratch
tmux server (`rlord-shots`), captures the list, the groups tab and the
overlord's pane with `tmux capture-pane -e`, turns each into an HTML page
and has Playwright screenshot it as an iPhone 15 would show it. The PNGs,
captures and pages stay in `target/screenshots`. Needs `tmux`, `cwebp`,
`sqlite3` and `python3`.

## Tests

```sh
cargo fmt --all && cargo clippy --workspace --all-targets && cargo test --workspace
```

Never touch the default tmux server in tests or scripts: rlord runs on its
own socket (`tmux -L rlord`), and tests use scratch sockets
(`RLORD_TMUX_SOCKET`). Tests that need tmux are gated: `RLORD_TMUX_TESTS=1
cargo test --workspace`.

## Versions and releases

Every PR bumps `[workspace.package] version` in `Cargo.toml` (and commits
`Cargo.lock`); `cargo xtask check-version-bump` enforces it (CI, and
`.githooks/pre-push` once you `git config core.hooksPath .githooks`).

- **Stable releases** `X.Y.Z`: what a tested PR bumps to. A feature bumps
  the minor (`1.0.0` → `1.1.0`), a fix the patch (`1.1.0` → `1.1.1`).
  Released for every platform, with the Homebrew formula and crates.io.
- **Development builds** `X.Y.Z-dev.N`: optional, for work that isn't ready
  yet: a pre-release of the next version (`1.2.0-dev.1`, then `-dev.2`, …).
  Released for macOS only, as GitHub pre-releases; `[update] channel =
  "dev"` installs them.

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
  as a pre-release. `build-release macos` signs with the keychain's
  "Developer ID Application" identity, by its hash (secure timestamp,
  identifier `com.robot-lord.rlord`), so macOS keeps privacy grants such as
  folder access across updates. The web plugin (`rlord-web`) is signed
  the same way, identifier `com.robot-lord.rlord-web`, before its hash is
  compiled into `rlord`. Without an identity it refuses outside CI.
  `RLORD_SIGN_IDENTITY` picks an identity by name or hash, or `-` signs
  ad-hoc on purpose. Stable releases are signed the same way in CI:
  release.yml's macOS job runs in the `release` environment, imports the
  `MACOS_SIGN_P12` / `MACOS_SIGN_P12_PASSWORD` secrets into a throwaway
  keychain, and checks the Developer ID signature of both shipped
  binaries (`rlord` and `rlord-web`). In CI
  without an identity, a publishing run (`RLORD_RELEASE=1`) fails; a
  release dry run, or a dev version elsewhere in CI, signs ad-hoc with a
  warning. Not notarized and no hardened runtime: `get.sh` downloads with
  curl, which sets no quarantine flag.
- **Stable releases** are built in GitHub Actions
  (`.github/workflows/release.yml`), so anyone can see how the binaries were
  made. `cargo xtask release` pushes the `vX.Y.Z` tag, which starts the workflow. A
  preflight job fails fast unless the tag is on `main`, stable and matches
  `Cargo.toml`. Then a macOS runner builds `aarch64-apple-darwin`; a Linux runner cross-compiles
  the static musl builds (`cargo zigbuild`), the relay server's
  (`rlord-relay-v<ver>-<target>.tar.gz`, Linux only) among them, and
  packages `.deb`/`.rpm`; both
  architectures are smoke-tested in Docker on Ubuntu, Debian, Fedora and
  Amazon Linux (`cargo xtask smoke-linux`); the release job writes `SHA256SUMS`,
  attests build provenance and creates the release as a **draft**. Then
  the `relay-image` job builds the relay's container image for
  `linux/amd64` and `linux/arm64` from those relay tarballs (`cargo xtask
  relay-image`, `deploy/relay/Dockerfile`), pushes it to
  `ghcr.io/robot-lord/rlord-relay` as `<version>` and `latest`, and
  attests it; a dry run only builds it.
  `cargo xtask release` waits for the run, checks the assets against `SHA256SUMS`
  and `gh attestation verify`, publishes the draft, then does what still needs
  local credentials: the Homebrew tap (`cargo xtask update-tap`) and crates.io
  (`cargo xtask publish-crates`, once the repo is public).

Before a stable release, run `cargo xtask fresh-install` (Docker; colima
on the Mac). It builds the Linux release files and installs them on fresh
Ubuntu, Debian and Fedora containers every way a user can (get.sh, the
`.deb`/`.rpm`, and the newest stable from GitHub upgraded in place), then
runs rlord as a new non-root user would: first run with no agent, then
with stand-ins for Claude Code, Codex and OpenCode on PATH (no real agent,
no model calls), sessions on rlord's own tmux server, `reset`, `update`,
and removing the package. `--dist DIR` reuses built files, `--offline`
skips GitHub, `--only` and image names narrow it; logs go to
`target/fresh-install/`. It tests Linux only: a macOS fresh install needs
a clean user account, by hand.

The build commands live in `cargo xtask build-release`, which CI and local
builds share; the version rules, targets and asset names in
`xtask/src/version.rs`; the packaging tools' versions (zig,
cargo-zigbuild, cargo-deb, cargo-generate-rpm) in `xtask/src/tools.rs`,
installed by `cargo xtask install-tools` into `target/release-tools`. The
Linux build runs only those copies and refuses other versions. A manual run of the workflow with `dry_run` (the default for
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
is already published and skips the build, so the two never race. That
skips the relay image too: push it from the published release with
`docker login ghcr.io`, then `cargo xtask relay-image --push` (needs
`docker buildx`; the image then has no GitHub attestation). If no
run shows up for a pushed tag, `cargo xtask release` also cancels any run queued
for it before suggesting `--local`. One-time setup for that:

```sh
brew install colima docker && colima start
cargo xtask install-tools   # again after a cargo clean or a pin change
```

`cargo xtask smoke-linux DIST [IMAGE…]` runs the Docker smoke test on its
own (images like `debian:12`, or `linux/amd64:ubuntu:24.04` for the
emulated x86_64 build); `--local` runs it on the default set.

Stable releases also need push access to `robot-lord/homebrew-tap` and
`cargo login` (crates.io token).

## License

By contributing you agree your work is licensed under MIT OR Apache-2.0, as
the project is.
