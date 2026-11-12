**Status:** in progress (phases 1 and 2 built on `codex-support`, 0.19.0-dev.1; phases 3 and 4 to come)

# Codex CLI support

Goal: Codex sessions are first-class in rlord, beside Claude Code: launch,
resume, status, list, preview, search, summaries, import, messaging; then
the overlord on Codex. Researched against codex-cli 0.167.2 (local binary,
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
| Overlord | `--mcp-config`, `--allowedTools`, `--effort`, `.claude/agents` | `-c mcp_servers.rlord.*` + `default_tools_approval_mode="approve"`, `-c model_reasoning_effort`, AGENTS.md (already written), `[agents]` | doable; later phase |
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
4. **A Codex overlord.** `[overlord] agent = "codex"`. (The overlord is one
   session now. Its Codex defaults are in place: Codex's default model
   with `-c model_reasoning_effort="low"`, `overlord::effort_args`. What's
   missing is the MCP and tool-approval wiring, then `Agent::runs_overlord`.)
   **Requirement before the repo goes public (Brandon's decision):** the
   overlord must run on Codex, and preferably on OpenCode too. That is the
   next work after the single-overlord PR (#7).
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
  sub-agents and a Codex overlord follow.
- The overlord running on Codex (and preferably OpenCode) is required
  before the repo goes public; it is the next work after PR #7 (one
  overlord). See phase 4.

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

Known gap in the status work: a permission denied in Codex clears only
at the next tool or the turn's end.

After the PR's code review:

- **Whose conversation a hook speaks for** (`methods::conversation`): a
  sub-agent's thread never rebinds a session (Codex's index says which
  threads are the user's); an index-bound id (`minted`) gives way to
  whatever the hooks name; with nothing bound, only a SessionStart binds
  (a late event from a replaced process is ignored). Another thread's
  permission prompt still shows "needs you".
- **Who "needs you" is for** is a set (kv `needs_you_by:<id>`, a JSON
  list, kept only while the session waits: a Claude sub-agent's
  `agent_id`, a Codex sub-agent's thread, "" for the main agent). A party
  leaves it by running a tool or ending (SubagentStop, its thread's
  Stop); the session needs you until it is empty; the main agent's own
  prompt or end of turn clears it. A cut-off payload names nobody.
- A bound Codex session moves to another thread only when Codex's index
  says the user drives it (any event); one the index doesn't know yet
  waits.
- **Index binding is cautious** (`discover::pick_thread`): CLI threads
  only, the first message must be the launch prompt when there was one,
  and nothing is bound when two sessions wait in one folder or two
  threads fit.
- **Unhooked status** uses the incremental transcript stats; "unknown"
  never ends a running turn; a pane must look gone twice to mean stopped.
- **Hook client**: payload cut to the fields rlord uses before sending
  and spooling; bounded waits; spooled only when the daemon is
  unreachable.
- **Installed** means found on the daemon's PATH or by the user's login
  shell; when only the shell has it, the pane runs that path (launch,
  resume, fresh). The shell is asked outside the DB lock (before a launch,
  and at daemon start); under the lock only its last answer is read.
  Hooks count as installed only when every event is.
- **Hook client**: an event is spooled unless the daemon answered or is
  merely slow to answer (a closed or reset connection is a restart: keep
  it).
- Clicks that stop, restart or archive ask first; menus and double-clicks
  go by session id.

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

Known limits, accepted:

- A hook event the daemon handled but died before answering is delivered
  again from the spool at the next start.
- When the login shell leaves something holding its output open, the
  thread reading it lives until that ends (the answer is given up on at
  the time limit).
- `RLORD_NO_LOGIN_SHELL=1` turns login-shell resolution off (the demo and
  scratch setups set it, with a stand-in `SHELL`).

## Owed

- Screenshots (`scripts/demo/screenshots.sh`) not retaken with the Codex
  demo session and the form's Agent hint.

- `docs/architecture.md`: the adapter section now describes the real
  interface; the Stop-hook mailbox and pane-pattern status it describes
  elsewhere were never built: reconcile when the effort finishes.
