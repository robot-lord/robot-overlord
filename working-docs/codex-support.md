**Status:** in progress (phases 1 and 2 built on `codex-support`, 0.19.0-dev.1; phases 3 and 4 to come)

# Codex CLI support

Goal: Codex sessions are first-class in rlord, beside Claude Code: launch,
resume, status, list, preview, search, summaries, import, messaging; later a
Codex overlord seat. Researched against codex-cli 0.167.2 (local binary,
~/.codex files, its app-server schema) and its docs.

## What rlord uses from Claude, and Codex's counterpart

| Area | rlord today (Claude) | Codex 0.167 | Gap |
|---|---|---|---|
| Launch | `claude --session-id <minted> [--model] [prompt]` | `codex [-m] [prompt]`; **no caller-chosen id** (UUIDv7, assigned at start) | bind the id after launch |
| Resume | `--resume <id>` (transcript must exist) | `codex resume <id> [prompt]`, `codex fork <id>` | none |
| Identity | minted id → known transcript path; hooks confirm | `SessionStart` hook gives `session_id` + `transcript_path`; also `~/.codex/state_5.sqlite` `threads` (id, cwd, rollout_path, created_at_ms) | hook-bound (or SQLite scan by cwd+time) instead of minted |
| Status | hooks: SessionStart, UserPromptSubmit, Stop, Notification, SessionEnd | same hook system and format, events incl. SessionStart, UserPromptSubmit, Stop, **PermissionRequest**, SessionEnd, Subagent*; no Notification (also no idle "waiting" flip) | map PermissionRequest → waiting |
| Hook install | `~/.claude/settings.json` | `~/.codex/hooks.json` (or `[hooks]` in config.toml) | new installer; **hooks need user trust** (`/hooks` in the TUI, or `--dangerously-bypass-hook-trust`) |
| Transcripts | `~/.claude/projects/<slug>/<id>.jsonl` | `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl` (`session_meta`, `turn_context`, `response_item`, `event_msg`, `token_count`…), plus SQLite index | a second parser; every consumer (excerpts, stats, search, read, summaries, import) goes through a common shape |
| Summaries | `claude -p --resume --fork-session --no-session-persistence --tools ""` | `codex exec fork <id> --ephemeral -s read-only -o <file>` (or excerpts to `codex exec --ephemeral`) | per-agent runner |
| Import | scan Claude project dirs | `threads` table has cwd, title, first message, git branch, model | easier than Claude |
| Sub-agents | hooks + `<id>/subagents/` files | `threads` rows with agent_nickname/role, `thread_spawn_edges`; Subagent* hooks | later |
| Messaging a session | overlord's Claude peer tools (`SendMessage`) | **`codex queue --thread <id> --message …`**: delivered without typing | better than Claude's; new rlord tool |
| Overlord seat | `--mcp-config`, `--allowedTools`, `--effort`, `.claude/agents` | `-c mcp_servers.rlord.*` + `default_tools_approval_mode="approve"`, `-c model_reasoning_effort`, AGENTS.md (already written), `[agents]` | doable; later phase |
| Models | opus/sonnet/haiku catalog | `codex debug models` / `models_cache.json` | per-agent catalog |
| Remote Control | Claude app bridge | `codex remote-control` (experimental) | later; report unavailable |
| Key shadows | Claude defaults + keybindings.json | `tui.keymap.*` (only `alt-r` default) | later; verify Alt-O/Alt-D by hand |

## Plan (phases)

1. **Adapter seam, no behavior change.** An `Agent` trait (launch/resume
   argv, hook install + event mapping, transcript reader into a common
   shape, summary runner, model catalog, overlord flags). Claude moves
   behind it; transcript consumers use the common shape.
2. **Codex sessions.** `rlord new --agent codex`, the new-session form and
   the overlord choose the agent; id bound by the SessionStart hook
   (fallback: SQLite by cwd + launch time); status from hooks; `rlord codex
   install-hooks` + trust flow; resume/restart/stop; list, preview, search,
   read, excerpts, stats; archive summaries.
3. **Codex extras.** Import from `threads`; models catalog; `send_to_session`
   via `codex queue` (and the Claude path); sub-agents.
4. **Codex overlord seats.** `[overlord] agent = "codex"` per seat.
5. Later: Codex Remote Control, key shadows, Gemini.

## Decisions (with Brandon)

- Hook trust: rlord installs its Codex hooks and guides the user to trust
  them once (`/hooks` in Codex); until trusted, status falls back to Codex's
  SQLite (`threads` / `thread_turns`). Never `--dangerously-bypass-hook-trust`
  (it would also run untrusted repo hooks).
