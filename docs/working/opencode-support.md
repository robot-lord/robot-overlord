**Status:** in progress (sessions built, 0.20.0-dev.1; the overlord on OpenCode and `send_to_session` built on `codex-overlord`, 0.22.0-dev.3; import to come)

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
- **Overlord**: config in the overlord folder's `opencode.json`
  (`mcp.servers.rlord`, and `permissions` rules), not flags. Built: see
  "The overlord on OpenCode".
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
5. Later: import, messaging via the API, the overlord on OpenCode.

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
  OpenCode's server (Brandon's call). The listing runs on a thread of its
  own (one at a time): the request that asked gets the list as last
  listed, at once; the next one gets the fresh list.
- **Launch creates the session first.** The TUI has no model flag, creates
  a session only with its first message, and doesn't notice one created
  under its id afterwards (all checked live). So the pane command is
  `sh -c '<create, then exec>' opencode <{id, location, model}> -s <id>
  [--prompt …]`: OpenCode's own client creates the session with its
  model, then the TUI opens on it and the prompt runs on that model. The
  create step runs in the background, polled for up to 10s and killed
  after (plain sh, nothing left behind); when it fails or is cut off,
  its stderr is printed in the pane with a line from rlord, held 3s, and
  the TUI starts anyway (`CREATE_LIMIT_S`, `CREATE_PAUSE_S`; the
  daemon's "starting with no row" window is longer than both). The
  session exists from the start, so a promptless launch shows "your
  turn" at once. Resume: `opencode::exists` is three-way. A session
  OpenCode has is opened as it is (`opencode -s <id>`: it keeps its own
  model, and the `resumed` log entry says so); one it surely doesn't have
  is created again with the launch's model; when its database can't be
  read, the session is opened as it is and nothing is held to confirm
  (never created over, so a model chosen in OpenCode is never replaced). `Launch::deferred` (kv `deferred:<session>`,
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
  (`time_busy` / `time_idle`), vetoed by `GET /api/session/active` (a
  "busy" row the server doesn't list as running is over: its turn died
  with an earlier server; logged `agent_stopped` from "opencode turn not
  running", times untouched), pending permissions and forms from the
  server, for the session and those sub-agent sessions under it whose row
  is busy (a child that can't be asked counts as waiting on nothing). A
  session that was waiting and no longer is goes back to working logged
  `answered`, times untouched (as from the stream). No server: the
  session rows alone, each tick, and only for what they can say: a
  starting session is up; a "busy" row is believed only for a session that
  was starting; once nothing has been there twice running (no
  `service.json`, or no TCP connection at its address; an error answer
  such as a 401 means it is there, and resets the count, as does having
  no live sessions) it is gone, and a session still working becomes your
  turn (turns run in the server; "updated" doesn't move). Stopped: the
  pane, two looks (`discover::pane_looks` / `stop_from_pane`, shared with
  Codex). A
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
  pipe: one that starts OpenCode's server would leave it holding the pipe
  (`core::process::run_to_file`, shared). Summary runs' files go in
  `<data>/summary-output` (uuid names), not the excerpt runs' folder
  `<data>/summaries`; leftovers are swept when the daemon starts.
- **Config**: `[session] opencode_model`, `[archive]
  opencode_summary_model`.
- **Forms, CLI, overlord**: `--agent opencode`, the form's Agent hint,
  `agent` on the MCP tools; overlord.md / mastermind.md; README and guide.
- **Demo**: `scripts/demo/bin/opencode` (writes an OpenCode database; no
  server) and one OpenCode session. Its status comes from the session row;
  the demo can't show an OpenCode session that needs you.

## The overlord on OpenCode (branch `codex-overlord`)

How it works is in `docs/architecture.md` ("The overlord") and the guide.
Facts found, against OpenCode 2.2.7 and its docs (opencode.ai/docs/config,
/mcp-servers, /permissions, /agents):

- `opencode mcp add` writes `{"mcp": {"servers": {"<name>": {"type":
  "local", "command": [...]}}}}` to the project's `opencode.json`; a
  server takes `environment`, added to the shared server's own.
- MCP tools are named `<server>_<tool>` for permissions. Rules are
  `{action, resource, effect}`; **the last match wins**, and every agent
  starts from "allow everything", so the overlord's config sets `*` to ask
  first, then allows `read`, `glob`, `grep`, rlord's own subagents (by
  name) and `rlord_*` (`opencode::overlord_config`), and repeats the rules
  under `agents.<name>` (see the #10 review below).
- OpenCode calls MCP tools from a Code Mode script (`execute`): the pane
  showed `✓ execute › rlord.list_sessions`.
- Subagents: `.opencode/agents/<name>.md` (`description`, `mode:
  subagent`, body = instructions); they run as child sessions and use the
  folder's rules. OpenCode reads `{agent,agents}/**/*.md` (in its binary),
  and `opencode debug agents` in the folder lists `rlord-tidy` and
  `rlord-import`.
- No effort setting, no flags: `write_files` returns no arguments.

Verified live (one throwaway overlord in a scratch rlord with its own XDG
dirs, tmux socket and OpenCode service on its own port; no credentials
there, so only the free models existed; `opencode/glm-5.3-flash-free`):

- `rlord overlord ask` → `rlord.list_sessions` ran with no permission
  prompt and was answered by the scratch daemon; status went working, then
  your turn, from the server's events.
- "Run `touch probe.txt`" → OpenCode asked ("Permission required"), rlord
  showed "needs you" (`permission_request` from the event stream, then
  `answered` when rejected), nothing was written. This is also the first
  real permission prompt seen through the event stream: it maps as written.

Not exercised: the idle fresh restart.

### PR #10 review (checked live, OpenCode 2.2.7, `glm-5.3-flash-free`)

- **Code Mode (`execute`)**: no longer allowed by name (it falls to `*`
  ask). It needs no rule: `execute › rlord.list_sessions` still ran with
  no prompt, so OpenCode checks the tools a script calls, not the script.
  Inside a script, `import "fs"` / `"child_process"` fail ("isn't
  available in Code Mode"), the built-in shell and write tools aren't in
  its catalog, and `fetch` asked ("Permission required: WebFetch").
- **The user's global config**: `opencode debug config` / `debug agents`
  show the merge. Top-level rules from the user's global config come
  before the folder's (a global `bash: allow` lost to the folder's `*`
  ask: `touch` asked). But agent-level rules (`agent.build.permission` or
  `agents.build.permissions`) are applied after every file's top-level
  rules, so a global agent-level `bash: allow` came last and would have
  won. Mitigated: the folder repeats its rules under `agents.build`,
  `agents.plan`, `agents.rlord-tidy`, `agents.rlord-import` (folder after
  global at that level too); with a global `agent.build` shell allow in
  place, `touch` still asked. Also `subagent` is allowed only for rlord's
  own: starting `general` asked. Not covered: a custom primary agent the
  user switches to by hand in the overlord's TUI.
- **A subagent pass**: "a tidy pass with rlord-tidy" → `Rlord-Tidy
  Subagent` ran `triage` with no prompt.
- **`rlord mcp --socket`**: the scratch OpenCode service had been started
  with another `XDG_RUNTIME_DIR` than the daemon's; the overlord's tool
  calls reached the daemon anyway.
- **Found: `overlord ask` approved a permission prompt.** Typed while the
  overlord was waiting on one, its Enter landed on "Allow once" and the
  command ran. `ask` now refuses while the overlord needs you (checked:
  refused with a message; the prompt stayed).
- **`send_to_session`**: the overlord passed "reply with only the word
  PONG" to an OpenCode session (answered). By the RPC directly: to a
  working session it was refused; with `when_idle` it was queued, a second
  replaced it (`replaced_pending: true`), and the second was delivered when
  the turn ended (`message_delivered`); to the overlord itself, refused.

All throwaway OpenCode sessions (overlord, target, tidy child) were
deleted and the scratch service stopped.

### PR #10 review 2 (checked live, OpenCode 2.2.7, free model "Big Pickle")

Scratch HOME and XDG dirs (no credentials), own service port.

- **Dialogs on screen** (`opencode::dialog_shown`), as captured: a
  permission request ("△ Permission required", `Allow once  Always allow
  Reject`, "⇆ select  enter confirm") and a question (numbered options
  with no marker, "↑↓ select  enter submit  esc dismiss"). Live: with the
  overlord's status forced to working over a permission request, `rlord
  overlord ask` was refused and the dialog stayed.
- **Every agent gets the folder's rules**: `general` and `explore` added,
  plus the user's global agents by name (`opencode::user_agents`). Where
  OpenCode 2.2.7 reads global agents from (checked with `debug config` /
  `debug agents`): `opencode.json` and `opencode.jsonc` in its config
  folder (not `config.json`), the Markdown files under `agent/` and
  `agents/` (nested ones named by path, `team/nested`), and
  `$OPENCODE_CONFIG`, `$OPENCODE_CONFIG_DIR`, `$OPENCODE_CONFIG_CONTENT`.
  Live, with a global `agent.general.permission.bash = allow` (and the same
  on `build` and a Markdown subagent `shelly`): `debug agents` in the
  overlord's folder put the folder's `*` ask after the user's `shell`
  allow for each; asked to have `general` run `touch probe-general.txt`,
  OpenCode asked to start `general`, then asked again for the `touch`
  (rejected; nothing written).
- `confirm_sends`: an ask rule for `rlord_send_to_session` after
  `rlord_*` (unit-tested; not run live).

The scratch service, its sessions and data were deleted.

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
- `GET /api/session/active` is `{"data": {"<id>": {"type": "running"}}}`
  while a turn runs and `{"data": {}}` once it has ended, matching the
  row's `time_busy` / `time_idle` (two short turns, polled; the free
  models answered with a usage-limit error, so the turns were brief, but
  the session was listed during them and not after).
- `opencode api …` with no server running starts the background service
  itself (scratch XDG dirs, own port: answered in about 0.1s). When the
  service can't start (its port taken), it fails after about 20s with
  exit 1 and the reason on stderr: hence the bound on the create step.

Not exercised against the real thing: a permission request (events and
the pending list are from the OpenAPI spec; whether a session waiting on
one is listed by `/api/session/active` too: pending wins over that veto
either way), `opencode run … --fork --archived` (that `--archived`
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
  `hand_off`, and neither overlord session could run on OpenCode. (Since
  PR #7 the overlord is one session, with no `hand_off`. It can run on
  OpenCode now: the bridge's `caller` is then whatever session's pane
  started the shared server, and is only logged.)
- (Batch 2) Creating the session from the daemon through `Service` when
  `service.json` answers: launches run under the DB lock, so a request
  there would stall every other call; the bounded wrapper covers it.
- (Batch 2) Setting a different model only on sessions created after the
  launch was held: with `exists` three-way, a session is only created
  over when OpenCode's database surely lacks it, so the deferred model is
  only ever applied to a session rlord's launch made.

### PR #10 review 3

- `opencode::dialog_shown` reads only the bottom box (the last run of
  `┃` lines and the line under it) and needs a dialog's own line
  (`△ Permission required` opening a line, `Allow once` with `Reject`,
  `enter submit` with `esc dismiss`, `⇆ select` with `enter confirm`), so
  the same words in the transcript don't hold sends (PR #10 review 3).

## Owed

- Import (`session_v2` has directory, title, model).
- `send_to_session` types into the pane; `POST …/prompt {delivery:
  queue}` could deliver to a busy OpenCode session without typing.
- A draft in OpenCode's prompt box isn't detected (Claude's and Codex's
  are): a send would be added to it. Needs a capture of the box with
  attributes (its placeholder is drawn differently from typed text).
- Sub-agent tracking (child sessions: `parent_id`).
- A question form, checked by hand (a real permission prompt was: see
  "The overlord on OpenCode").
- Screenshots retaken with the OpenCode demo session (README pass, 0.22.0-dev.4).
- `docs/architecture.md` got the OpenCode facts in its agent section; the
  larger reconcile owed by the Codex doc still stands.

## Codex Remote Control: deferred

Not supported until Codex makes it official (Brandon, 2026-11-13); findings
and branch `codex-remote` in `docs/working-archive/codex-daemon.md`.
