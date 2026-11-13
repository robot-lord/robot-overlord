**Status:** in progress

# Making the repository public

## Road to 1.0

The first stable release is 1.0.0, cut only after everything below is
done, in this order (decided with the user, 2026-11-12):

1. [ ] Linux Codex sandbox check. Partial (0.23.0-dev.4): blocked as
       root in a privileged container; the check on real Ubuntu as a
       non-root user is still open (below, "Before flipping it public").
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
5. [ ] Limits completion: Codex and OpenCode limit detection, cancelling
       a scheduled resume, a "limited" list filter, Option-S on the keys
       screen.
6. [ ] Robustness: OpenCode draft detection, bounded sub-agent reads,
       narrower helper tools.
7. [ ] Codex daemon mode, with Codex Remote Control (whether it is in
       1.0 is the user's call, still open).
8. [ ] Polish: clickable "more" markers, groups-tab counts, light-theme
       contrast, website phone screenshots.
9. [ ] A fresh-install end-to-end test.
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

- OpenCode: a draft in the input box doesn't hold a `send_to_session`.
- Switching an existing overlord to another agent.
- Narrower tool lists for Codex/OpenCode overlord subagents.
- `session.subagents` reads unread sub-agent transcripts in full.
- A hooked Codex session whose tmux session is killed stays "working".
- Codex daemon mode (create-then-attach), which also brings Codex Remote
  Control.
- "↑/↓ N more" can't be clicked; the Groups tab has no more-counts.
- `accent_dim` contrast on light themes.
- Context-window table (`models::context_window`) upkeep.
- AUR package.
