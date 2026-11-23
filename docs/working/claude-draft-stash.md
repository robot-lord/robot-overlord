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

## Owed / open

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
