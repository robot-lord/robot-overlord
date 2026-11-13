**Status:** in progress

# Making the repository public

## Before flipping it public

- [ ] Stable release (`X.Y.Z`, no suffix) so `get.sh`, Homebrew and the
      stable channel stop installing 0.18.1. Needs `colima start` for the
      Linux smoke tests; check OpenCode and Codex on Linux in them.
- [x] Codex overlord's read-only sandbox on Linux: blocks unix-socket
      connects (checked 2026-11-12, codex-cli 0.167.2, ubuntu:24.04 arm64
      in Docker, `codex sandbox -c sandbox_mode="read-only" -c
      approval_policy="on-request" -- …` against a live `tmux -L tsock`):
      `tmux list-sessions` and `send-keys` fail with `error connecting to
      /tmp/tmux-0/tsock (Operation not permitted)`; a Python `AF_UNIX`
      connect and a TCP `socket()` both raise `PermissionError: [Errno 1]`
      (seccomp, not only a netns); writes to `/tmp` and the workdir fail
      with `Read-only file system`. No fix needed. Notes:
      - Where unprivileged user namespaces are blocked (Ubuntu 24.04's
        `kernel.apparmor_restrict_unprivileged_userns=1`, Docker's default
        seccomp), bwrap can't start and every sandboxed command fails
        (`bwrap: loopback: Failed RTM_NEWADDR` / `No permissions to create
        a new namespace`). Fails closed, but a Codex overlord on stock
        Ubuntu 24.04 may be unable to run any shell command. Check this
        on a real Ubuntu host before calling Codex-on-Linux supported, and
        document it if so.
      - x86_64 not exercised: under QEMU bwrap couldn't create
        namespaces either. Same sandbox code; low risk.
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
- [ ] Publish to crates.io (`scripts/release.sh` does it once public).
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
