**Status:** in progress (phases 1 and 2 built, 0.19.0-dev.1; phase 4, the overlord on Codex, and `send_to_session` built on `codex-overlord`, 0.22.0-dev.3; the rest of phase 3 to come)

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
4. **A Codex overlord.** `[overlord] agent = "codex"`. Built: see "Phase 4"
   under Progress. **Requirement before the repo goes public (Brandon's
   decision):** the overlord must run on Codex, and preferably on OpenCode
   too. Both do now.
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

### Phase 4: the overlord on Codex (branch `codex-overlord`)

How it works is in `docs/architecture.md` ("The overlord") and the guide
("Which agent runs the overlord"). What was found on the way, against
codex-cli 0.167.2:

- **Wiring** (`codex::overlord_args`, all `-c` overrides, nothing written
  under `~/.codex`): `mcp_servers.rlord.command` / `args` / `env_vars`,
  `mcp_servers.rlord.default_tools_approval_mode="approve"` (values
  `auto | prompt | writes | approve`, from `codex app-server
  generate-json-schema`), `sandbox_mode="read-only"`,
  `approval_policy="on-request"`, `check_for_update_on_startup=false`, and
  the folder's trust.
- **Folder trust**: `-c 'projects."<dir>".trust_level="trusted"'` is
  accepted and ignored (Codex still shows "Trust this folder?"). The whole
  table works: `-c 'projects={"<dir>"={trust_level="trusted"}}'`.
