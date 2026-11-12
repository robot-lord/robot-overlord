**Status:** in progress (sessions built on `opencode-support`, 0.20.0-dev.1; import, messaging and overlord seats to come)

# OpenCode support

Goal: OpenCode sessions beside Claude Code and Codex, through the same
`Agent` interface (crates/core/src/agents). Researched against OpenCode
v2.2.7 (Homebrew `anomalyco/tap/opencode-v2`; v1 differs: require v2).

## How OpenCode differs

- **One shared background server per user** (`opencode serve --service`,
  started on demand). Sessions, tools, permissions and MCP run there; the
  TUI in a pane is a client. Its URL and password are in
  `~/.local/state/opencode/service.json` (HTTP Basic, user `opencode`).
- **rlord can mint the id**: `opencode -s <ses_…>` continues or creates that
  session (ids must start `ses_`). Resume is the same flag.
- **Status from the server's event stream** (`GET /api/event`, SSE), not
  per-pane hooks: `session.status` busy/idle, `permission.asked/replied`,
  `form.created/replied` (questions), `session.execution.*`. Resync after a
  reconnect from `GET /api/session/active`. One connection covers every
  session; no install or trust step.
- **Storage is SQLite** (`~/.local/share/opencode/opencode.db`: `session_v2`,
  `session_message` with JSON `data`); also `opencode session export` and the
  HTTP API. The DB also holds credentials: read the session tables only.
- **Panes can switch sessions** (tabs, new session, child sessions); nothing
  reports which one a pane shows. rlord follows its minted id; others are
  "another conversation", like Codex sub-threads.
- **Summaries**: `POST /api/session/{id}/generate` (session context, no fork;
  unverified that it appends nothing), else `run -s <id> --fork --archived`
  then delete the fork. No ephemeral flag.
- **Overlord seat**: config in the seat folder's `opencode.jsonc`
  (`mcp.servers.rlord`, an agent with deny-all + allow `rlord_*`), not flags.
- **Messaging**: `POST /api/session/{id}/prompt {text, delivery: queue}`.
- **TUI**: full-screen (no scrollback mode); `opencode mini` is inline with
  scrollback. Alt-O/Alt-M unbound; Alt-D deletes a word forward. It takes
  `-s`, `--prompt`, `--auto`, `--continue`; no model flag.

## Plan

(Steps 1 to 4 are built: see Progress.)

1. OpenCode agent behind the interface: launch/resume with a minted
   `ses_` id, install detection (v2 gate), models (`opencode models`).
2. Status: a daemon task subscribed to the service's SSE, mapping events to
   working / needs you / your turn, with resync; stopped from pane death
   (and the service idle).
3. Transcripts: a SQLite-backed reader into the common shape (excerpts,
   stats, search, read); search "everywhere" over `session_v2`.
4. Summaries via `/generate`, fallback fork + delete.
5. Later: import, messaging via the API, OpenCode overlord seats.

## Decisions (with Brandon)

- The full OpenCode TUI, not `opencode mini` (so: no tmux scrollback).
- Status from the service's event stream, not a server plugin.
- Summaries: `/generate` first (verified live, below), fork + delete as
  the fallback.
- OpenCode v2 is required (gated in install detection).

## Progress

Sessions work end to end (the scope of Codex's phase 2).

- **Agent** (`core/src/agents/opencode.rs`, `Agent::OpenCode` in `mod.rs`):
  the id is minted in OpenCode's shape (12 hex digits of inverted time, 14
  base-62). No hooks (`Agent::uses_hooks`). Install detection runs it with
  a v2 floor read from `--version` (`install::warm` asks, `missing` and
  the form refuse an older one with the upgrade command). Models, kept ten
  minutes (a failed attempt too): from the server (`GET /api/model`)
  whenever one is running; `opencode models` only when the user asks to
  see them (the form opening, `rlord agents` / `list_agents`:
  `install::list_models`), never at daemon start, before a launch or for
  the overlord's `get_session_defaults`, because the command may start
  OpenCode's server (Brandon's call).
