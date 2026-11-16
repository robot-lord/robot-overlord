**Status:** in progress

# Session forking

A new session that continues from a copy of another session's whole
conversation, on the same agent, in the same folder (or a new worktree).
The original is left as it is. One PR: 0.23.0-dev.29.

## Hard constraints

- Never touch the user's default tmux server, and never touch `tmux -L
  rlord`. Scratch sockets only, live checks included.
- No model calls in tests. Live checks F1–F4 are manual, one short prompt
  each, on a scratch rlord with `RLORD_NO_LOGIN_SHELL=1` and a stand-in
  `SHELL`. Kill each scratch server afterwards.
- Tests never read the real `~/.claude`, `~/.codex` or OpenCode DB, and
  never call the real daemon socket.
- Read only `session_v2` and `session_message` from OpenCode's DB.
- No `--dangerously-*` flags.
- Never run agent install commands.
- Never run `codex app-server daemon update|restart|stop` or `codex
  remote-control stop`.
- Never print credentials.
- Never write the user's agent config.
- Logic goes in Rust; shell is glue only.
- No time estimates.

## User decisions (2026-11-16)

- Folder: same folder by default, with a warning when the parent is live;
  new-worktree forks are in this PR too. Codex yes; Claude only if live
  check F2 passes; OpenCode refused with a clear message.
- Form: `f` forks directly from a small form, by plain code. No
  `confirm_fork`.
- Notes (the user's own per-session text): not copied.
- Live checks F1–F4: allowed on scratch setups, one short prompt each.

## Design (as built)

See the plan for the reasoning; this is what ships.

- RPC `session.fork`; CLI `rlord fork`; list key `f` (a small form); MCP
  tool `fork_session`.
- Native fork: Claude `claude --resume <parent> --fork-session`, Codex
  `codex fork <parent>` (always in the pane, `--no-daemon`), OpenCode
  through its server (`POST /api/session/<id>/fork`), then `opencode -s
  <fork id>`.
- Lineage: `sessions.forked_from` (migration 0011), shown in the list,
  details, `rlord show`, `rlord ls --json` and MCP answers.
- Copies: agent, model, group, tags, description, and the parent's
  `session_context` rows (with `at`, `source`, `seen_at`). Not notes,
  pinned, summary, usage, Remote Control, `codex_host`, role.
- Adds: a link to the parent and a note "forked from …" (`source =
  'fork'`). A fork always has background (`context::turn_on`).
- Codex binding: kv `bind_fork:<id>` = the parent's thread; `pick_thread`
  takes only a thread whose rollout says `forked_from_id == parent`, and
  ordinary sessions skip forked threads.
- A Claude/Codex fork not materialized yet is re-forked on restart (kv
  `fork_pending:<id>`).

## Live checks

(Results filled in as they run.)

- F1 Claude `--resume <id> --fork-session --session-id <new>`:
- F2 Claude fork from another worktree of the same repo:
- F3 Codex `codex fork <id> --no-daemon`:
- F4 OpenCode fork call and `opencode -s <fork id>`:

## Progress

- [x] Working doc, version 0.23.0-dev.29.
- [ ] Extract `methods::start` from `session_new`.
- [ ] Store: migration 0011, `forked_from`, `forks_of`, `context_copy`,
  `names::fork_name`.
- [ ] Agents: fork argv, rollout `forked_from_id`, OpenCode fork call.
- [ ] Binding: `bind_fork`, `pick_thread` fork mode.
- [ ] `daemon::fork`, `session.fork`, restart of a pending fork.
- [ ] CLI `rlord fork`, lineage in `show` / `ls --json`.
- [ ] TUI: key, form, list marker, details.
- [ ] MCP `fork_session`, `overlord.md`.
- [ ] Worktree forks.
- [ ] Docs, reconcile.

## Owed / deferred

- The seeded fallback for agents with no native fork (Gemini): designed
  in the plan (section 8), built after session-context PR 2.
- Daemon-mode Codex forks through the app-server's `thread/fork`.
- Fork at an earlier message (OpenCode's `before`, Codex's
  `lastTurnId`).
- Cross-agent fork.
