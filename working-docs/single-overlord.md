**Status:** in progress (planning; starts after OpenCode lands)

# One overlord

Brandon's call: the two-seat overlord (a Sonnet front desk handing off to an
Opus mastermind) was overcomplicated. Go back to one overlord session.

## Shape

- The current **mastermind becomes the overlord**: Opus, **low effort**
  (an experiment the user asked for: a smart model at low effort feels like
  the right balance for the overlord; if it proves too dumb, go back to
  Opus at medium). Same idea for Codex: its strong model with
  `model_reasoning_effort = "low"`. OpenCode has no clean equivalent: use
  its default model and don't map effort (it may spend more tokens; fine).
  Fresh
  restart after idle (`mastermind_fresh_after_mins` → the overlord's),
  Remote Control always available (shown as "rlord overlord").
- Removed: the Sonnet front desk seat, `hand_off`, Option-M / seat switching
  in the overlay, the mastermind row, `mastermind.md` (merged into
  `overlord.md`).
- Migration: the mastermind's conversation becomes the overlord's (role and
  tmux name renamed); the old front-desk session is stopped and archived.
  Old config keys (`mastermind_model`, `mastermind_effort`) map onto the
  overlord's; an explicit `[overlord] model` / `effort` still wins. A user's
  explicit `mastermind_effort` carries over; only the default changes to low.
- `n` (new session) goes straight to the overlord.
- **Setup dialog** (first start, `tui/src/overlord.rs` `Setup`): one
  overlord now, so its rows become **agent**, **model**, **effort**, and
  Remote Control. Agent is pickable (installed agents only); changing it
  resets model and effort to that agent's defaults (Claude: Opus / low;
  Codex: strong model / low; OpenCode: default model, no effort row).
  Effort is pickable in the dialog (today it's shown but only settable in
  config.toml). The choice is saved to config so later starts reuse it;
  the user can change it in config any time.

## How the overlord routes a request (the heart of overlord.md)

The overlord is a quick place to route requests: send the user to panes,
launch work, keep the list tidy. Default handling:

| Request | Default |
|---|---|
| Find / jump / show / tag / rename / describe / keys / search | do it itself (rlord tools) |
| Routine mechanical work over many sessions (tidy, import, bulk edits) | spawn a cheaper-model subagent (rlord-tidy, rlord-import, …) |
| Non-trivial work in a repo | launch a session for it (propose/confirm per the form rules) |
| A question with no work to do | answer it itself, while it stays short (a few look-ups) |
| Deep research / many turns of digging | spawn a subagent; report the result |
| Work with no clear repo/session to land in | do it itself if small; spawn a subagent if larger |
| Work for an existing session | pass it to that session |

Rules of thumb: it doesn't do repo work in its own conversation; it keeps
its context small (it's long-lived); when unsure between answering and
launching, launching is right for work, answering is right for questions.

**The headline rule (state it first and plainly in overlord.md):** the
overlord runs at low effort, so it does only light work itself: look-ups,
small edits to rlord metadata, short answers. Anything that needs more than
mild effort (multi-step reasoning, reading a lot, research, bulk passes, any
repo work) goes to a subagent or a new session. When in doubt, delegate.

## The overlord remembers by looking things up

It restarts on a fresh conversation after idle, so its own past must be
cheap to reach without carrying it in context:

- `recall` (MCP): searches only the overlord's own earlier conversations
  (its transcripts across fresh restarts; normal search skips seat folders)
  and returns short dated snippets; with no query, a bounded digest of the
  previous conversation (the user's asks and its short replies, clipped).
  Plain code, no AI call.
- `search_history` can take one session, for "what happened in X?".
- overlord.md: when the user refers to something not in its context, call
  `recall` first, before asking or searching broadly.
- Nothing is preloaded into a fresh conversation; it fetches on demand.

## Fresh restarts the user can see (user ask, 2026-11-12)

Today: `start_fresh_keeper` / `fresh_if_idle` (`daemon/src/overlord.rs`)
checks once a minute. It restarts the seat on a fresh conversation when the
seat has been idle `mastermind_fresh_after_mins` (default 15; 0 = never)
since the last exchange, the turn really ended, it isn't waiting on the
user, and nobody is looking at it (overlay closed, no attached client). It
records `mastermind_fresh_at` in kv and a `fresh_conversation` event
`{from, to}` (the old and new agent session ids). So rlord already has the
full history of past conversations; the UI just doesn't show it.

Make it visible:

- **Overlord bar / row:** "fresh 2h ago · 14 earlier" (when the current
  conversation started, how many came before). Hover or the details pane
  says why: "idle 15m, restarted on a fresh conversation".
- **Details pane:** a list of past conversations: started/ended, turns,
  first prompt (clipped), why it ended (idle restart / user `/clear` /
  manual restart). Cheap: events + transcript stats, no model call.
- **Jump to one:** pick a past conversation, show its transcript excerpt in
  the preview (same reader as search). Stretch: "continue it" resumes that
  conversation in the overlord (`claude --resume <id>`, Codex `resume <id>`)
  and ends the current one the same way an idle restart would.
- **Count the user's own clears too:** Claude's SessionStart hook reports
  `source: "clear"`; record a `fresh_conversation` event with
  `reason: "clear"` so the history is complete. (Idle restarts get
  `reason: "idle"`.)
- `recall` (above) reads the same list, so the overlord and the UI agree on
  what "earlier conversations" are.
- Rename kv `mastermind_fresh_at` → `overlord_fresh_at` in the migration.

## Also in this PR (small UI fix, user ask 2026-11-12)

- The list's tags column cuts tags mid-word ("#release #tooli"). When a
  row's tags don't fit `TAGS_W`, show a group count instead ("3 groups";
  one tag that fits shows as is). Groups stay fully visible in the Groups
  tab and the details pane: check that the details pane wraps rather than
  clips them.
- Archive asks first: `x` (and the menu/launcher archive action) opens a
  small confirm ("Archive <name>? y / n", Enter = yes, Esc = no). It's
  too easy to hit by accident; bulk archiving goes through the overlord,
  which isn't affected.
