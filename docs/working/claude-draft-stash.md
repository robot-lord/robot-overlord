# Deliver to Claude past a draft (prompt stash)

**Status:** in progress (PR open, not merged)

## Problem

`send::type_into` refused (or, with `when_idle`, held indefinitely) any
message while the agent's input held unsent text. A user who left half a
thought in a Claude session's box blocked every overlord message to it.

## Decision (user)

For Claude, use Claude Code's native prompt stash (`chat:stash`, Ctrl+S)
so delivery isn't blocked by a draft. Codex and OpenCode unchanged.

## Live findings (Claude Code 2.1.327, scratch tmux)

- Ctrl+S with text stashes it and clears the box; "draft stashed" shows.
  On an empty box it restores text, cursor, pasted content, input mode.
- One slot: stashing again overwrote the first stash (lost).
- draft, Ctrl+S, paste, Enter, Ctrl+S restore worked; Ctrl+S ~1s after
  Enter did not ("draft restored · draft stashed", box empty, still
  stashed); once settled it restored. Restore mid-turn untested.

## What shipped in this PR

- `crates/daemon/src/stash.rs`: pure decisions (`plan`, `restore_step`,
  `stash_shown`, `stash_bound`), draft-age tracker, restore on `send`'s
  tick, kv `draft_stash`, events `draft_stashed` / `draft_restored` /
  `draft_restore_failed`, row field `draft_stash`.
- `send::type_into`: Claude draft goes through `stash::plan`; press,
  verify box empty + indicator, paste, Enter, record.
- Held messages keep their latest reason (`held`), rows get `mail`; TUI
  marks `✉`, `✉ draft`, `⎘` (restore failed).
- overlord.md, mcp.rs `send_to_session`, guide, architecture.

Constants: `DRAFT_IDLE` 30s, restore needs 3s idle + `SETTLE`, 6 presses,
30 min bound, failed mark kept 2h or until the draft shows back.

## Review round 1 (applied)

Stash recorded right after the C-s check (before paste); an empty box
without the indicator is recorded and not typed; restore presses on an
empty box even without the indicator, and a press that brings nothing
back drops quietly (`draft_restore_unknown`); failed records don't block
new stashes; indicator read only on status lines; draft age by the whole
box, reset by user activity and leaving idle; `held` lifts when the
session leaves idle; hold kind carried as `Why::draft` / `held_draft`;
`chat:stash` on any other key means no stash.

Live (coordinator): "draft stashed" stayed up ~13s across a turn; C-s on
an empty box with nothing stashed appears to be a no-op;
"draft stashed · ✔ Update installed" rendered right above the top rule.

## Review round 2 + live test (applied)

Rule: never lose a draft; a held message is fine.

- Live (2.1.331, scenarios A-C passed): Claude Code restores the stashed
  draft by itself ~2s after the delivered turn; rlord's press never ran
  (`draft_restored by: other`). The 2.1.327 "draft restored · draft
  stashed" with an empty box was a manual C-s re-stashing after that
  auto-restore. So the restore presses only on an empty box whose latest
  status is "draft stashed", after idle + a few seconds, with a fresh
  look right before; no indicator and not our draft = dropped quietly
  (round 1's "press once without the indicator" removed).
- After the stash's C-s, the screen is polled (~2s): box empty = stashed
  (recorded, then paste + Enter in the same call). Text still there =
  10 min in-memory back-off, message held. RecordOnly-without-typing
  removed (only an unreadable screen records without typing).
- Failed records block new stashes again (round 1 change reverted);
  lifted when the draft shows back or `rlord stash-dismiss`
  (`session.stash_dismiss`). Reason shown in the hold text.
- Whole-first-line match; cancel_mode before restore's C-s; tick runs
  prune + restore even when a delivery errors; `held_logged` separate
  from the reason, `d.changed()` on lift; chords starting with ctrl+s
  mean no stash; `settled_since_typed`/`typing()` in type_into,
  `agent_pane` shared, keybindings read only after the age check.

## Owed / open

- No unit test reaches `type_into`'s tmux sequence: the back-off call on
  Untouched, record-before-paste ordering, and whole-box hashing are
  covered only through pure functions. Same for `cancel_mode` before the
  restore press and tick continuing past a delivery error.
- The `type_into` stash path (C-s, `after_stash`, record before paste,
  whole-box age) has pure-logic tests only; the tmux sequence itself is
  untested.
- A user's own stash made while the indicator isn't visible could be
  overwritten by rlord's stash (it can only see the indicator).

- Untested live: the whole rlord flow end to end against a real Claude
  pane; restore while Claude is mid-turn (we never try it: we wait for
  idle); whether "draft stashed" stays on screen the whole time the stash
  is held (assumed; if it is transient, rlord refuses to stash again only
  on its own pending record, and declares a restore "lost" and marks the
  row instead of pressing).
- Indicator read from 2 lines above the last `❯` line to the bottom; a
  Claude UI change that moves it would make rlord stop stashing (safe
  direction) and give up restores early (marked, not lost).
- Multi-line drafts: only the first line is compared on restore.
