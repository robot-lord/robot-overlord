**Status:** parked

# Stale stored statuses

A stored status moves only when an event moves it, so a clearing event
that never comes leaves it wrong. The attention-queue audit found a
session reading `waiting_input` 158 hours after a stray notification set
it, interrupted turns reading `working` (Claude runs no `Stop` hook on
Escape), and crashed agents leaving a live status on a dead pane.

## Shipped (#82, 1.13.2)

Dead-pane detection only (`daemon/src/recheck.rs`; rules in
`docs/architecture.md`, "Stale statuses re-checked"). The user decided
to cut #82 down to that after seven review rounds kept finding ways the
transcript rules could clear a real "needs you".

## Deferred: transcript-based clearing waits for attention shadow data

Built and reviewed in #82, then removed. Bring back only once the
attention queue's shadow run (`rlord attention report`) shows how often
statuses end stale, and how:

- `working` to `idle` when the transcript's last turn line ended or
  interrupted the turn (Claude's `[Request interrupted by user…]`,
  Codex `turn_aborted`), written after the status change, quiet for
  2 minutes; a final reply dates `last_agent_msg_at`.
- `waiting_input` to `idle` only when the user moved on: Escape at the
  prompt (an interrupt after the wait began), or a prompt of theirs after
  it whose turn ended; never while a sub-agent or another thread is named
  as waiting. Dates `last_user_msg_at` only from a typed prompt, never a
  Codex turn start, and not when rlord typed since the wait began.
- The transcript reading it needed (`transcript::tail`, `Tail`,
  `TurnTail`): one backward pass; an allow-list of local output (`!`
  shell lines, `<local-command-…>`, compact summaries); a slash command's
  wrapper is a turn unless its local output follows (only a turn line
  clears that, not bookkeeping lines); custom commands count as typed
  prompts; meta lines (another session's or a coordinator's message, Stop
  hook feedback) may start a turn; Codex v2 `turn_started` /
  `turn_complete`.
- Proving a dead tmux server from its pid (a pid can be reused: needs a
  check that the process is still tmux) instead of no verdict.
- Following a pinned pane moved by `join-pane`, and reading a pin whose
  pane is gone as dead.
- The 158 h case itself: needs a rule the review accepts; today the user
  clears it (a new prompt in that session, or `rlord restart <session>`).

## Owed

- A dead tmux server while the daemon runs: "no server running" gives no
  verdict, so its sessions stay live until the next daemon start's
  reconcile. Stopping them needs proof the server is down for good: a
  deleted socket file doesn't prove it (a server can outlive its socket
  file), the daemon doesn't own the server's lifetime, and a pid can be
  reused.
- A pin whose pane is gone falls back to the first pane (as
  `window_pane_ids` does): an unrelated split there reads as the agent.
- `discover`'s own pane check on `agent_panes` (marks, pane ids, the
  pin).
- The "still as read" guard could move into `discover::stop_from_pane`.
- Dead branches left by the cut.
- A shared "every N seconds" loop helper for the watchers.
- The startup reconcile and `discover` still go by tmux names, not marks
  (as before #82): a rename outside rlord fools them.
- A session whose tmux session sits unmarked under its stored name (made
  before marks, or the mark failed) is found by name only.
- One helper writes the pin (`pin_agent_pane`, `respawn_pane`).
- `find` runs twice a session a pass: once is enough.
- `Memo.gone`'s tuple wants a struct.
- A tail `(len, mtime)` cache and a lazy reverse parse (for the
  deferred rules).
- `pid_alive` is written three times (`remote.rs`, `autostart.rs`, and
  the deferred server proof).
- The user-text helpers in `claude.rs` (`human_prompt`, `interrupted`,
  the deferred `user_text`) read user text their own ways.
- `check`/`verdict` duplication and one place to derive status age (for
  the deferred rules).
- The pin is written by `pin_agent_pane` and `respawn_pane`: one helper.
