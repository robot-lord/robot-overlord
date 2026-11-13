# Parallel overlords with a hot swap; resume after a usage limit

**Status:** in progress (built, PR open; reconcile into docs when it merges)

Two features, one PR (0.23.0-dev.1).

## 1. One overlord per agent, one active

Decisions (the user's): an overlord may exist per agent; each starts the
first time it is switched to and keeps running; exactly one is active and
gets everything (overlay, `n`, `rlord overlord ask`, the Remote Control
"you"); no automatic switching.

Design calls:

- **DB**: every overlord keeps role `overlord` (so every refusal and
  `is_overlord_role` check holds for all of them); the agent column tells
  them apart (one per agent); kv `overlord_active` holds the active one's
  id. No kv: the oldest row, so an existing install needs no migration.
- **Folders**: each overlord has its own (`overlord`, then
  `overlord-<agent>`). `write_files` output is the same whichever agent
  runs it, so sharing would work for the files, but every agent finds its
  conversations by folder, and a migrated install's overlord may live in
  `mastermind/` anyway.
- **tmux**: the first keeps `overlord-rlord`; later ones are
  `overlord-rlord-<agent>`. No renames on a swap.
- **recall**: across all overlords (the user thinks of one desk, and
  switches because one is limited). The list's "earlier conversations"
  stay the active overlord's own.
- **Swap key**: `[tmux] overlord_switch`, default `M-s`, read by the
  overlay only.

## 2. Resume after a usage limit

- Detection: Claude Code's transcript record (`apiError:
  "usage_limit_reached"`, `resetsAt` epoch seconds). Seen on CLI 2.1.322,
  one sample (a 5-hour limit).
- `limits.rs`: kv `limits` (what is limited now), kv `limit_resume` (jobs).
- Checked in the demo (stand-in agents, scratch tmux): a limit record
  appended to a transcript shows as limited, `rlord resume-after-limit`
  schedules, and "continue" is typed in a minute after the reset.
  Switching in the overlay (key), CLI and the list was checked the same
  way; a click on the switcher row is covered by a unit test only.

## Review round 1 (PR #19)

- Claude ends a turn that failed on an API error through `StopFailure`,
  not `Stop` (checked in the CLI 2.1.323 bundle). Hook added, mapped to
  idle; `rlord install-hooks` adds it to existing installs (they show as
  outdated until then). `limits::step` also sends on `working` when the
  untouched limit record is the transcript's last word, so an install
  without the hook still resumes. Never on `waiting_input`.
- `usage_limit` needs `apiError: "usage_limit_reached"` or `status:
  "rejected"`; a plain 429 is not a limit.
- `scan` runs one at a time and keeps a file's stamp only after saving.
  Scheduling takes each session's lock; a job only writes back over
  itself (`limit_at`, `asked_at`).
- `roster` runs after the DB guard drops. `overlord_switch` sharing a key
  with a tmux binding turns the switch key off, with a warning.
- `overlord.continue` with another overlord's conversation switches first.

## Owed

- `recall` treats the active overlord as the caller. `rlord mcp` knows the
  calling session's id (`agents::MCP_ENV`, only logged today); passing it
  through would make a background overlord's `recall` exact.

- Codex limit detection: no rollout on this machine ever hit a limit, so
  its record's shape is unverified. Likely signal: `token_count`
  `rate_limits.rate_limit_reached_type` non-null, `resets_at` epoch
  seconds. Needs one real sample.
- OpenCode limit detection.
- The swap key isn't on the key screen (`keys_list` / `:keys`): it is an
  overlay key, not a tmux binding, and the screen's live test goes
  through tmux.
- No way to cancel a scheduled resume other than using the session.
- No test drives the "job rescheduled while it runs" race end to end;
  the guard (`limit_at`/`asked_at` compare) is small and reviewed only.
- `status:limited` as a list filter.
