# Deliver to Claude past a draft (prompt stash)

**Status:** in progress (PR #91 open, not merged)

## Problem

`send::type_into` refused (or, with `when_idle`, held indefinitely) any
message while the agent's input held unsent text. A user who left half a
thought in a Claude session's box blocked every overlord message to it.

## Decision (user)

For Claude, use Claude Code's native prompt stash (`chat:stash`, Ctrl+S)
so delivery isn't blocked by a draft. Codex and OpenCode unchanged.
Guiding rule (review): never lose a draft; a held message is fine.

## Live findings

- 2.1.327: Ctrl+S with text stashes it and clears the box ("draft
  stashed"); the stash is one slot (stashing again lost the first).
- 2.1.331 (end-to-end on 9d81d4f, scenarios A-C passed): Claude Code
  restores the stashed draft by itself ~2s after the delivered turn
  ("draft restored"). The earlier "draft restored · draft stashed" with
  an empty box was a manual Ctrl+S re-stashing after that auto-restore.
- "draft stashed" stayed up ~13s across a turn once (not relied on);
  "draft stashed · ✔ Update installed" rendered right above the top rule.

## Current design (round 3)

See `docs/architecture.md` ("Claude drafts: the prompt stash") and the
header of `crates/daemon/src/stash.rs`. In short: stash only when safe
(shared `claude::input_box`), poll after the press, record then type;
rlord never presses Ctrl+S to restore; while a record is live every
message to the session holds; the record clears when the draft shows
back, the user sends their own prompt, or `rlord stash-dismiss`; `⎘`
after the turn + 60s; `draft_lost_with_session` when the session goes.

Earlier rounds' press-to-restore, retry backoff and give-up timer were
removed in round 3.

## Owed / open

- Review 4 item 5: a user's own stash whose "draft stashed" notice has
  faded can be overwritten by rlord's stash (rlord can only see the
  indicator).
- Review 4 item 7: the first direct send (no `when_idle`) to a session
  with a draft is always refused, because the draft's age starts at
  rlord's first look. With `when_idle` it is queued and goes once the
  draft has sat 30s.
- Not unit-tested (tmux path of `type_into`): the `back_off` call on
  Untouched, record-before-paste order, `stash::sent` after Enter,
  `mark_stale` after a failed paste, no record when Ctrl+S's send_keys
  fails, whole-box hashing at the call site, `TYPING` held only around
  the key.
- Cleanups: `after_press` takes a `pressed` flag the caller always
  passes as true (its false arm is the early return in `type_into`);
  `Why` and `Kind` could live in their own module; the mutation checks
  were run by hand with a scratch script, not kept in the repo.
- Only the first visual row is stored; a draft whose first row is
  identical to unrelated later text could read as "back".
- This head is not yet live-tested end to end.
