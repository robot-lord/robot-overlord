**Status:** in progress

# Importing Codex conversations

Extends import (`rlord_core::import`, `session.import_scan`,
`session.import`, `scan_agent_sessions` / `import_sessions`, the
`rlord-import` subagent, the first-run offer) from Claude Code to Codex.
OpenCode stays unsupported.

## What is read from Codex

- Rollouts: `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<time>-<uuid>.jsonl`,
  listed by `codex::rollouts_in` (directory walk + mtime, no reads). The
  id is the file name's trailing uuid.
- Per candidate: the first 16 KB of the `session_meta` line (`originator`,
  `source`, `cwd`, `git.branch`; the line can be megabytes of
  instructions, so fields are found by key as `codex::rollout_cwd` does),
  plus the usual head/tail windows (`transcript::read_head_tail`,
  `excerpts_from(Format::Codex)`), the same reader running sessions use.
- Thread index `state_5.sqlite`, one read-only `SELECT id, cwd, source,
  thread_source FROM threads` per scan (falls back to `id, cwd` on an
  older schema, and to nothing without an index).

## Filters (same as Claude's)

Window by last real exchange (mtime pre-filter), tracked ids, rlord's data
dir and temp dirs, the overlord folder, nothing typed. Codex-specific:
headless runs (`source` `exec`/`mcp`, or `originator` containing `exec`)
and sub-agent threads (`thread_source` not `user`, or a `subagent`
source object) are skipped.

## Folder, resume and binding

- Folder: the latest `turn_context.cwd` in the tail, else the index's
  `cwd`, else `session_meta.cwd`. Codex resumes into the thread's last
  folder.
- The imported session's launch spec is `{"agent": "codex"}` with no
  `codex_host`, so it resumes in the pane: `codex resume <id>
  --no-alt-screen --no-daemon` (`Agent::resume`).
- The thread id is set as the session's `agent_session` with trust
  `hook`. When the resumed Codex reports its thread on its hooks, the id
  matches (`Whose::Ours`), so it stays bound.

## Not tested live

- Resuming an imported thread with a real Codex, including a thread that
  was created on Codex's background server and is now resumed in the pane.
- Real `session_meta.source` values for `codex exec` and the SDK
  (assumed `exec` / originator `codex_exec`); the index's `source` column
  is checked first where it exists.

## Owed

- Import on Codex's background server when `[session] codex_daemon` is on
  (needs the server started first, as `session.new` does).
- PR links: Codex rollouts have no `prUrl` equivalent; `pr_url` is empty.

## Review round 1 (PR #78)

Fixed: full ids in proposal lines and the CLI listing (UUIDv7 prefixes
collide), agent column in `rlord import`; stale overlord.md line;
architecture.md Import section covers Codex; `source` vscode threads are
flagged "maybe open elsewhere" (possibly hosted by Codex's background
server); session_meta is parsed whole (head, else first line up to 4 MB)
so a big AGENTS.md can't hide `source`.

Declined: bounding the walk by day folder. A rollout stays in the folder
of the day it started and is appended to later, so a thread started long
ago but active yesterday would be missed. The walk is readdir + stat only;
the offer check already sorts by mtime and stops at the first hit or the
first file older than the window.

Owed:
- Bound the walk using the index (`updated_at` per thread) once its schema
  is confirmed, keeping the full walk as fallback.
- `explain` merge: typed per-agent results instead of string matching on
  "no transcript".
- Dedupe `codex_index`/`open_state` and `session_meta`/`rollout_first_line`
  with `agents::codex`.
- Dedupe the Claude/Codex candidate scaffolding; drop now-unused
  `scan_claude`/`scan_codex` wrappers.
- `explain_codex` / import by id: a per-id `WHERE id = ?` lookup instead of
  loading the whole index.
