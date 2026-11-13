**Status:** in progress

# Making the repository public

## Road to 1.0

The first stable release is 1.0.0, cut only after everything below is
done, in this order (decided with the user, 2026-11-12):

1. [ ] Linux Codex sandbox check (below, "Before flipping it public").
2. [x] rlord's own pastes no longer count as the user's prompts
       (0.23.0-dev.5; `daemon/src/own.rs`).
3. [ ] CI release builds: macOS on a Mac runner, Linux on a Linux runner.
4. [ ] Limits completion: Codex and OpenCode limit detection, cancelling
       a scheduled resume, a "limited" list filter, Option-S on the keys
       screen.
5. [ ] Robustness: OpenCode draft detection, bounded sub-agent reads,
       narrower helper tools.
6. [ ] Codex daemon mode, with Codex Remote Control (whether it is in
       1.0 is the user's call, still open).
7. [ ] Polish: clickable "more" markers, groups-tab counts, light-theme
       contrast, website phone screenshots.
8. [ ] A fresh-install end-to-end test.
9. [ ] 1.0.0 stable.

After 1.0: an AUR package, and the Homebrew tap and crates.io tokens as
repo secrets (to do later, not part of the CI release builds step).

## Before flipping it public

- [ ] Stable release (`X.Y.Z`, no suffix) so `get.sh`, Homebrew and the
      stable channel stop installing 0.18.1. Needs `colima start` for the
      Linux smoke tests; check OpenCode and Codex on Linux in them.
- [ ] Codex overlord's read-only sandbox on Linux: confirm it refuses
      unix-socket connects (`codex sandbox`, e.g. in Docker). If not, fix
      or document before Linux users get a Codex overlord.
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