- **Launch creates the session first.** The TUI has no model flag, creates
  a session only with its first message, and doesn't notice one created
  under its id afterwards (all checked live). So the pane command is
  `sh -c '"$0" api POST /api/session -d "$1" …; shift; exec "$0" "$@"'
  opencode <{id, location, model}> -s <id> [--prompt …]`: OpenCode's own
  client creates the session with its model, then the TUI opens on it and
  the prompt runs on that model. The session exists from the start, so a
  promptless launch shows "your turn" at once. Resume: a session OpenCode
  has is opened as it is (`opencode -s <id>`: it keeps its own model, and
  the `resumed` log entry says so); one it doesn't have is created again
  with the launch's model. `Launch::deferred` (kv `deferred:<session>`,
  with the time it was held; cleared by every restart) names the model for
  the status task to confirm: on the session's row, done silently;
  otherwise set through the server (`POST …/model`) with a note that a
  first prompt may have run before; neither within 20s, the session's log
  says the model wasn't applied. The first prompt is always on the command
  line: nothing can drop it.
- **Status** (`daemon/src/opencode.rs`, client and event mapping in
  `core/src/agents/opencode_service.rs`): one thread, one connection to
  `GET /api/event?types=…` (types repeated; a comma-separated list is
  rejected), read in 3s ticks. Only while there are live OpenCode
  sessions. Resync after every connect: turn state from `session_v2`
  (`time_busy` / `time_idle`), pending permissions and forms from the
  server, for the session and the sub-agent sessions under it. No server:
  the session rows alone, each tick, and only for what they can say: a
  starting session is up; a "busy" row is believed only for a session that
  was starting; once the server has failed to answer twice running it is
  gone, and a session still working becomes your turn (turns run in the
  server; "updated" doesn't move). Stopped: the pane, two looks. A
  permission or question from a sub-agent's child session counts for the
  session rlord launched (parents looked up in OpenCode's database; only a
  definite answer is remembered). The HTTP client is hand-written (plain HTTP/1.1 to localhost,
  chunked bodies, bounded waits): no new dependency.
- **Transcripts** (`agents/opencode.rs`): a conversation's path is
  `<opencode.db>/<session id>`; `transcript` (excerpts, stats, context
  size, model, turn state, `exists`) and `history` (search, read; a hit's
  line is the message's `seq`) answer from the database, read-only, only
  `session_v2` and `session_message`. `scope: all` lists every top-level
  session.
- **Summaries** (`daemon/src/summary.rs`, `opencode_summary`): `generate`,
  then a fork (`opencode run -s <id> --fork --archived`), then excerpts to
  a session of its own; the throwaway session's answer is read from the
  database and the session deleted (`opencode session delete`).
  `[archive] opencode_summary_model` applies to the excerpt run only:
  `generate` and the fork use the session's own model. Commands rlord runs
  for an answer (`opencode run`, `opencode models`) print to a file, not a
  pipe: one that starts OpenCode's server would leave it holding the pipe.
- **Config**: `[session] opencode_model`, `[archive]
  opencode_summary_model`.
- **Forms, CLI, overlord**: `--agent opencode`, the form's Agent hint,
  `agent` on the MCP tools; overlord.md / mastermind.md; README and guide.
- **Demo**: `scripts/demo/bin/opencode` (writes an OpenCode database; no
  server) and one OpenCode session. Its status comes from the session row;
  the demo can't show an OpenCode session that needs you.

## Verified against OpenCode 2.2.7 (throwaway sessions, all deleted)

- `opencode -s <minted id> --prompt …` creates the session under that id
  and sends the prompt. **Without `--prompt` it creates nothing** until
  the first message (no row, `GET /api/session/{id}` 404), and a TUI
  opened that way stays on its empty screen even when the session is then
  created over REST and prompted (the turn ran in the server, unseen).
- `opencode api POST /api/session -d '{id, model, location}'` creates the
  session (exit 0; a bad body exits 1); creating it again returns it
  unchanged. A TUI opened afterwards with `-s <id> --prompt …` shows the
  model and runs the prompt on it.
- Events on the stream are `data:` lines only (the type is in the JSON),
  with a heartbeat comment every few seconds. A turn showed
  `session.created`, `session.inbox.enqueued` (the user's prompt),
  `session.execution.started`, …, `session.execution.succeeded`.
  **`session.status` and `session.idle` were never sent**; they stay
  mapped in case another version sends them.
- Forms (created, answered and cancelled over REST): `form.created` is
  `{form: {id, sessionID, …}}`; `form.replied` is `{id, sessionID,
  answer}`; `form.cancelled` is `{id, sessionID}`; `GET
  /api/session/{id}/form` lists the pending ones as `{data: [{id, …}]}`.
  As mapped. (A nested `form` on the last two is read too.)
- `POST /api/session/{id}/generate` returned text and changed nothing but
  the session's token totals: no message row, no `time_updated` /
  `time_busy` / `time_idle` change, no execution event (only
  `session.usage.updated`). It takes no model: the session's answers.
- `POST /api/session/{id}/model` switched the model (the TUI showed it).
- `opencode session delete <id>` removes the session and its messages.

Not exercised against the real thing: a permission request (events and
the pending list are from the OpenAPI spec), `opencode api` when no
server is running (whether it starts one; if it doesn't, the launch falls
back to the TUI creating the session and the model being set afterwards),
`opencode run … --fork --archived` (that `--archived`
marks the fork `time_archived`, which is how rlord finds the fork to
delete: if it doesn't, the fork is left behind, never a wrong one
deleted), `GET /api/model` (names built from `providerID` and `modelID` /
`id`, per the spec), and an OpenCode older than v2 (what its `--version`
prints).

The whole flow was run in a scratch rlord against a stand-in server that
speaks the observed format: deferred model and prompt, every status
transition from events, an outage (fallback to the rows), reconnect and
resync with a pending permission, pane death, resume, both summary
paths, and (with the demo's stand-in) the create-then-open launch with
and without a model and a restart of a session OpenCode no longer had.

## Known limits

- rlord follows the conversation it minted. A new session or tab opened
  inside the TUI isn't followed (nothing says which one a pane shows).
- A turn runs in the server: closing the TUI stops the rlord session, and
  the turn may go on unseen.
- While no server is reachable, "needs you" can't be seen, and no session
  is shown working (see Status).
- A turn rlord learns of from the session row (resync, no server) is
  logged as `prompt_submitted` whoever started it (as `discover` does for
  Codex; agreed).
- `generate` adds its tokens to the session's totals (so a summary nudges
  the session's token counts).
- Summaries by fork or excerpts may run tools if the model insists (no
  tool-less mode was found); the run is bounded by the summary timeout.

## Review of PR #6 (declined, with why)

- The shared server inherits the first pane's environment
  (`RLORD_SESSION_ID`): OpenCode's design. rlord attributes OpenCode
  sessions by the id it minted, never by environment, and has no OpenCode
  hooks; the MCP bridge's `caller` only decides whether the mastermind may
  `hand_off`, and a seat can't run on OpenCode.

## Open

- A server that died mid-turn and came back: its session row may still
  say "busy", and a resync with the server would show working again
  unless OpenCode repairs the row itself (it has `resume_attempts` /
  `time_suspended`, so it may). `GET /api/session/active` could veto a
  stale row, but what it lists mid-turn wasn't checked live.

## Owed

- Import (`session_v2` has directory, title, model), messaging through
  the API (`POST …/prompt`, already used for the deferred prompt),
  OpenCode overlord seats (`opencode.jsonc` in the seat folder).
- Sub-agent tracking (child sessions: `parent_id`).
- A real permission prompt and question form, checked by hand.
- Screenshots not retaken with the OpenCode demo session.
- `docs/architecture.md` got the OpenCode facts in its agent section; the
  larger reconcile owed by the Codex doc still stands.

## Also queued: Codex Remote Control

Codex's Remote Control is machine-wide (`codex remote-control start/pair`)
and reaches threads run by Codex's background app-server, not rlord's
in-pane (`--no-daemon`) sessions. Supporting it means running remote Codex
sessions through the app-server and matching their hooks by conversation
id; it pairs naturally with Codex overlord seats.
