**Status:** in progress (planning; starts after OpenCode lands)

# One overlord

Brandon's call: the two-seat overlord (a Sonnet front desk handing off to an
Opus mastermind) was overcomplicated. Go back to one overlord session.

## Shape

- The current **mastermind becomes the overlord**: Opus, medium effort, fresh
  restart after idle (`mastermind_fresh_after_mins` → the overlord's),
  Remote Control always available (shown as "rlord overlord").
- Removed: the Sonnet front desk seat, `hand_off`, Option-M / seat switching
  in the overlay, the mastermind row, `mastermind.md` (merged into
  `overlord.md`).
- Migration: the mastermind's conversation becomes the overlord's (role and
  tmux name renamed); the old front-desk session is stopped and archived.
  Old config keys (`mastermind_model`, `mastermind_effort`) map onto the
  overlord's; an explicit `[overlord] model` still wins.
- `n` (new session) goes straight to the overlord.

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
