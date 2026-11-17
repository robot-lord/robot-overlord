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
       their tokens into repo secrets is owed. Toolchain parity done
       (0.23.0-dev.24): `rust-toolchain.toml` pins Rust for CI, release
       and local builds; `cargo xtask install-tools` (`xtask/src/tools.rs`)
       pins zig, cargo-zigbuild, cargo-deb and cargo-generate-rpm, prebuilt
       and checksum-checked where upstream ships a binary
       (cargo-generate-rpm is still `cargo install`ed: no upstream
       binaries).
4. [x] Release automation in `cargo xtask` (0.23.0-dev.8; the `xtask`
       crate): release, build, tap, crates.io and the version-bump check
       are Rust with unit tests; the version rules, targets and asset
       names are in `xtask/src/version.rs` only. Shell is left for simple
       glue and for what users run on a bare machine (`get.sh`,
       `install.sh`, `update.sh`). The smoke test and dev loop moved too
       (0.23.0-dev.24: `cargo xtask smoke-linux`, `cargo xtask
       dev-daemon`; the scripts are thin wrappers). Owed: the
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
7. [x] Codex daemon mode (shipped, 0.23.0-dev.12). Codex Remote Control
       deferred past 1.0 (Brandon, 2026-11-13): wait until Codex makes it
       official. Findings and branch `codex-remote`:
       `docs/working-archive/codex-daemon.md`. Open until these live
       checks are done:
       - [x] a daemon-mode session's hooks through the real daemon into
         rlord (the payload fields, a first SessionStart bound by
         folder). Done 2026-11-14 on 0.23.0-dev.21, codex-cli and daemon
         0.169.0, macOS: SessionStart, UserPromptSubmit and Stop ran in
         the daemon's `codex app-server --managed-daemon` process with no
         `RLORD_SESSION_ID`, `TMUX` or `TMUX_PANE`, passed the gate,
         bound the session by folder (`agent_session_trust = hook`) and
         set it to "your turn". Run in a scratch rlord with its own
         `CODEX_HOME`, so a second daemon (the real Codex binary, started
         by rlord's `ensure_running`): the hooks a daemon runs come from
         its `CODEX_HOME` and go to the rlord its environment names, so
         the user's own daemon would have sent them to the installed
         rlord. Payloads and the bug it found (a `codex exec fork`
         `SessionStart` could bind a waiting session; fixed in
         0.23.0-dev.25): `docs/working-archive/codex-daemon.md`;
       - [x] a daemon restart or update mid-turn, by hand (what the session
         shows; `notLoaded` should read as not running, so it stops once
         the pane is gone). Done 2026-11-16 on 0.23.0-dev.29, codex-cli
         and daemon 0.169.0, macOS, scratch rlord and `CODEX_HOME` as
         above (`daemon restart` only; `update` not run, nothing newer).
         `restart` waits up to about 60s for a running turn, then
         interrupts it (`turn_aborted`, `interrupted`), and the new daemon
         at once starts the turn again on the same thread with no client
         and no new prompt (the model re-ran its command). So the thread
         stays loaded and `notLoaded` never came up; rlord was right
         anyway: with the pane gone it stayed "working" while the
         resumed turn ran and went "stopped" (`session_end`) within a
         `discover` pass of `task_complete`. With the pane up, the TUI
         showed "Conversation interrupted", then "Working" again, then
         the answer. Details: `docs/working-archive/codex-daemon.md`.
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
10. [ ] Added 2026-11-16, before the 1.0 cut:
       - [ ] #66 (`repo_latest`, fresh code for the overlord and new
         sessions) and #67 (Developer ID signing, installers that fall
         back when there is no stable release).
       - [ ] Back-version the old stable tags (approved by the user): rename
         `v0.2.0`…`v0.18.1` to `vX.Y.Z-dev.0`, delete the `v0.18.1`
         release and remove the Homebrew formula. Do this after #67 is
         released. Linux and Homebrew have no install until 1.0.0.
       - [ ] `cargo xtask fresh-install`: its "newest stable from GitHub,
         upgraded in place" run needs a stable release, and there won't be
         one. Upgrade from the newest release instead.
       - [x] Sign stable releases in CI (#67). Still owed by the
         maintainer: the one-time secret and environment setup in
         `docs/working/signing.md`, then a dry run, or 1.0.0's release
         run fails.
       - [ ] Session context PRs 2–4 (`docs/working/session-context.md`).
         They change how prompts and messages reach sessions, so settle
         them before calling it stable. They include the #64/#65
         leftovers and the accumulating pending message.
       - [ ] Website copy pass (`robot-lord/website`): bring every page up
         to date with what shipped since it was last written. That covers
         the overlord on any of the three agents, session background,
         forking, fresh repo code and signed builds.
       - [ ] By hand, by the user: the item 9 macOS fresh install.
       - [ ] Not yet placed before or after 1.0 (the user decides): a
         "waiting on background work" status. Today a session whose
         agent stopped while background shells or subagents still run
         shows "your turn". Count the launched background tasks in the
         transcript against their completion notifications, with
         Subagent hooks as a faster signal and plain code only. Fall back
         to "your turn" when the transcript stops growing. Claude
         first; Codex and OpenCode later.
11. [ ] 1.0.0 stable.

After 1.0:
- 1.x features: an overlord that switches agent when it hits a usage limit,
  and `[limits] auto_resume`, with a must-choose prompt when it isn't set.
- An AUR package, and the Homebrew tap and crates.io tokens as repo secrets
  (to do later, not part of the CI release builds step).
- **Versioning changes at 1.0 (user, 2026-11-16).** A dev build no longer
  claims the next minor version. Every dev build is a pre-release of the
  next patch (after `1.0.0`: `1.0.1-dev.N`), and the stable cut picks
  `1.0.1` or `1.1.0` to fit what changed. At the cut, update CLAUDE.md
  ("Releases"), CONTRIBUTING.md, `xtask/src/version.rs` and
  `check-version-bump` to match. Until then the 0.x scheme stays.

## Before flipping it public

The repository is already public (checked 2026-11-14); the open items
below still stand before announcing it.

- [ ] Stable release (`X.Y.Z`, no suffix) so `get.sh`, Homebrew and the
      stable channel stop installing 0.18.1. Needs `colima start` for the
      Linux smoke tests; check OpenCode and Codex on Linux in them.
      Still 0.18.1 (checked 2026-11-14: the latest non-pre-release).
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
- [x] Wrap up stale working docs (2026-11-14): `usage-column.md`,
      `own-prompts.md` and `linux-and-packaging.md` reconciled and
      archived (`docs/working-archive/README.md`).

Done (2026-11-12): `main` ruleset fixed (it targeted no branch; now PR +
`check`/`version-bump`/`advisories` required, no force-push or deletion,
admin bypass); `v*` tags protected; Dependabot alerts and security
updates; Actions limited to GitHub's own plus pinned third-party ones;
squash-only merges; branches deleted on merge; wiki and projects off;
`SECURITY.md`; CI pinned to SHAs with read-only token and a RustSec check
(rusqlite 0.42 for the bundled-SQLite CVE; `paste` ignored with a reason);
Dependabot version updates, grouped weekly.

## Right after flipping it public

- [x] Re-create the `main protect` and `release tags` rulesets (lost: a free
      org can't use rulesets on a private repo; the repo moved to
      `robot-lord` on 2026-11-12). Same rules as before: PR + `check`,
      `version-bump`, `advisories` required; no force-push or deletion;
      admin bypass; squash only; `v*` tags can't be deleted or moved.
      Done (checked 2026-11-14): both are active with those rules, except
      that neither has a bypass actor (no admin bypass). Adding one is the
      owner's call.
- [x] `scripts/go-public.sh`: secret scanning + push protection, private
      vulnerability reporting, approval for workflows on outside PRs (all
      unavailable on private repositories). Done (checked 2026-11-14):
      all on, approval for all external contributors.
- [ ] Publish to crates.io (`cargo xtask release` does it once public).
      Not yet (checked 2026-11-14): none of `robot-overlord`,
      `robot-overlord-core`, `robot-overlord-daemon`,
      `robot-overlord-tui` exist on crates.io. It happens with the first
      stable release.
- [x] README links that 404 while private: `/releases`, raw `get.sh`.
      Both return 200 (checked 2026-11-14).
- [x] Move the tracked items below into GitHub issues (2026-11-14).

## After launch (tracked)

Tracked as GitHub issues (filed 2026-11-14):

- Codex daemon mode, from "After launch (owed, daemon mode)" in
  `docs/working-archive/codex-daemon.md`:
  [#40](https://github.com/robot-lord/robot-overlord/issues/40) approval
  raised in another client,
  [#41](https://github.com/robot-lord/robot-overlord/issues/41) `/clear`
  and `/new`,
  [#42](https://github.com/robot-lord/robot-overlord/issues/42) a
  sub-agent's permission prompt,
  [#43](https://github.com/robot-lord/robot-overlord/issues/43)
  `send_to_session` via `codex queue`,
  [#44](https://github.com/robot-lord/robot-overlord/issues/44) the Codex
  overlord on the daemon,
  [#45](https://github.com/robot-lord/robot-overlord/issues/45) Linux,
  [#46](https://github.com/robot-lord/robot-overlord/issues/46) a daemon
  thread's environment,
  [#47](https://github.com/robot-lord/robot-overlord/issues/47) a daemon
  started by hand from an rlord pane,
  [#48](https://github.com/robot-lord/robot-overlord/issues/48) a daemon
  started by a pane's TUI,
  [#49](https://github.com/robot-lord/robot-overlord/issues/49) a session
  that fell back to the pane,
  [#50](https://github.com/robot-lord/robot-overlord/issues/50)
  `type_message`'s read deadline.
- [#51](https://github.com/robot-lord/robot-overlord/issues/51) Switching
  an existing overlord to another agent.
- Known gaps, accepted for now (Brandon, 2026-11-14: leave them):
  [#52](https://github.com/robot-lord/robot-overlord/issues/52) Codex
  subagents get every overlord tool;
  [#53](https://github.com/robot-lord/robot-overlord/issues/53) OpenCode
  subagents get `ask`, not `deny`.
- [#54](https://github.com/robot-lord/robot-overlord/issues/54) A hooked
  Codex session whose tmux session is killed stays "working".
- [#55](https://github.com/robot-lord/robot-overlord/issues/55) Codex
  Remote Control, once Codex makes it official.
- [#56](https://github.com/robot-lord/robot-overlord/issues/56)
  Context-window table (`models::context_window`) upkeep.
- [#57](https://github.com/robot-lord/robot-overlord/issues/57) AUR
  package.