- **Environment**: Codex starts an MCP server with a short list of
  variables. Since the #10 review the server is `rlord mcp --socket
  <path>` (the daemon's socket, never looked up), and `env_vars` passes on
  `agents::MCP_ENV` (`RLORD_SESSION_ID`, only logged).
- **AGENTS.md** in the folder is loaded (`codex debug prompt-input` shows
  it, with no model call; that command takes `-c` only, not `-s` / `-a`).
- **Subagents**: `.codex/agents/<name>.toml` (`name`, `description`,
  `developer_instructions`; `model_reasoning_effort = "low"`). They
  inherit the overlord's MCP servers and approvals and can't narrow them.
  A hyphen in the name is fine.
- **Codex reports nothing until a prompt**: no thread, no SessionStart,
  for a new conversation before its first prompt or a resumed one before
  its next. The overlord starts without a prompt and `ask` waits for it to
  leave "starting", so `discover` now settles both (a promptless launch
  becomes "your turn" once its pane has been up 4s; a hook-confirmed
  session still "starting" is read from its transcript). This also fixes
  ordinary promptless and resumed Codex sessions, which stayed "starting".
- An overlord that stopped with no conversation bound starts fresh.
- The setup dialog lists Codex when installed; its Remote Control row is
  only shown for an agent that has it.

Verified live (one throwaway overlord in a scratch rlord, its own XDG dirs
and tmux socket; the three Codex threads deleted afterwards):

- `rlord overlord ask` → `rlord.list_sessions` called with no approval
  prompt, answered from the scratch daemon (so the socket's environment
  reached the server); the call's `caller` was the overlord's session id.
- Hooks: SessionStart bound the thread; UserPromptSubmit / Stop gave
  working / your turn.
- `rlord overlord restart` → `codex resume <id>` with the same overrides;
  the conversation was back; status settled to "your turn".
- `/clear` → the next prompt's SessionStart carried `source: "clear"`:
  logged as `fresh_conversation` (`reason: clear`), new thread bound.
- "Start a tidy pass" → `spawn_agent` with `agent_type: "rlord-tidy"`
  (Codex's index: `agent_role = rlord-tidy`, `thread_source = subagent`);
  it called `triage` and `list_groups` with no prompt, and its thread did
  not rebind the overlord.
- "Run `touch probe.txt`" → Codex asked ("Would you like to run the
  following command?"), rlord showed "needs you", nothing was written.

Not exercised: the idle fresh restart by the keeper (it is `session_fresh`,
the same call the unbound start made live), "continue it" from the list
(`codex resume <earlier id>`, the same argv as restart), a Codex overlord
whose hooks aren't trusted (bound from the index, like any Codex session),
and `rlord-import`.

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

### PR #10 review (fixes, checked live on this Mac)

- **`approval_policy`**: the review asked for `"untrusted"` (a sandboxed
  command would run unasked under `"on-request"`; on Linux the read-only
  sandbox may allow a Unix-socket connect, so `tmux -L rlord send-keys`
  could type into a pane). Codex 0.167.2 refuses it at startup:
  `approval_policy = "untrusted" is no longer supported; remove this
  setting` (the pane died). The policies left are `on-request`, `never`
  and `granular`. Kept `on-request`. Live: the overlord ran `tmux -L
  <scratch socket> list-sessions` unasked and the sandbox refused it
  (`error connecting to … (Operation not permitted)`); rlord's tools ran
  with no prompt. Linux is unverified: owed below.
- **Killed mid-turn, then resumed**: the rollout ends on `task_started`.
  `follow` now counts only lines since the (re)start while a session is
  starting (`transcript::working_since`), and leaves the incremental
  reader at the end. Live: a session killed mid-story, restarted: starting,
  then your turn (`agent_stopped` from the transcript), not working.
- **Settling a start checks the screen**: `codex::composer_shown` (a `›`
  line near the bottom that isn't a numbered dialog choice). Live: the
  overlord and a promptless session settled once the composer showed.
  `check_for_update_on_startup=false` is accepted (no warning).
- **Unbound overlord**: `discover::bind_now` once with the old
  `bind_since`. Live: an overlord's `agent_session` cleared by hand →
  `ensure` bound the thread from the index (`agent_session_bound`) and
  resumed it; with no thread, it started fresh (`reason: unbound`).
- **`send_to_session`** (rlord's own tool, not `codex queue`): typed into
  the target's agent pane. Live: the Codex overlord sent "reply with only
  the word PONG" to a promptless Codex session; its hooks then showed
  SessionStart, UserPromptSubmit, Stop.

### PR #10 review 2 (fixes, checked live on this Mac, Codex 0.167.2)

- **Never press Enter into a dialog.** `send::type_into` captures the
  pinned pane right before every paste (ask, direct send, delivery tick)
  and needs `Agent::at_prompt`. `codex::composer_shown` now looks only at
  the bottom input area (the lowest `›` line of the last six, and no
  "Press enter to confirm"). Captured live and kept as fixtures: an
  approval (`› 1. Yes, proceed (y)` under the user's `›` prompt), an MCP
  tool approval (indented `  › 1. Allow`, "enter to submit"), the login
  choice (`> 1.`), working (status line, composer below). Live: with the
  overlord's status forced to working over an approval dialog, `rlord
  overlord ask` was refused ("its screen isn't at its input prompt") and
  the dialog stayed; nothing was written.
- **Never stuck in starting.** After 20s a start settles on any screen
  without a numbered dialog (`codex::dialog_shown`), logged;
  `awaiting_first_prompt` records `composer_seen`. Live: the overlord
  settled with `composer_seen: true`.
- **One paste per session; a direct send replaces the queued one.** Live:
  a message queued (status forced to working), then a direct send →
  `replaced_pending: true`, `message_dropped` (replaced), the newer one
  answered, the older never arrived.
- **`confirm_sends`** (Brandon's decision: stays off, sends run unasked):
  `-c mcp_servers.rlord.tools.send_to_session.approval_mode="prompt"` is
  accepted by 0.167.2 (the variants are `auto`, `prompt`, `writes`,
  `approve`). Live: the overlord asked "Allow the rlord MCP server to run
  tool send_to_session?"; allowed, PONG arrived.
- Not checked live: the attached-client check (no terminal could be
  attached here; unit-tested) and the per-session lock against a restart
  (unit-tested). A normal Codex session here (the user's own approval
  settings) never showed an approval, so the send path's screen check was
  checked live on the overlord (`ask`, same `type_into`) and on a scratch
  tmux pane in a gated test.

### PR #10 review 3 (screen checks)

- `codex::composer_shown` / `dialog_shown` also treat "enter to submit"
  and "esc to cancel" as a dialog's footer (the composer's is "esc to
  interrupt").
- A draft in the composer holds a send (`codex::draft`). Captured live
  with attributes (codex-cli 0.167.2, scratch tmux socket, no turn run):
  the placeholder is drawn dim (`ESC[2m`), typed text plain; a numbered
  draft (`› 1. numbered`) already reads as not the composer.
- Claude (`claude::dialog_shown`, not run live): only the lowest `❯` line
  counts, and one under a `─` rule is the input box. The old check read
  any numbered prompt in the history (`❯ 1. do x`) as a dialog and blocked
  every send to that session. A draft in the input box
  (`remote::input_text`, read with attributes) holds a send.
- The overlay's terminal doesn't count as someone typing in the session
  behind it; a held queued message is logged once (`message_held`).

Found on the way, not fixed (outside this PR): a hook-confirmed Codex
session whose tmux session is killed stays "working" (no SessionEnd, and
`discover` doesn't watch hooked sessions' panes) until it is restarted or
the daemon restarts.

## Owed

- Linux: check that the overlord's read-only sandbox refuses a connection
  to rlord's tmux socket (`tmux -L rlord list-sessions` from the overlord).
  If it doesn't, find a Codex setting that does (a permissions profile's
  network rules, or an execpolicy rule), since `"untrusted"` is gone.
- `codex queue --thread <id> --message …` could deliver to a busy Codex
  session without typing; `send_to_session` types for every agent today.
- Moving an existing overlord to another agent (the agent is fixed when it
  is first set up).
- A Codex subagent can't be given a narrower tool list or a cheaper model
  by name (only a lower effort); revisit if Codex's agent files grow that.

- Screenshots (`scripts/demo/screenshots.sh`) not retaken with the Codex
  demo session and the form's Agent hint.

- `docs/architecture.md`: the adapter section now describes the real
  interface; the Stop-hook mailbox and pane-pattern status it describes
  elsewhere were never built: reconcile when the effort finishes.
