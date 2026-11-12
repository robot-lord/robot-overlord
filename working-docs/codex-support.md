**Status:** in progress (planning)

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

## Open decisions

- Hook trust: user trusts once (rlord guides it, with a status fallback
  when untrusted) vs. `--dangerously-bypass-hook-trust` on every launch
  (also runs untrusted repo hooks: no).
- Codex's shared background app-server (`--no-daemon` to isolate each pane?).
- Summaries: summarize Codex sessions with Codex, or one summary agent for all.
- Default agent for new sessions; whether the overlord picks per request.

## Owed

- `docs/architecture.md` describes an adapter trait, a Stop-hook mailbox
  and pane-pattern status that were never built: reconcile when this lands.
