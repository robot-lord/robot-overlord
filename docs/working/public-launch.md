**Status:** in progress

# Making the repository public

## Road to 1.0

The first stable release is 1.0.0, cut only after everything below is
done, in this order (decided with the user, 2026-11-12):

1. [x] Linux Codex sandbox check. Partial (0.23.0-dev.4): blocked as
       root in a privileged container. The user called this good enough
       for 1.0 (2026-11-13); the README says testing has been mostly
       macOS with Claude Code and asks for bug reports. Linux fixes come
       after release if people report them.
2. [x] rlord's own pastes no longer count as the user's prompts
       (0.23.0-dev.5; `daemon/src/own.rs`).
3. [x] CI release builds: macOS on a Mac runner, Linux on a Linux runner
       (0.23.0-dev.6; `.github/workflows/release.yml`, shared
       `cargo xtask build-release`). Tap and crates.io stay local; moving
       their tokens into repo secrets is owed. Also owed (PR #25 review):
       toolchain parity for `--local` (a `rust-toolchain.toml`, pinned
       zig/cargo-zigbuild/cargo-deb/cargo-generate-rpm versions in
       CONTRIBUTING to match release.yml); prebuilt packaging tools in CI
       instead of `cargo install` on every run.
4. [x] Release automation in `cargo xtask` (0.23.0-dev.8; the `xtask`
       crate): release, build, tap, crates.io and the version-bump check
       are Rust with unit tests; the version rules, targets and asset
       names are in `xtask/src/version.rs` only. Shell is left for simple
       glue and for what users run on a bare machine (`get.sh`,
       `install.sh`, `update.sh`). Owed: `scripts/smoke-linux.sh` and
       `scripts/dev-daemon.sh` still hold some logic in bash; the
       stable paths of `cargo xtask release` (CI, `--resume`, `--local`,
       tap, crates.io) have unit tests and a dry run but get their first
       real run at 1.0.0. SIGTERM ends xtask without its cleanup
       (`publish-crates` leaves the manifests pinned); Ctrl-C and SIGHUP
       unwind.
5. [x] Limits completion (0.23.0-dev.18): cancelling a scheduled resume
       (`resume_after_limit` `cancel`, `rlord resume-after-limit
       --cancel`, the list's menu and `:cancel resume`), a
       `status:limited` list filter, and Option-S (`[tmux]
       overlord_switch`) on the keys screen, where it was missing.
       Codex and OpenCode limit detection (0.23.0-dev.11) came from
       their source at Codex 0.167.2 and OpenCode 2.2.7 with
       hand-written fixtures; no real limit was hit for it. Live
       confirmations still owed (check the first real one against
       `docs/architecture.md`, "Usage limits"): whether Codex's 429
       snapshot carries `resets_at` (its tests send none; rlord then
       takes the window's reset from an earlier `token_count`, so a
       limit hit on a session's first request after a restart, with no
       earlier snapshot in the tail, isn't seen), and whether OpenCode
       Go's `retry-after` is the time to the limit's reset (OpenCode's
       own timeline takes it so). Not detected, no reset recorded:
       Codex API-key quota, OpenCode `provider.chatgpt-limit` and
       `provider.quota`, and other providers' 429s (OpenCode retries
       those as throttles).
6. [x] Robustness (0.23.0-dev.19): a draft in OpenCode's prompt box
       holds a send (`opencode::draft`, from a live capture of 2.2.7:
       the placeholder is in the muted colour of the agent line's `·`);
       `session.subagents` reads only sub-agent transcripts at most 4 MB
       behind and queues the rest on the warm-up; rlord's subagents on
       OpenCode are allowed only the rlord tools their definition lists
       (`agents.<name>.permissions` in `opencode.json`, everything else
       asks). Codex's subagents still inherit the overlord's tools: a
       `.codex/agents/*.toml` can't narrow MCP tools (see "After
       launch"). Not checked live: a draft held in a running OpenCode
       session through rlord, and a subagent asking for an unlisted tool
       on OpenCode.
7. [ ] Codex daemon mode (shipped, 0.23.0-dev.12). Codex Remote Control
       deferred past 1.0 (Brandon, 2026-11-13): wait until Codex makes it
       official. Findings and branch `codex-remote`:
       `docs/working-archive/codex-daemon.md`. Open until these live
       checks are done:
       - a daemon-mode session's hooks through the real daemon into
         rlord, once a release with the gate is installed (the payload
         fields, a first SessionStart bound by folder);
       - a daemon restart or update mid-turn, by hand (what the session
         shows; `notLoaded` should read as not running, so it stops once
         the pane is gone).
       Linux is not a blocker (as in item 1): it is on the "After launch
       (owed, daemon mode)" list in `docs/working-archive/codex-daemon.md`.
8. [x] Polish: clickable "more" markers, groups-tab counts, light-theme
       contrast, website phone screenshots.
       Done in 0.23.0-dev.20: the "↑/↓ n more" counts page the list when
       clicked; the groups tab's group column counts its hidden groups the
       same way (also clickable); `accent_dim` on every light theme now
       passes WCAG AA (4.5:1) against its background, checked by a test.
       Done in 0.23.0-dev.21: phone screenshots on the website, made by
       `cargo xtask screenshots` from the demo fleet (a 48-column
       terminal rendered by Playwright). Under 60 columns the list now
       goes narrow (names first, Status as its glyph, the time as its
       value) and the groups column drops its counts.
9. [ ] A fresh-install end-to-end test. Linux done (0.23.0-dev.23):
       `cargo xtask fresh-install` (CONTRIBUTING has the usage) installs
       on throwaway ubuntu:24.04, debian:stable and fedora:latest
       containers via get.sh (local files), the .deb/.rpm, and the newest
       stable from GitHub upgraded in place, then runs rlord as a new
       non-root user: first run with no agent (list, setup dialog,
       `agents`, `new` refusing), then stand-in claude/codex/opencode on
       PATH (`install-hooks`, a session each on rlord's own tmux server,
       stop, `reset`, `update`, package removal). All 9 runs pass, and
       x86_64 under emulation (tarball, deb). Found and fixed: `rlord
       reset` hung on the old daemon's zombie where PID 1 doesn't reap
       (containers, devcontainers); upgrades kept the old daemon where
       there is no `ps` (Debian's image); `rlord update` on a .deb/.rpm
       (or Homebrew, cargo) install put a second, older copy in
       ~/.local/bin; the setup dialog called a missing agent "the only
       one installed"; get.sh now says when no agent is installed; Mac-
       built tarballs carried xattrs GNU tar warned about. Not done:
       **macOS fresh install** needs a clean user account, by hand.
       Noted, not changed: `rlord overlord status` with no agent says it
       will create one on claude/opus; under qemu the list can't start
       (ratatui ENOSYS: emulation only); the 0.18.1 stable tarballs
       carry those xattr warnings.
10. [ ] 1.0.0 stable.

After 1.0: an AUR package, and the Homebrew tap and crates.io tokens as
repo secrets (to do later, not part of the CI release builds step).

## Before flipping it public

- [ ] Stable release (`X.Y.Z`, no suffix) so `get.sh`, Homebrew and the
      stable channel stop installing 0.18.1. Needs `colima start` for the
      Linux smoke tests; check OpenCode and Codex on Linux in them.
- [ ] Codex overlord's read-only sandbox on Linux. Partial: it blocked a
      unix-socket connect as root on a `/tmp` socket (checked 2026-11-12,
      codex-cli 0.167.2 from npm, `ubuntu:24.04` arm64 on colima). Run
      as root in `docker run --rm --privileged ubuntu:24.04` (root and
      `--privileged` were the only way bwrap could start there), with
      `codex sandbox` (check `codex sandbox --help` for the exact form;
      it doesn't exercise the overlord's on-request escalation) against a live `tmux -L tsock`
      (`/tmp/tmux-0/tsock`):
      - `tmux list-sessions` and `send-keys`: `error connecting to
        /tmp/tmux-0/tsock (Operation not permitted)`; pane untouched.
      - Python `AF_UNIX` connect and a TCP `socket()`: `PermissionError:
        [Errno 1] Operation not permitted` (whether at `socket()` or
        `connect()` wasn't told apart).
      - writes to `/tmp` and the workdir: `Read-only file system`.

      Not covered: a non-root user and rlord's real socket path
      (`/tmp/tmux-<uid>/rlord`); that's the next item. As non-root,
      bwrap wouldn't start in this setup (Docker's default seccomp: `No
      permissions to create a new namespace`; with `--privileged` or
      seccomp/apparmor unconfined, the colima VM's
      `kernel.apparmor_restrict_unprivileged_userns=1`: `bwrap: loopback:
      Failed RTM_NEWADDR: Operation not permitted`). x86_64 under QEMU
      couldn't create namespaces either.
- [ ] Codex overlord on a real Ubuntu 24.04 host
      (`apparmor_restrict_unprivileged_userns=1`): does bwrap start, as
      non-root, with rlord's real socket path? If it doesn't, every shell
      command fails (closed, but unusable): document it, and see the
      owed detection item in `codex-support.md`.
- [ ] Fresh install end to end on a clean machine or account: `get.sh` and
      `brew install`, then first-run setup per agent, including a machine
      without Claude Code.
- [ ] Wrap up stale working docs (usage-column: shipped).

Done (2026-11-12): `main` ruleset fixed (it targeted no branch; now PR +
`check`/`version-bump`/`advisories` required, no force-push or deletion,
admin bypass); `v*` tags protected; Dependabot alerts and security
updates; Actions limited to GitHub's own plus pinned third-party ones;
squash-only merges; branches deleted on merge; wiki and projects off;
`SECURITY.md`; CI pinned to SHAs with read-only token and a RustSec check
(rusqlite 0.42 for the bundled-SQLite CVE; `paste` ignored with a reason);
Dependabot version updates, grouped weekly.

## Right after flipping it public

- [ ] Re-create the `main protect` and `release tags` rulesets (lost: a free
      org can't use rulesets on a private repo; the repo moved to
      `robot-lord` on 2026-11-12). Same rules as before: PR + `check`,
      `version-bump`, `advisories` required; no force-push or deletion;
      admin bypass; squash only; `v*` tags can't be deleted or moved.

- [ ] `scripts/go-public.sh`: secret scanning + push protection, private
      vulnerability reporting, approval for workflows on outside PRs (all
      unavailable on private repositories).
- [ ] Publish to crates.io (`cargo xtask release` does it once public).
- [ ] README links that 404 while private: `/releases`, raw `get.sh`.
- [ ] Move the tracked items below into GitHub issues.

## After launch (tracked)

- Codex daemon mode: "After launch (owed, daemon mode)" in
  `docs/working-archive/codex-daemon.md` (one issue each).
- Switching an existing overlord to another agent.
- Known gaps, accepted for now (Brandon, 2026-11-14: leave them):
  - Codex subagents get every overlord tool: a `.codex/agents/*.toml`
    can't narrow MCP tools.
  - OpenCode subagents get `ask` (not `deny`) for tools outside their
    list: `deny` in that format is unconfirmed.
- A hooked Codex session whose tmux session is killed stays "working".
- Codex Remote Control, once Codex makes it official (deferred; see
  `docs/working-archive/codex-daemon.md`).
- Context-window table (`models::context_window`) upkeep.
- AUR package.