- Summaries: by the session's own agent (Codex: `codex exec fork` ephemeral,
  read-only; excerpts to `codex exec --ephemeral` when too big).
- New sessions: `[session] agent` is the default; the form gets an agent hint
  and the overlord picks Codex when asked.
- First round: phases 1 and 2 in one PR (0.19.0-dev.1). Import, messaging,
  sub-agents and Codex overlord seats follow.

## Progress

Phases 1 and 2 are built (branch `codex-support`).

- **Seam**: `core/src/agents/mod.rs` (`Agent` enum), `claude.rs`,
  `codex.rs`, `transcript.rs` (one shape for both agents' JSONL). Every
  `"claude"` gate in the daemon and TUI asks the agent.
- **Identity**: SessionStart hook binds the Codex thread (`trust = hook`).
  Without trusted hooks `daemon/src/discover.rs` binds from `state_5.sqlite`
  `threads` (cwd + started since launch, not held by another session;
  `trust = minted` until a hook confirms). Only a `SessionStart` moves a
  Codex session to another thread: other events with another id are a
  sub-agent thread in the same pane and are ignored.
- **Status**: hooks (SessionStart, UserPromptSubmit, Stop,
  PermissionRequest, SessionEnd). Without them, `discover` reads
  task_started / task_complete from the rollout tail (only when the file
  changed) and "stopped" from the dead pane, every 3s, for unhooked Codex
  sessions only. It reads the rollout, not `thread_turns`: same facts, one
  file already open for other reasons.
- **Summaries**: `codex exec fork <id> --ephemeral --skip-git-repo-check -c
  sandbox_mode="read-only" -c approval_policy="never" -o <file> <ask>`
  (`exec fork` has no `-s`; `-c` is accepted), excerpts on stdin to `codex
  exec … -` when too big or the fork fails. Not run against the real Codex
  (no model calls in testing): the flags are checked against `--help` only.
- **Models**: Codex's from `models_cache.json` (visibility `list`, by
  priority), read on demand; free text when there is none. `[session]
  codex_model`, `[archive] codex_summary_model`.
- **Choosing**: `rlord new --agent`, the form's Agent hint (changing it
  resets the model to that agent's default), `agent` on
  propose/confirm/create tools.
- **Demo**: `scripts/demo/bin/codex` stand-in (hooks, rollout, thread
  index; `nohooks/<folder>` turns its hooks off) and one Codex session.

Since then, on the same branch:

- Codex launches with `--no-daemon` (hooks must run in the pane) and
  `--no-alt-screen` (tmux scrollback). Verified against real Codex by the
  lead: hooks bind, status, search, `exec fork` summary.
- **Install hints** (`core/src/agents/install.rs`): one table of each
  vendor's install commands (Claude Code, Codex, OpenCode), the one for
  this machine suggested by `rlord agents`, `list_agents` (MCP), the form
  (a missing agent shows the command, `^Y` copies, sending refused) and
  `session.new`'s error. rlord never runs an install. Commands checked
  against the vendors' pages on 2026-11-11.
- **Statuses mean one thing each**: `waiting_input` is shown as "needs
  you" (PermissionRequest, Claude's AskUserQuestion via PreToolUse, a
  permission/elicitation Notification by `notification_type`), cleared to
  working by PostToolUse / PostToolUseFailure; `idle` is "your turn"
  (Claude's idle Notification is ignored). Stored values unchanged. Tool
  hooks that change nothing return before any tmux call, write or log
  line, and don't tick the list. Existing installs need `rlord
  install-hooks` again (Codex: re-trust).

Known gaps in the status work: a permission denied in Codex clears only
at the next tool or the turn's end; a sub-agent's tool finishing while the
main agent waits on a prompt clears "needs you" early (same session id).

Unverified against a real Codex run: that hooks fire with these payload
fields for an interactive session, that SessionStart is not fired for
sub-agent threads, `thread_source = 'user'` for user threads only, and the
summary command end to end.

Next: OpenCode (server/SSE events, plugins), then the integrations are done.
For it, keep in mind what is still file-shaped: a transcript is located by
a `Path` (`transcripts.source_path`, `Format::of`), and `history::search`
greps files. The consumer seam is `agents::transcript`'s functions and
`history::search`/`read` (common shapes out), so an OpenCode `Format` would
answer those from its storage; status would be a third source next to hooks
and `discover` (an SSE listener writing the same status events).

## Owed

- Screenshots (`scripts/demo/screenshots.sh`) not retaken with the Codex
  demo session and the form's Agent hint.

- `docs/architecture.md`: the adapter section now describes the real
  interface; the Stop-hook mailbox and pane-pattern status it describes
  elsewhere were never built: reconcile when the effort finishes.
