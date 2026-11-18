# Robot Overlord — architecture plan

Status: v1 plan, 2026-09-11, kept current as the design changes.

## Locked decisions

| Decision | Choice | Why |
|---|---|---|
| Language | **Rust** | Commodity crates cover every need (tokio, rusqlite, notify, ratatui, rmcp); we want a resident daemon that idles at ~zero cost. |
| Frontend | **Terminal (TUI)** | Matches how we develop. GUI stays cheap to add later because the core is a daemon any client can speak to. |
| Process substrate | **tmux** | It buys real things: sessions survive SSH disconnects and client crashes, `respawn-pane -k` gives atomic in-place restart, attach UX is free, and every agent CLI runs unmodified in a pane. Not worth reinventing with a hand-rolled PTY supervisor. |
| Control plane | **Live daemon (`rlordd`)** + thin clients | A CLI-only model is slow because every call is a cold start (process spawn → SQLite open → several tmux round-trips → pane scrapes). A resident daemon keeps state hot, holds one persistent tmux control-mode connection, and pushes events instead of being polled. |
| AI control surface | **First-class MCP server** (CLI kept as the human/scripting surface) | Agents call tools directly — no Bash spawn, no arg parsing, no output re-parsing. Both surfaces are thin facades over one internal API, so nothing is MCP-only or CLI-only. |
| Agent support | **Claude Code first-class, Codex second**, generic adapter trait for the rest | Matches the measured cost curve: Claude is the easiest deep integration (accepts a minted `--session-id`); Codex has a known-good recipe (notify program + rollout gates + fork fallback). |

## System overview

```
                                   ┌────────────────────────────┐
  ┌──────────┐   UDS (JSON-RPC)    │  rlordd (daemon, tokio)    │
  │  rlord   │◄──────────────────►│                            │
  │   CLI     │   + event stream   │  ┌──────────────────────┐  │
  └──────────┘                     │  │ core API (one impl)  │  │
  ┌──────────┐                     │  └──────────────────────┘  │
  │  rlord   │◄──────────────────►│   │        │         │     │
  │   TUI     │                    │   ▼        ▼         ▼     │
  └──────────┘                     │ state    status   mailbox  │
  ┌──────────┐    UDS (proxied     │ store    engine   /events  │
  │  rlord   │◄──────────────────►│ (SQLite)                   │
  │ mcp shim  │     MCP)           │        tmux supervisor     │
  └────┬─────┘                     │   (persistent control-mode │
       │ stdio                     │    client + exec fallback) │
  ┌────▼─────┐                     └──────┬──────────────┬──────┘
  │ Claude / │                            │              ▲
  │ Codex in │◄── pane is the agent ──────┘              │ UDS
  │ tmux pane│                                     ┌─────┴─────┐
  └──────────┘                                     │  rlord    │
                                                   │ hook emit  │ ← Claude hooks /
                                                   └───────────┘   codex notify
```

One cargo workspace, one installed binary `rlord` with subcommands; the
daemon is `rlord daemon` (started on demand by the first client that needs it, or by an optional service unit: a launchd user agent on macOS, a systemd user unit `rlord.service` on Linux; when one is installed, starts and restarts go through it). The daemon logs to `~/Library/Logs/rlordd.log` on macOS and `$XDG_STATE_HOME/robot-overlord/rlordd.log` elsewhere. Crates:

- `rlord-core` — domain types, state store, event log, status engine,
  mailbox, adapter trait + claude/codex adapters, tmux supervisor.
- `rlord-daemon` — API server (UDS JSON-RPC + subscriptions), MCP server,
  hook ingest, reconciliation loops.
- `rlord-cli` — thin client subcommands, `hook emit`, `mcp` stdio shim.
- `rlord-tui` — ratatui client.

### Why the daemon fixes the responsiveness problem

- State reads (`ls`, sort, filter) never touch tmux or disk on the hot path:
  the daemon holds the session table in memory, persisting write-through to
  SQLite. A CLI `rlord ls` is one UDS round-trip.
- tmux interaction goes over **one persistent control-mode client**
  (`tmux -C`) owned by the daemon: pane lifecycle, activity events, and
  output capture are streamed/pushed by tmux itself rather than re-spawning
  `tmux` binaries per query (forking tmux for every query is what makes
  such tools sluggish). Plain `tmux` exec remains as fallback for the
  few operations control mode handles poorly.
- Status is event-driven: hook events arrive over UDS within milliseconds of
  Claude firing them; clients receive push notifications on a subscription
  stream instead of polling.
- The hook path is latency-critical (Claude blocks on synchronous hooks):
  `rlord hook emit` does no SQLite, no tmux — it writes one JSON line to
  the daemon socket and waits (bounded: a second to connect, two for the
  answer) for the daemon's answer, which may carry output for the agent
  (a session's background, "Session background" below), printed to
  stdout and then acknowledged (`hook.ack`). If the daemon is down, it appends to a spool file that the
  daemon drains on startup, so hook evidence is never lost; slow or down,
  it prints nothing.

## State model

SQLite (WAL) at `$XDG_DATA_HOME/robot-overlord/state.db`. The schema encodes the
lesson from the research: **meta-context is derived from an event log, not
from throttled ad-hoc columns.**

```sql
sessions (
  id            TEXT PRIMARY KEY,   -- rlord id (ULID)
  name          TEXT NOT NULL,      -- display name (never a machine key)
  name_locked   INTEGER NOT NULL DEFAULT 0,
  agent         TEXT NOT NULL,      -- 'claude' | 'codex' | 'shell' | ...
  agent_session TEXT,               -- the agent's native conversation id
  agent_session_trust TEXT NOT NULL DEFAULT 'minted',
                                    -- 'minted' | 'hook' | 'scavenged'(never resumed)
  project_path  TEXT NOT NULL,
  tmux_session  TEXT,               -- NULL when not running
  status        TEXT NOT NULL,      -- see status engine
  launch_spec   TEXT NOT NULL,      -- JSON: cmd, flags, env, model, options
  group_path    TEXT NOT NULL DEFAULT '',
  pinned        INTEGER NOT NULL DEFAULT 0,
  sort_key      INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  archived_at   INTEGER,            -- NULL = live
  forked_from   TEXT                -- the session it was forked from (session.fork)
);
tags            (session_id, tag, PRIMARY KEY(session_id, tag));
events (
  seq        INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  ts         INTEGER NOT NULL,      -- ms epoch
  kind       TEXT NOT NULL,         -- prompt_submitted | rlord_prompt | agent_stopped | tool_use |
                                    -- state_changed | message_sent | message_received |
                                    -- launched | resumed | attached | renamed | ...
  payload    TEXT NOT NULL DEFAULT '{}'
);
messages (                          -- durable inter-session mailbox
  id         TEXT PRIMARY KEY,
  to_session TEXT NOT NULL,
  from_session TEXT,                -- NULL = human/CLI origin
  body       TEXT NOT NULL,
  kind       TEXT NOT NULL,         -- 'task' | 'info' | 'reply' | 'completion'
  created_at INTEGER NOT NULL,
  delivered_at INTEGER,             -- NULL = pending
  dedup_key  TEXT,
  UNIQUE(to_session, dedup_key)
);
```

Derived meta-context (maintained as materialized columns updated on event
insert, so sorting is an indexed query, not a scan):
`last_user_msg_at`, `last_rlord_msg_at`, `last_agent_msg_at`,
`last_state_change_at`, `last_delivery_at`. These are **distinct and honest** — no max-of-six-sources
badge heuristic. `last_user_msg_at` is the user's own prompts only: a
prompt rlord typed into the session moves `last_rlord_msg_at` instead
("Typing into a pane", below). Anything unknowable (e.g. pre-rlord history) is NULL and
displays as "unknown", never as epoch or a guess.

Sort/filter/pivot in CLI, TUI, and MCP all take the same query object:
filter by status/agent/tag/group/archived/project, sort by any derived
timestamp or name or manual order. This is the number-one product feature and
it is just SQL. The one sort that isn't is **usage** (below): SQL orders by
last message, then the daemon re-sorts by the usage it attached.

`session_context` holds a session's background from the overlord: one row
per note (`kind = 'note'`) or link to a related session (`kind = 'link'`,
`body` its id), with `seen_at` once a hook has pushed it to the agent.
See "Session background".

**rlord creates its files private to the user.** They hold prompts,
launch commands, transcript excerpts and the overlord's notes. Its
folders (the data dir, the config dir, the log dir on Linux) are created
0700, and `state.db`, the log file, the Codex gate and per-session files
0600, at creation, by `rlord_core::private` (`dir`, `dir_under`, `touch`,
`append`, `write`). Every place that may be first to make one goes
through it: `Db::open`, the daemon lock, tmux's conf and profile files,
the Codex gate, the hook spool, `config init` and config writes, the
daemon log. SQLite gives `-wal` and `-shm` the DB file's mode. Existing
installs are not tightened (the user's decision): a folder or file made
before this keeps its mode. (The runtime dir is `Daemon::bind`'s: 0700,
set on every start, since its socket is the trust boundary.)

### Usage (the list's Usage column)

Each `session.list` / `session.get` row carries `usage` (`turns`,
`tokens`, `context`, `window`, `context_pct`; null while unknown), from
`core::usage::of(transcript)`, filled in by `daemon::usage` after the DB
lock is released:

- **turns** = prompts the human typed (`Line::prompt`); **tokens** = input
  not read from a cache (cache writes count) plus output; **context** = the
  last request's whole context. Claude writes one API message over several
  lines that repeat its usage: it counts once (`Line::message_id`).
- JSONL transcripts: the counts ride on `transcript_stats`' incremental
  cursor (only appended bytes are parsed) and are saved with it in
  `transcript_cursors`. OpenCode: indexed look-ups in `session_v2` /
  `session_message` only, cached until the database or its WAL changes.
- Claude messages Claude Code made up (`"model":"<synthetic>"`, e.g. an
  API error) and any with zero context aren't requests: they don't move
  the totals or the context. `[Request interrupted by user…]` lines aren't
  prompts.
- **Window**: what the agent reports (Codex's `model_context_window`);
  else a configured `[1m]` model is 1M (the transcript names the model
  alone); else `models::context_window` (a per-family table; a 200k model
  that sent more than 200k is taken as its 1M variant). Unknown → no
  `context_pct`, shown as `–`.
- **A request never reads a whole transcript.** It reads what was
  appended since the last read, at most 4 MB. A position that fell out of
  memory (`STATS_CURSOR_CAP`) is reloaded from `transcript_cursors`, each
  just before its row is read (all at once would evict rows not yet
  reached), so long archived lists don't re-read files. The worker reloads
  a saved position before reading too. `session.subagents` (every TUI
  selection, MCP `get_session`) follows the same bound for the main
  transcript and each sub-agent's: a sub-agent transcript with no
  position or more than 4 MB behind isn't read on the request; the
  session goes to the warm-up (`Job::Full`, so it shows as loading) and
  the request answers with the rows as they are. A transcript with no position,
  or more than 4 MB behind, goes to the warm-up worker (`Job::Usage`, its
  own pending set, so it doesn't mark sub-agents loading); the row says
  `usage_loading` meanwhile. Once read it can be queued again; one whose
  read failed isn't, this run. `session.get` (one row) may read an
  unread transcript of at most 4 MB itself.
- OpenCode rows are read per database over one connection
  (`usage::of_opencode`).
- The measure is `SessionQuery::usage` (the TUI sends its current one),
  else config `usage`. `SortMode::Usage` sorts by it: pinned first, most
  first, unknown last; equals stay most recently active first, reversed
  or not. The toolbar names the measure (the column may be hidden for
  width). Clicking the Usage header sorts or reverses like any other
  header; it doesn't switch the measure (`:usage`, config or the
  overlord's `set_list_view` do).
- Known limits, accepted: the list refetches on daemon change notices,
  so tokens and context move at turn boundaries, not mid-turn; a Claude
  message split across a daemon restart can count twice (the last
  counted message id isn't saved); a list of more archived sessions than
  `STATS_CURSOR_CAP` reloads evicted positions from the DB on each list
  (DB reads, no file reads). New model families need adding to
  `models::context_window` (issue #56).

## tmux supervisor

- One tmux session per rlord session, named `rlord_<ulid>` — the name is an
  id, never derived from the display name (rename ≠ tmux churn).
- Launch: `tmux new-session -d -s rlord_<id> -c <project> <agent argv>` with
  `RLORD_SESSION_ID=<id>` (and agent-specific env) exported into the pane.
- Restart/resume: adapter builds the resume argv → `respawn-pane -k` if the
  tmux session survives, else recreate. Spawn-lock per session prevents
  restart storms.
- Attach: TUI suspends itself and execs `tmux attach -t rlord_<id>` (detach
  returns to the TUI); CLI `rlord attach <name|id>` does the same.
- Reboot recovery: on daemon start, reconcile DB vs `tmux ls`. Sessions whose
  tmux is gone become status `stopped(resumable)`. Auto-resume is a per-session
  policy flag (default off: a crash and an intentional kill look alike — but
  opt-in-able per session or group).

## Agent adapter contract

`rlord_core::agents::Agent` (an enum, one variant per agent; each agent's
facts live in its module) is what the rest of rlord asks:

- **Commands**: `launch` (a new conversation; returns the id and transcript
  path when rlord can choose them), `resume`, the same `launch` for a
  fresh conversation in the pane, and `fork` (a user fork: the agent's
  own command, `Fork::Native`; or its server first, `Fork::Server`, then
  `open_fork`). See "Forking a session".
- **Identity**: `mints_id`. Claude takes a caller-chosen id (`--session-id`),
  so the transcript path is known at launch. Codex assigns its own: the id
  is bound from the first hook that carries it (`trust = hook`), or, while
  hooks aren't trusted, from Codex's thread index (the thread started in the
  session's folder since launch; `trust = minted`, rlord's own attribution,
  confirmed by a later hook). Only `minted` or `hook` ids resume; a
  `scavenged` one never does. A Codex session with no bound id can't resume.
  OpenCode takes a caller-chosen id too, but its TUI creates a session
  only with the first message and has no model flag: the pane command
  first creates the session through OpenCode's server (`opencode api POST
  /api/session`, with the id, folder and model), then execs the TUI on it
  (`opencode -s <ses_…>`). `Launch::deferred` names the model
  for `daemon::opencode` to confirm afterwards (set it if the create step
  didn't take; log it if it can't be).
- **First prompt off argv** (`daemon::first_prompt`): a start that carries
  the user's words writes them (with any background pointer line) to
  `<data>/sessions/<id>/prompt.md` (0600), so they never show in `ps`,
  the pane command, `launched_args` or the `launched` event, and aren't
  bound by the argument limit. Codex gets a fixed line naming the file as
  its prompt (`first_prompt::pointer_line`, starting `[rlord session
  <id>]` so a clipped first message still tells sessions apart; that line
  is what its conversation is bound by). Anything that shows a
  conversation's task (triage, summaries, `session_brief`) reads the
  launch spec's words in its place (`first_prompt::task`). OpenCode's is
  POSTed to its server (`POST /api/session/<id>/prompt`, `{text}`) once
  the session exists and its model is settled (kv `first_prompt:<id>`,
  carried over a restart, `opencode::apply_first_prompt`). Refused (no
  connection, an error status) or not there after 30s: it is typed in at
  the first idle turn from its own slot (`send::queue_first`, before and
  apart from the overlord's queued messages). Sent with no answer back:
  logged as unknown, never typed again (it may have run). A session that
  stops first keeps it (file and kv, marked `stopped`) for its restart
  to send; one archived or gone first has it settled. The file is
  removed once it is sent, typed or settled, and the log has a
  `first_prompt` event. The overlord's messages to the session wait
  until the first prompt has gone. Claude reads the file on stdin
  (`exec … < prompt.md`) once live check V1 passes
  (`first_prompt::CLAUDE_STDIN`); until then its prompt stays on argv.
  Fixed text with no user words (`context::pointer_only`) stays on argv.
- **Hooks**: `install_hooks` merges `rlord hook emit <agent>` into the
  agent's hooks file (Claude `settings.json`, Codex `hooks.json`; same
  shape, others' hooks kept, one backup), `map_hook_event` turns an event
  into (log kind, status). Codex hooks need the user's trust (`/hooks`);
  rlord never bypasses it. OpenCode has none (`uses_hooks` is false).
- **Transcripts**: where a conversation is written (`transcript_path`,
  `all_transcripts`), read through `agents::transcript` into one shape for
  every consumer (excerpts, stats, search/read, context size, model, turn
  state). An OpenCode conversation is rows in OpenCode's SQLite database
  (`session_v2`, `session_message`), not a file: its "path" is
  `<opencode.db>/<session id>`, and `agents::opencode` answers the same
  questions from the database (opened read-only; only those two tables,
  since the database also holds OpenCode's credentials).
- **Capabilities**: `has_remote_control`, `tracks_subagents`,
  `runs_overlord`, `importable` (callers ask instead of comparing names).
  Every agent runs the overlord; the other three are Claude-only today.

Status has more than one source, and that is not part of `Agent`: hook
events (`methods::hook_emit`); for agents not reporting through hooks
`daemon::discover` (the agent's own index and transcript tail, a slow
tick); and for OpenCode `daemon::opencode`. OpenCode v2 runs one
background server per user (address and password in its `service.json`,
re-read on every connect) and the TUI in the pane is a client of it, so
that task holds one connection to the server's event stream
(`agents::opencode_service`: filtered event types, no replay), matches
events to sessions by the minted id, resyncs after each connect from
OpenCode's session rows, checked against the turns the server says are
running (`GET /api/session/active`: a row left "busy" by a server that
died mid-turn is over), and the server's pending permissions and forms,
falls back to the session rows alone while no server is reachable, and
takes "stopped" from the pane. It runs only while there are live OpenCode
sessions. Archive summaries are written by the session's own agent
(`claude::summary_argv`, `codex::summary_argv`: one-shot, nothing saved,
nothing it could change; OpenCode: its server's `generate` call, which
adds nothing to the conversation, else a throwaway archived session that
is deleted afterwards).

### Codex on its shared background server (daemon mode)

Codex runs one app-server daemon per user; a TUI started without
`--no-daemon` is a client of it, and the daemon runs the turn and its
hooks. `[session] codex_daemon` (off by default) puts new Codex sessions
there: launch spec `codex_host = "daemon"`, set only at creation
(`methods::session_new`), never for the overlord or a session whose
internal `extra_args` need the pane (only rlord's own wiring passes those;
`codex::needs_embedded`: Codex runs any `-c` / `--config`, `--enable`,
`--disable` or `--search`, in any of their spellings (`-cK=V`,
`--config=V`, …), in the TUI's own process; `-m` / `--model` doesn't). So
a daemon-mode pane can't be given `-c check_for_update_on_startup=false`
(the overlord's) and, like an in-pane session, can open on Codex's update
prompt. `spec_args` adds `--no-daemon` for every other Codex session. The
rest is `daemon/src/codex_daemon.rs`, with the client in
`agents::codex_server`. Daemon mode is plumbing: no new-session hint or
list marker, only the details line (`codex  shared server · its turn goes
on when the pane closes`).

- **Hooks go by thread, not pane.** The daemon's hooks carry the
  environment of whatever started the daemon. So a daemon-mode pane gets
  no `RLORD_SESSION_ID` (unset, so a daemon its TUI starts can't pass one
  on), and rlord starts the daemon itself (`codex app-server daemon
  start`, `RLORD_SESSION_ID`, `TMUX` and `TMUX_PANE` removed) before such
  a pane when none runs, with Codex's own folder first on `PATH` as the
  pane has it (not tried again for a minute after a failed start, never
  failing the launch; when none runs and rlord didn't start one, the pane's TUI does,
  with the pane's environment, or runs the turn in the pane). `rlord hook
  emit codex` without `RLORD_SESSION_ID` reads the gate file
  (`<data>/codex-daemon-threads`, `agents::codex_gate`: each live
  daemon-mode session's bound thread, and the folder of each still waiting
  for one) and sends only what it admits, marked `by: "thread"`; anything
  else exits after that one read, unspooled. `codex_daemon::route` decides
  whose an event is: a live daemon-mode session bound to its thread
  (whoever the event says sent it), else the session `RLORD_SESSION_ID`
  names if that one is in the pane, else a first `SessionStart` in the
  folder of exactly one waiting daemon-mode session, from a thread started
  fresh (`source` `startup` or none, `codex_gate::starts_fresh`): a
  `fork` there, such as the `codex exec fork` of rlord's archive summary
  of another session, never binds it. A stopped session's
  thread is ignored. Matched by thread, there is no pane check
  (`foreign_hook`), and a `SessionEnd` while the pane is up is ignored (a
  client leaving ends nothing). `sync_gate` rewrites the file whenever
  sessions change, and every `discover` pass.
- **Binding** without trusted hooks: Codex's index lists a daemon thread
  as an app-server client's (`source = 'vscode'`), which
  `codex::threads_started_in` accepts for daemon-mode sessions only.
- **A pane that ends isn't a stopped agent.** `discover` asks the daemon
  (`thread/read`; the transcript when it can't be asked) every few passes
  and stops the session only once no turn runs; meanwhile it stays as the
  hooks say, logged once as `pane_gone_turn_running`.
- **Fresh restarts don't interrupt**: `session_fresh` is used only for
  the overlord ("Fresh restarts" below), which never runs on the daemon.
  It refuses a daemon-mode session: it would leave the turn running there
  while a new thread starts.
- **Stop and archive interrupt first**, before the DB lock: Escape into
  the pane when it is up with no dialog (it interrupts from any client),
  else `turn/interrupt`; then up to 4s for the turn to end. The answer and
  the log say `turn: interrupted | still_running`.
- **Restart re-attaches** (`codex resume <id>` without `--no-daemon`).
- **Launch, then bind; never create the thread first.** A thread made by
  `thread/start` has no rollout until its first turn, so `codex resume
  <id>` on it fails ("no rollout found for thread id"; 0.167.2 / daemon
  0.169.0). The pane's TUI creates it; binding is as above.
- **Another client on the same thread**: a turn started there streams
  into the rlord pane, but the pane's composer reads as idle (`› Ask
  Codex…`, no "esc to interrupt"), so `Agent::at_prompt` alone can't tell.
  Its hooks are matched by thread, so the session turns working and
  `send_to_session` and held deliveries wait. Typing itself also asks the
  daemon (`send::type_message`, `codex_daemon::busy_for_send`, half a
  second per step) and holds back while any turn runs on the thread,
  waiting on input included (a usage-limit resume too, which doesn't wait
  on the status). What's left: a daemon that can't be asked (the status
  decides), and a thread the session isn't bound to (after `/clear` or
  `/new`).
  Escape in either client interrupts.
- **The control socket** (`~/.codex/app-server-control/
  app-server-control.sock`) speaks JSON-RPC over WebSocket over the Unix
  socket. rlord's client only reads (`thread/read`, `thread/turns/list`)
  and interrupts; it never subscribes to a thread. The daemon keeps the
  first `clientInfo.name` it is given after it starts as the originator of
  the threads it then creates, so the client gives the TUI's own name.
- rlord never runs `daemon update`, `restart` or `stop`; the daemon updates
  itself and can be newer than the CLI (`rlord agents` shows its version
  then).

## Status engine

Same three-tier degradation the research validated, implemented once:

1. **Native events** (Claude and Codex hooks; OpenCode's server events),
   freshness-windowed.
   Vocabulary: `starting | working | waiting_input | idle | stopped | error`.
2. **Pane hints** — OSC pane-title spinner check (zero-capture fast path).
3. **Pane capture + patterns** — via the control-mode stream where possible;
   activity-gated capture otherwise; error-banner classification for Claude.

Every status transition is an `events` row and a push to subscribers; that is
what makes the TUI live and `last_state_change_at` trustworthy.

### Usage limits (`daemon/src/limits.rs`)

"Limited" is not a stored status: it is read from the transcript and laid
over the status in what the list, the CLI and `list_sessions` show, so
hooks and `discover` keep the stored status to themselves.

- **Detection** is `transcript::limit_hit`, on the transcript's tail.
  Claude Code ends a turn that hit a usage limit with a synthetic
  assistant line: `isApiErrorMessage: true`, `apiError:
  "usage_limit_reached"`, `error: "rate_limit"`, the limit's `status:
  "rejected"`, and the reset time as
  epoch seconds in `apiErrorParams.rate_limit_info.resetsAt` (also
  `quotaLimits.resetsAt`), with `rateLimitType` (`five_hour`). The text
  ("You've hit your 5-hour limit · resets 1:30pm (America/Los_Angeles)")
  is never parsed. Other API errors carry `isApiErrorMessage` too
  (`apiError: "computer_slept"`), so that flag alone is not enough, and
  neither is `error: "rate_limit"`: a plain 429 the CLI retries has it
  too. It takes `apiError: "usage_limit_reached"` or `status:
  "rejected"`. The turn ends through the `StopFailure` hook (any API
  error), not `Stop`, so rlord installs that hook too and maps it to
  `idle`; the hook doesn't say which error, so the transcript is still
  what tells a limit. Seen on Claude Code 2.1.322.
  **Codex** (0.167, from its source: no real one seen yet) keeps no line
  of its own for a limit: its `error` event isn't written to the
  rollout. The turn's `task_complete` carries the error that ended it,
  `codex_error_info: "usage_limit_exceeded"` (`codex::limit_ended`;
  that also covers an API key out of quota, which has no reset), and
  just before it a `token_count` holds the 429's rate-limit snapshot
  with the exhausted window at `used_percent` 100. The reset is that
  window's `resets_at` (epoch seconds); Codex's own tests send the 429
  without one, so failing that it is the same window's `resets_at` from
  an earlier `token_count` in the tail (every answer carries one). No
  exhausted window, or no reset found: not a limit (`codex::limit_reset`).
  The error's text ("try again at 3:00 PM") is never parsed.
  **OpenCode** (2.2.7, from its source) records a refused step on its
  assistant row: `finish: "error"`, `time.completed`, `error: {type,
  status, response: {body}, retryAfterMs}`. Its own service's limits
  have a reset, worked out as OpenCode's timeline does
  (`opencode::usage_limit`): `provider.free-limit` and
  `provider.go-limit` reset `retryAfterMs` after the step ended (at most
  45 days), `provider.budget-limit` and `provider.model-budget-limit` at
  the `resetsAt` in the response body. Other types have no reset
  (`provider.chatgpt-limit`, `provider.quota`) or are throttles OpenCode
  retries itself (`provider.rate-limit`). A conversation is limited when
  its latest assistant row is one, touched by a `user` row after it
  (`opencode::limit_hit`: two indexed look-ups in `session_message`).
- A session is limited while that record is the last thing in its
  conversation (no prompt the user typed and no real answer after it:
  `LimitHit::touched`) and the reset time is ahead. A tick (15s) stats
  each live session's transcript and re-reads only those that changed
  (an OpenCode conversation, rows rather than a file, is read every tick)
  (one scan at a time; a file's stamp is kept only once what was read
  from it is saved; a tail without `usage_limit_reached`, `"rejected"`
  or `usage_limit_exceeded` isn't parsed; a record with no readable time is dated when first
  seen, `limits::dated`);
  kv `limits` holds the current ones, each until a minute past its
  reset (`limits::held`), and rows carry `limit` (`limits::annotate`:
  `reset` once the reset time has passed, shown as "limit reset").
- **Nothing else is typed into a limited session** (it would only get
  another limit reply). `send_to_session` refuses it ("stopped on a usage
  limit until …"); with `when_idle`, and for a message already queued,
  it waits for the reset plus a minute, as a resume does
  (`send::refusal`/`next`, from `limits::limited`). Its hook state reads `idle` all the same.
- **Resuming is only ever asked for** (`limits.resume`: the
  `resume_after_limit` tool, `rlord resume-after-limit`, the list's
  menu). Each session gets a job in kv `limit_resume`, so a daemon
  restart keeps it. `limits::step` decides each tick: wait until reset +
  60s; drop the job if the conversation went on since the limit; else
  type `[limits] resume_message` in through `send::type_message` (under
  the session's lock, with all of `type_into`'s screen checks). It sends
  on `idle`, and on `working` too: the untouched limit record is where
  the turn ended, so a session whose `StopFailure` hook is missing
  (older install) still resumes. Not on `waiting_input`, which is
  blocked on a prompt. Scheduling takes the session's lock, and a job
  only writes back over itself (same `limit_at` and `asked_at`), so
  asking again while a job runs replaces it cleanly. After
  sending, a newer untouched limit record within 15 minutes reschedules
  the job to that reset (five sends at most), unless the user typed a
  prompt since the send (`last_user_msg_at` more than 5s after the
  job's `sent_at`; one closer is taken as the job's own message, should
  its marker have missed): then the session is theirs and the job ends,
  logged as `limit_resume_skipped`. What rlord
  types (the resume itself, a `when_idle` delivery, `send_to_session`)
  never moves `last_user_msg_at` ("Typing into a pane"). The
  record a resume was sent on is kept as `sent_hit` (a hash of its line),
  so it stays the old limit even if nothing gets written after it. The overlords are never
  resumed this way: the user switches to another agent's. Cancelling
  (`limits.resume` with `cancel`) removes the job under the session's
  lock and logs `limit_resume_cancelled`; the limit itself stays.
- **`status:limited`** is not a stored status, so `session.list` takes
  it out of the DB query and keeps the annotated rows with a `limit`
  (or any other status asked for alongside it).

### Background work (`daemon/src/background.rs`)

"Waiting on background work" is not a stored status either. It is laid
over `idle` (your turn) as `async_tasks: N` on the row (with
`async_work`: what and since when). It shows as `◔ bg N` in the list,
`bg N` in `rlord ls`, and status `background work` (with `async_labels`)
in `list_sessions`, `get_session`, `read_screen` and `triage`, with a usage limit's `limited_until`, `limit_reset` and `resume_scheduled` (`mcp::status_fields`). Every row goes through `annotate_status` (remote where it applies, background work, limits, `shown_status`). `status:bg` filters
it. A `waiting_input` session never gets it, and a usage limit is drawn
over it. The daemon decides the precedence once and sets `shown_status`
on each row (limited, else `background`, else the stored status). The
list, `rlord ls` and the MCP tools all read it. (`async_*`, not
`background_*`: `background` is a session's context, `session.context`.)

Status filters: the overlays and the status they are drawn over (your
turn, `background`, `limited`) match `shown_status`, so `status:turn`
leaves out the rows shown as background work or limited. The other
stored statuses (`stopped`, `working`, `error`, …) match the stored
status, so `status:stopped` still finds a stopped session a limit is
drawn over (`shown_status_matches`).

- **Only subagents set it.** An async Agent or a skill forked into the
  background means the session's work goes on. A background shell (`Bash`
  with `run_in_background`) is often a dev server or a watcher that never
  ends, so it never changes the status. Rows carry it as `async_shells`,
  and the details say "2 background shells".
- **Detection** (`agents::background`, Claude Code only;
  `Tracker::advance_within` is where another agent's adapter goes).
  - A launch is a `user` line whose structured `toolUseResult` names the
    work: `agentId` with `isAsync: true` / `status: "async_launched"` (an
    Agent) or `background: true` (a forked skill), or `backgroundTaskId`
    (a shell, also one moved to the background at its timeout).
  - A finish is a later line the harness wrote (never text a user typed)
    holding `<task-notification>` with `<task-id>` and `<status>`
    `completed`, `failed`, `killed` or `stopped`. Most are `attachment`
    lines (`attachment: {type: "queued_command", commandMode:
    "task-notification", prompt}`), the rest `user` lines with
    `origin.kind: "task-notification"`. The `queue-operation` lines that
    carry the same text have no origin field, so they don't count.
  - An id can notify more than once (a resumed agent); a notification
    for an id that isn't open is ignored. A successful `SendMessage` that
    resumes an agent (`toolUseResult: {success: true, resumedAgentId,
    revivedAgentId}`) opens it again, with the label and files it had (the
    last 32 finished agents are remembered).
  - A successful `TaskStop` finishes a task too (`toolUseResult:
    {message: "Successfully stopped task: …", task_id}`); the older
    `KillShell` result has `shell_id`. The tool result's text is never
    parsed.
- **Which sessions.** Live ones whose agent runs, the overlords
  included, plus those stopped or failed less than 15 s ago: a `/clear`
  stops a session for a moment (`SessionEnd`) in the same process, and its
  tasks go on. `Db::live_transcripts` returns ids, paths, statuses and the
  last start of each agent process, using `idx_events_session_kind`. A start is either:
  - a `launched`, `imported`, `forked`, `resumed`, `restarted` or
    `reforked` event (a fork's copied transcript holds the original's
    launches, which don't run in it), or
  - a `session_start` hook event whose `start` is `startup` (recorded
    from the hook's `source`): always a new process, so this also catches
    a restart done outside rlord. `resume` doesn't count: Claude Code sends
    it for an in-process `/resume` too, which may keep the tasks, and a new
    process is certain only when rlord resumed it. `clear` and `compact`
    keep the same process too.

  Open work is forgotten in only these ways besides a finish or a stop
  call:
  - the session has been stopped for more than 15 s, or is archived (its
    state is dropped);
  - its agent process starts again (work launched before the start is
    forgotten, since its tasks died with the old process; so is work with
    no launch time, which can't be placed after the start).

  A `/clear` moves the session to a new transcript in the same process,
  and the open work carries over to it.
- **Read.** A 5s tick takes one stat of each transcript. If its length,
  inode and mtime are unchanged since a read that reached the end of the
  file (a skipped long line included) and didn't fail, nothing is opened, and the subagents'
  own files are stat'ed only while some are open. Otherwise it reads what
  was appended since its byte offset: whole lines only, at most 4 MB a
  session and 16 MB in all per tick (the rest waits for the next), and a longer line is skipped unparsed. A cheap substring check
  picks the lines worth a JSON parse. The first look starts at the last
  4 MB, skipping its first line unless the byte before it is a newline.
  A new inode or a shorter file is read afresh, with the open work
  carried over. The read state is only touched by the scan thread. The
  rows read only what the last tick decided to show, and a change to that
  (going stale included) pushes to the list. This is not
  `transcript_stats`' cursor: that one reads everything unread at once
  and keeps usage totals.
- **Stale guard.** When nothing was written for `STALE_MS` (30 min), the
  subagents are hidden from the status and it shows your turn again, so a
  lost notification (the agent restarted with work running) can't leave
  a session stuck. "Written" is the newest mtime of the transcript and of
  each open subagent's `outputFile` and sidechain transcript (in the
  `subagents` folder of the transcript it was launched in, kept across a
  `/clear`; the 5 most recently started are stat'ed), since subagents write there, not in the
  main transcript. Hidden work stays open: a late notification still
  closes it, and it shows again once its files change. Shells are hidden
  the same way once the transcript itself has been quiet that long.

## Transcript store (raw conversation data)

Both target CLIs already persist **complete structured transcripts as local
JSONL** — the very files their own resume features replay — so getting raw
conversation data into software-land costs parsing, not capture:

- **Claude Code**: `~/.claude/projects/<cwd-slug>/<session-uuid>.jsonl`.
  Typed records (`user`, `assistant`, `system`, `attachment`, …), each with
  `uuid` + `parentUuid` (the conversation is a DAG — forks and subagent
  sidechains included, flagged `isSidechain`), ISO `timestamp`, `cwd`,
  `gitBranch`, `sessionId`, CLI `version`; assistant records embed the full
  API message (text + `tool_use` blocks) plus per-message token usage.
  Because we mint the session UUID, the path is **deterministic at launch** —
  no discovery needed. Hooks also hand us `transcript_path` in every payload.
- **Codex**: `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` —
  the rollout `codex resume` replays, complete by construction; includes
  cwd/workdir and is attributable to our session via the bound id.
- tmux `pipe-pane` raw capture is deliberately **not** the mechanism —
  terminal output is ANSI redraw soup; the JSONL is the semantic record.
  (Pane capture stays only as a status-detection fallback.)

### Storage split: metadata in SQLite, text on disk

Deliberate decision (not an accident of implementation): **transcript text
never lives in SQLite.** SQLite holds metadata and linking only — which
sessions exist, where their transcript files are, byte offsets ingested, time
ranges covered, agent/project/tags — everything needed to *scope and map*
a search, nothing needed to *perform* one.

The text itself lives as plain files:

```
$XDG_DATA_HOME/robot-overlord/transcripts/<session-ulid>/
  raw.jsonl        # verbatim copy of the agent CLI's file (fidelity; replayable)
  transcript.md    # rendered plaintext: one speaker-labelled turn per block,
                   #   [ts] prefixes, tool calls one-line summarized
  meta.json        # denormalized snapshot of the SQLite row (self-describing dir)
```

Search is **ripgrep-class file search over the `transcript.md` files**, not an
FTS index. Why this wins here:

- **Scale is on grep's side.** A heavy session is ~0.5 MB of JSONL; even years
  of daily use lands in low-GB territory, which rg chews through in tens to
  hundreds of ms warm. FTS5 would buy ranking we don't need at the cost of an
  index to build, sync, and migrate — roughly doubling storage and adding the
  classic index-drift failure mode.
- **The content is code-shaped.** FTS tokenizers mangle identifiers, paths,
  and error strings; regex search is exactly right for "find where we hit
  `ECONNRESET` in the myapp worker".
- **Plain files stay legible.** The user can `rg`/`less` the archive directly,
  back it up, sync it, and it outlives any schema decision. The rendered form
  doubles as a human-readable artifact of the session, not just search fodder.
- **Raw JSONL is grep-hostile** (one escaped mega-line per message), which is
  the whole reason for the rendered `transcript.md`. That render is the *only*
  normalization we commit to — anything cleverer (entity extraction, embedding
  indexes) is explicitly deferred until proven necessary.

Daemon pipeline: watch each live session's transcript (fsnotify + hook-event
triggers) → append-parse new lines (offset-tracked, never full re-reads) →
append rendered turns to `transcript.md` and update derived metadata. Live
sessions and archived sessions thus have identical on-disk shape; **archiving
just stops the tail and marks the row** — the copy already exists.

One search function serves everyone: query + metadata filters (project, agent,
tag, time range, archived?) → SQLite narrows the file set → rg (`--json`) over
those files → hits mapped back to session + timestamp. Exposed as
`rlord search` (human CLI, plus a TUI view), and as the MCP tool
`search_history(query, filters)` so a session can be told "go find how we
solved X in an old conversation" — with `get_context` fetching the surrounding
turns of any hit. Same code path, three doors.

**Durability caveat that makes the store load-bearing**: Claude Code prunes
old transcripts (`cleanupPeriodDays`, default ~30 days), and Codex homes can
be wiped per-account. Our mirror is therefore the durable record — context
outlives the agent CLI's retention policy.

### Parsing & schema drift

The JSONL formats are internal to the agent CLIs and drift across versions.
The design absorbs that instead of fighting it:

1. **Raw is the source of truth; the render is a cache.** `transcript.md` is
   a pure function of `raw.jsonl`, stamped with a `render_version`. A parser
   fix or a new dialect means `rlord render --stale` regenerates renders —
   no data was ever at risk, because we never parse-then-discard.
2. **Dialect cascade, not one struct.** Each line parses to loose JSON first,
   then typed extractors are tried newest-dialect-first (keyed on record
   `type`, observed fields, and the CLI `version` field when present). All
   dialects emit one common `Turn` IR that the renderer and search mapping
   consume. Supporting a new CLI version = add a dialect; old dialects stay,
   so historical archives always re-render.
3. **Tolerant by construction.** Extractors ignore unknown fields and accept
   missing optional ones (serde defaults). A line no dialect claims is still
   preserved verbatim in `raw.jsonl` and rendered as an explicit
   `[unparsed record]` marker turn — nothing silently vanishes.
4. **Corpus-pinned tests.** A fixtures directory holds sanitized real
   transcript samples per CLI version with golden-render tests. The drift
   workflow is mechanical: new CLI version misbehaves → capture fixture →
   extend/add dialect → goldens prove old versions still render byte-stable.
5. **Visibility.** Per-file unparsed-line counts are tracked in metadata and
   surfaced by `rlord doctor`, so drift is noticed when it starts, not
   when a search comes back mysteriously empty.

## Sub-agent tracking (verified 2026-10-27)

Claude Code exposes sub-agents outside the pane, and rlord consumes all of it:

- **Hooks** `SubagentStart` (`agent_id`, `agent_type`, `prompt_id`) and
  `SubagentStop` (`agent_id`, `agent_type`, `agent_transcript_path`,
  `last_assistant_message` = the result, `background_tasks` roster, `effort`)
  fire for interactive sessions via `settings.json` — confirmed with a probe.
- **Disk**: `<project>/<session uuid>/subagents/agent-<id>.meta.json`
  (`agentType`, `description`, `toolUseId`, `spawnDepth`, `requestShape`,
  `finished`) and `agent-<id>.jsonl` (full transcript, `isSidechain`,
  `agentId`, per-message usage). Claude's internal helper agents (title /
  suggestion generators) have an empty `agent_type` and no meta file — they
  are filtered out.

`subagents` table rows are upsert-merged from both sources (hooks for
latency, disk for description/tool-use id/backfill). Token totals keep the
larger value, except that `subagents.counted` records the
`STATS_PARSE_VERSION` they were counted with: totals from older counting
(or marked by `rlord reset --rescan`) are replaced by the next count from
the agent's transcript, even when lower. A sub-agent transcript that
couldn't be read gives zeros, not a count: those use the keep-the-larger
upsert. So bumping the parse version
needs no data migration: a live session is recounted by the warm-up, an
archived one when it's next viewed. `sessions` carry
`agents_running`/`agents_total`. Surfaces: TUI details card + `⚙N` in the
list, `rlord subagents`, `rlord show`. Sub-agent transcripts will join the
transcript store so `rlord search` covers them.

## Inter-session messaging

**Built today:** only the idle-prompt path below, as the overlord's
`send_to_session` (see "Passing work to a session" under "The overlord"):
typed into a session whose turn is over, or held (one message per
session) until it is. The mailbox, pull tool and Stop-hook injection
described here are the original design and are not built. (A session's
background, "Session background", reaches it through its hooks; the
work in `docs/working/session-context.md` adds Stop-hook delivery and
OpenCode's queue.)

Durable mailbox in the daemon; **delivery is channel-ranked per adapter**,
keystroke injection last:

1. **Pull (MCP)** — the recipient agent calls `check_messages` whenever it
   likes; the tool also appears in results of other rlord tool calls as a
   pending-count hint. Zero-risk, always available.
2. **Next-turn injection (Claude)** — recipient busy: its Stop hook drains the
   mailbox and returns `{decision:"block", reason:<messages>}`, so mail
   becomes the next turn. Consecutive-block cap (3) to prevent token-burn
   loops.
3. **Idle nudge (fallback)** — recipient idle at a confirmed prompt: inject
   text via `send-keys`/`paste-buffer` with post-send verification. Only used
   when 1–2 can't apply (non-Claude agents without a hook channel), because
   typing into a pane is the fragile path.

Semantics: at-least-once with `dedup_key`; `delivered_at` stamped only on
confirmed delivery; undeliverable → dead-letter event with a typed reason.
Completion reporting is just a message of kind `completion` — no magic pane
sentinels; a worker reports done via the MCP tool (or the human does via CLI).

## Control surfaces

### Internal API (the single implementation)

JSON-RPC over `$XDG_RUNTIME_DIR/robot-overlord/daemon.sock` (perms 0700 dir /
0600 socket — the socket is root-of-trust; no TCP listener at all in v1).
Methods, grouped: `session.{list,get,create,launch,restart,stop,archive,
unarchive,rename,set_tags,set_group,fork}`, `msg.{send,list,ack}`,
`events.{query,subscribe}`, `agent.{install,doctor}`. `subscribe` upgrades the
connection to a push stream (status changes, new messages, session CRUD).

### MCP server

The daemon serves MCP; each agent session gets it via a stdio shim in its
`.mcp.json`: `rlord mcp` (proxies stdio ↔ UDS; the overlord's adds
`--overlord`, see below). Because the shim runs
inside the pane, it inherits `RLORD_SESSION_ID` — **caller identity is
automatic**, so tools can default "me" and enforce policy (e.g. rate limits,
no self-restart).

Three modes. `rlord mcp` alone is read-only, for any agent session to add:
the daemon lists and accepts only `mcp::READ_ONLY_TOOLS` (look-ups that
change no session, tag, view, watermark or UI state and run no agent CLI
or model), refuses any other call, and the shim never autostarts a
daemon. `rlord mcp --overlord` serves every tool; only the overlord's
config passes it (`agents::mcp_args`, shared by every agent's wiring). The
shim sends `overlord: true` with `mcp.tools` / `mcp.call`; without it the
daemon treats the caller as read-only. A test makes every tool sit in
exactly one of the read-only and mutating lists. The gate trusts the shim: it
keeps an agent that added plain `rlord mcp` to look-ups, but it is no
boundary against local processes, which can reach the socket anyway.

The third, `rlord mcp --session --token-file <p>` (server
`rlord_session`), is a session's own look-ups (`session_tools.rs`):
`session_brief`, `related_sessions`, `session_summary`,
`search_history`, `read_history`, read-only and bound to one session by
a token. `rlord context <tool> key=value…` is the same through the
shell. RPC `sctx.tools` / `sctx.call` take the token and nothing else
naming a session; it is a dispatch of its own, so no overlord tool is
reachable through it.

- **Token.** 32 random bytes (two v4 uuids), hex, made at a session's
  first start (`methods::start_args`, the one helper every start path
  uses: `session.new`, `session.restart`, `session.fork`, a fresh
  conversation) and kept across restarts: kv `session_token:<id>` and
  the 0600 file `<data>/sessions/<id>/token`. Removed on archive; a start
  after unarchiving makes a new one. Overlords get none and are refused.
- **Visible set.** The session itself; sessions linked to it (its
  background links either way, its fork parent and forks); sessions in
  the same repo; sessions sharing a tag. Archived ones count; overlords,
  and sessions that ran in an overlord's folder, never do. Search runs
  only over that set (`search::search_in`), never outside rlord. A
  session ref resolves only within it (id, exact name, unique name
  prefix). `read_history` takes a hit's `transcript` (an older
  conversation, before a `/clear`), and only one of that session's own.
- **Wiring**, computed at every start, never stored: Claude
  `--mcp-config=<data>/sessions/<id>/mcp.json` and
  `--allowedTools=mcp__rlord_session` (`=` form: both take several
  values and the first prompt may follow). Codex: none for now
  (`session_tools::CODEX_MCP` is off until live check V5; the code for
  `-c mcp_servers.rlord_session.*` is there behind it). Codex and
  OpenCode have only the CLI, which isn't relied on inside Codex's
  sandbox. `get_session` and the launch
  and fork answers say which (`lookup`: `mcp` only when the last start's
  wiring produced flags, kv `session_lookup:<id>`; else `cli`, or `none`
  without a token), so the
  overlord copies the needed facts into `context` for the last two. A
  Claude or in-pane Codex pane's env gets `RLORD_SESSION_LOOKUP_FILE` (a
  path); OpenCode and Codex daemon-mode panes don't, since their shell
  commands run in a shared server. The variable is scrubbed with
  `RLORD_SESSION_ID` from everything shared between sessions
  (`agents::PANE_ENV`: the daemon drops it from its own environment at
  start, every pane unsets what it isn't given, Codex's server, OpenCode's
  shared server, commands rlord runs). The background ends
  with a "Looking up more" section, once the token exists, naming the
  tools and the CLI form with `--token-file` and `--socket` always
  given, so it works from any shell.
- **Bounds.** 30 calls a minute and 300 an hour per session (in memory,
  per daemon; idle ids dropped),
  answers at most 16,000 chars, no model, agent CLI or tmux.
- Same trust model as the rest: a process running as the user can read
  any token file. What the token gives is a read-only surface bound to
  the worker that can't be steered into acting on other sessions.

`create_session` and `propose_session` take the user's words (`prompt`)
apart from the overlord's notes (`context`) and related sessions
(`related`); `update_session` adds to them (`add_context`,
`add_related`); `get_session` shows them as `background`. See "Session
background". `fork_session` calls `session.fork` (see "Forking a
session"); session rows carry `forked_from`.

#### The latest code (`repo_latest`)

The user's checkouts are often behind, on another branch, or mid-edit.
`repo_latest` (RPC `repo.latest`, CLI `rlord latest`) hands the overlord
the newest committed code of a repo, as plain code (`core/src/latest.rs`
for git, `daemon/src/repo_latest.rs` for state and locks). Its time
limits, waits and retry windows are constants in those two files.

- **One tree per repo**, in the repo's own `.worktrees` (never
  `[session] workspace_dir`, which may point outside the repo where
  sibling repos' trees would collide): a detached worktree named
  `.rlord-latest`, moved to whichever ref is asked for. No session
  folder can have that name (`sanitize_branch` trims leading dots; git
  refuses a branch component starting with one). The user chose the
  repo over rlord's data dir: a copy of their code outside the repo
  looks wrong. Callers learn of a move from `ref` and `commit` in every
  answer.
- **rlord only ever touches its own tree.** It moves or removes a tree
  only when rlord recorded it at that path (or adopts it: git's own
  entry at the expected path, named and detached, so rlord's by
  construction) and, seen from inside, it is that worktree, detached.
  A session's worktree is always on a branch, so it never passes. A
  branch checked out in rlord's recorded tree is detached again; one in
  an unrecorded tree is refused. A folder git doesn't know is replaced
  only if it is recognizably a leftover of rlord's; a tree whose repo
  was moved with it is repaired by git. Edits in the tree are discarded
  on a move; the answer says how many.
- **Tree work runs in the background.** Adding or moving the tree (it
  can take minutes) is a job on its own thread under the repo's move
  lock; the call waits a bounded time and otherwise answers
  `tree: "preparing"` with no path. A job's result goes only to the
  calls waiting on it: a later call runs its own, so no old result or
  old error is handed back. At most one re-add per call, and no call
  repeats itself.
- **Paths as git has them.** The tree's path is resolved the way git
  reports worktree paths (a symlinked `.worktrees` is followed; in a
  guarded folder git is asked, nothing is statted), so rlord's path and
  git's always match.
- **Done means done.** A move counts as complete only once it succeeds
  (the record's commit is cleared before and set after), so a checkout
  killed partway is redone, never trusted. Only a verifiably broken tree
  is rebuilt; a lock or a time limit is an error that leaves it as it
  is. The tree's path is fixed, so there are no trees at old paths to
  track.
- **Which repo.** Identity is the common git dir (records and locks are
  keyed by it). rlord works from git's main worktree, or a bare repo's
  folder; with `--separate-git-dir` it takes the checkout it was asked
  from, and from a linked worktree of such a repo it asks for the main
  checkout's path. It works on older git, leaving out flags it lacks.
- **Private refs only.** A fetch brings the one branch into
  `refs/rlord/latest/<remote>/<branch>` and never moves the user's
  `refs/remotes/*` (a background fetch that moved `origin/x` would
  weaken their `--force-with-lease` and change their `git status`). A
  private ref nested with the target is deleted first; long-unused ones
  are dropped, exactly by remote and branch.
- **Which commit.** The remote's side is rlord's private ref when this
  call just fetched it (even behind the user's own `origin/<b>`: a
  force-push back to an older commit), else the newer of the two. The default branch (no `ref`), a launch's base, and
  any `<remote>/<b>` are the remote's commit even when the local branch
  is ahead (a warning says how far); `<remote>/<b>` never falls back to
  a local branch. A plain named `ref` is the newer of local and remote;
  diverged, both sides come back and the overlord asks the user, then
  passes `prefer`. Commits that can't be compared are no divergence (a
  warning, the remote's). A named branch not here yet says why (still
  fetching, offline, failed), not that it doesn't exist.
- **Default branch and remote.** `origin`, else the only remote, else
  none (the local branch; `upstream` is owed). The default branch comes
  from the remote's HEAD as the repo knows it, rlord's record, the
  remote asked (recorded, and a failure backed off), then
  `main`/`master`. The tool may show the checkout's own branch when none
  of those answers, never as the default; a launch never uses it and
  starts at HEAD instead, saying why. A default branch the remote no
  longer has is asked of the remote again. A launch's `prefer` counts
  only with a named `base`. rlord never runs `remote set-head`.
- **Never hang, never prompt.** Every git call has a time limit that
  kills its process group, no terminal or askpass prompt, ssh in batch
  mode (the user's own ssh command kept), no hooks, no auto gc, no
  submodules; LFS files stay pointers. Branch names never reach git as
  options.
- **Fetch cost.** A recent success is reused (`cached`, never while the
  private ref is missing). A fetch runs on its own thread, one per
  (repo, remote, branch); callers wait a bounded time and otherwise go
  on with the last known commit (`running`), and the fetch's thread
  records its own success. A launch's whole wait is capped, longer only
  for a named base that is still on its first fetch. A failure is the
  answer again for a while (shorter for a named branch). Only a hard
  limit kills a fetch: a slow first fetch killed early would never
  finish.
- **Locks.** Resolving takes no lock (one fetch per key; records change
  field by field in one DB hold). One lock per repo is held while its
  tree is moved or removed, so a launch's base never waits on a tree.
  The DB is taken briefly under that lock, never the reverse
  (`locks.rs`).
- **Records and cleanup.** One kv key per repo (`repo_latest:<common git
  dir>`). A sweep after start and then daily works from those records
  only, never scanning for repos: it drops the record of a repo that is
  gone, removes long-unused trees (keeping refs and cache) and drops
  long-unfetched private refs. `rlord latest --prune [REPO]` (the current folder's repo by
  default, every one with `--all`) removes trees, refs and record now,
  and says what it left in place and why (in a guarded folder with git
  alone); `--list` shows them.
- **macOS privacy.** In a repo inside a folder macOS guards (and not
  under a scan root) rlord reads no file itself and stats nothing
  unprompted: only git runs there (it acts on the user's repo, which
  their agent already uses), a warning says so, and the sweep skips it.
- **Never touched:** the user's checkouts, local branches,
  `refs/remotes/*`, index, stash, uncommitted work, config and hooks.
  `local` in the answer reports, read-only, where the branch is checked
  out and, for a named `ref`, how many files are uncommitted there.
- A folder that is not a git repo is answered as it is (`git: false`).
  `repo_latest` is a mutating tool (it fetches and moves a tree), so a
  plain `rlord mcp` session can't call it.

Tools (v1, the original design):
- `list_sessions(filter?, sort?, usage?)` — same query object as the CLI;
  returns meta-context (status, timestamps, tags, usage, pending-message
  counts).
- `get_session(id)` — detail + recent event summary.
- `get_context(id, {last_message | recent_activity | transcript_tail})` —
  "go get context from that other agent" without a round-trip through the
  agent itself: served from transcript read-back and the event log.
- `search_history(query, filters?)` — rg-backed search across all sessions'
  rendered transcripts (live + archived); hits carry session + timestamp and
  feed straight into `get_context`.
- `send_message(to, body, kind?, wait?)` — mailbox send; `wait` long-polls
  for a `reply` up to a timeout.
- `check_messages()` / `ack_message(id)` — the pull channel.
- `launch_session(agent, project, name?, first_prompt?)`,
  `restart_session(id)`, `archive_session(id)`, `rename_session(id, name)`.
- `report_done(summary, status)` — completion without sentinels.

A small skill/CLAUDE.md snippet documents the tools and conventions
("prefer get_context over asking the agent", "check_messages when idle").

### CLI

Thin UDS client, ~1 round-trip per command: `rlord ls` (rich table,
`--sort last-agent-msg --status waiting --tag foo`), `new`, `fork`, `setup`, `attach`, `send`,
`restart`, `archive`, `rename`, `tag`, `log <id>` (event history), `doctor`,
`daemon`. Also the plumbing subcommands: `hook emit`, `mcp`,
`claude install-hooks`, `codex install-notify`.

### TUI

ratatui client over the same API: session list with live status, the
sort/filter/pivot verbs as first-class keys, group/tag views, archived view,
preview pane (control-mode capture stream), attach on Enter, message compose.
Deliberately thin — no business logic; if the TUI dies the fleet doesn't care.
(This is the anti-`home.go` rule: the daemon owns behavior, clients own pixels.)

## rlord's private tmux server (2026-11-10)

All tmux use goes to a dedicated server, `tmux -L rlord`, started with a
generated `<data>/tmux.conf`. That config turns the status bar off, sets no
prefix, unbinds the prefix table, and turns mouse and extended keys on. It
sources `tmux.local.conf` if present. The only root-table bindings are
rlord's navigation keys. Every tmux invocation goes through
`core::tmux::tmux_cmd()`. It adds `-L rlord -f <conf>` and drops an enclosing
`TMUX` that points at the user's own server (it keeps one pointing at ours,
which tmux needs to resolve "current client"). `inside_rlord()` (the socket
in `$TMUX`) decides between switch-client and exec'ing an attach. Started
from inside the user's tmux, `rlord` nests a client. The overlord's session
alone turns its status bar on (magenta, `◆ OVERLORD`).

### The user's tmux profile, per session

`rlord tmux import` (`core::tmux_profile`) asks tmux itself what the user's
config produced:

1. Start a scratch server with the user's config (no `-f`, so tmux finds it).
   It runs under a symlink mirror of `$HOME` that leaves out the
   resurrect/continuum plugin directories, so restore plugins can't act.
   `run` and plugin scripts get the scratch server's `$TMUX`, so they only
   touch that server.
2. Start a second scratch server with `-f /dev/null`.
3. Dump keys and options from both (waiting until plugin loading settles),
   and sort them:
   - **Bindings:** every table is copied whole. `root` becomes `rlord-user`,
     minus rlord's keys, `kill-server` and restore-plugin bindings.
   - **Options:** `@` options are copied. Server options come only from an
     allow-list, with the alternate-screen-disabling overrides dropped.
     Session and window options are kept where they differ from tmux's
     defaults or from rlord's config.
4. Apply every line on the clean server, and drop what tmux rejects (e.g.
   `cursor-colour ''`).

The output files:
- `tmux.user.conf` (keys and server options) is sourced by the server
  config. Its bindings are only reachable from sessions using `rlord-user`
  or a prefix.
- `tmux.user.{session,window}.conf` are applied per session with
  `source-file -t`, plus a per-session `after-new-window` hook.

Per session, tmux options do the scoping:
- `key-table` is a session option, so the list page keeps `root`.
- A custom key table does not fall back to `root`, so rlord binds its keys
  in both tables.
- Turning the profile off unsets every session- and window-local option
  except rlord's own.

The on/off choice:
- A per-session choice is stored in `kv` (`tmux_profile:<id>`); otherwise the
  `[tmux] profile` default applies. The overlord is always off.
- The daemon applies it on session new/restart, on startup, on reset, and
  after an import (`tmux.reload`).
- The agent pane is pinned as `@rlord_pane`, because a user's
  `split-window -b` lists the new pane first.

`rlord reset` restarts the daemon (via launchd or systemd only when the
service manager's pid is ours), then `admin.reset` closes overlays, reconciles, re-sources the config,
rebinds keys, and respawns the list's pane. `--hard` first kills the server.
It also removes legacy `rlord`/`rlord_*` sessions and `rlord nav` bindings
from the user's default server, but only for the real socket.

## The overlord (built 2026-11-10)

The product's primary control surface: an agent session
(`role = 'overlord'`, fixed tmux name `overlord-rlord`) that the user talks to,
wired to rlord's MCP server. There can be one per agent, with exactly one
active ("One per agent, one active", below); what follows holds for each.
A strong model at medium effort
(`[overlord] model` / `effort`; the effort is a launch flag, per agent:
`overlord::effort_args`): it does light work itself and sends the rest to
subagents and new sessions, which `overlord.md` states first. It lives in
`<data>/overlord/` (an overlord that began as the "mastermind" of the
former two-session design keeps `<data>/mastermind/`, so its conversation
resumes: the folder is the session's `project_path`, never assumed). Each
`ensure`
regenerates that folder's `CLAUDE.md`, `AGENTS.md` and `GEMINI.md` (the same
standing instructions under each agent's file name: the built-in
`daemon/src/overlord.md` plus the user's `overlord.md`), each agent's MCP
config and each agent's subagent files (`overlord::write_files`; every
agent's are written, whichever runs it). These are project files, so each
agent still loads the user's own global instructions too. Its adapter flags
ride in `launch_spec.extra_args`, so resume reuses them; they are rewritten
on every `ensure` and fresh restart, so a config change reaches it at its
next start.

Any agent can run it (`write_files` matches on `Agent` with no wildcard
arm, so a new agent doesn't build until it is wired). The surface is the same for
each, in its own terms: rlord's tools without a prompt, reading without a
prompt, a prompt for anything that writes or runs a command.

| | MCP server and its tools | Everything else | Subagents |
|---|---|---|---|
| Claude | `--mcp-config <folder>/mcp.json`, `--allowedTools mcp__rlord,Read,Grep,Glob` (with `confirm_sends`: rlord's tools listed without `send_to_session` and `resume_after_limit`) | asks | `.claude/agents/*.md` |
| Codex | `-c mcp_servers.rlord.{command,args,env_vars}`, `default_tools_approval_mode="approve"` (`codex::overlord_args`) | `-c sandbox_mode="read-only"`, `-c approval_policy="on-request"`, `-c check_for_update_on_startup=false` | `.codex/agents/*.toml` |
| OpenCode | `<folder>/opencode.json`: `mcp.servers.rlord` (`opencode::overlord_config`); no flags | its `permissions`: `*` asks, then `read`/`glob`/`grep`, rlord's own subagents by name, `rlord_*` allow (last match wins); repeated under `agents.<name>`, except rlord's subagents: `*` asks, then only their own rlord tools | `.opencode/agents/*.md` |

Things that are not visible at the call site:

- **`rlord mcp --overlord --socket <path>`**: every agent's config names the daemon's
  socket as the daemon resolved it, and `rlord mcp` then never autostarts
  a daemon. An agent may start the server with another environment
  (OpenCode's shared server keeps whatever environment first started it,
  where the XDG dirs could name another socket). What `rlord mcp` still
  reads from its environment is one list, `agents::MCP_ENV` (the calling
  session's id, only logged): Codex, which starts a server with a short
  environment of its own, passes it on in `env_vars`; OpenCode blanks it
  in `environment`.
- **Codex's approvals.** `approval_policy="on-request"`: commands run in
  the read-only sandbox unasked, and anything needing more asks.
  `"untrusted"` (ask for anything not known to be read-only) would be
  stricter, but Codex 0.167 refuses it ("no longer supported"). The
  sandbox keeps a shell command from reaching rlord's tmux socket: the
  connection is refused on macOS, and on Linux too where Codex's sandbox
  can start (see the launch checklist for what has been checked). Where it
  can't start (e.g. stock Ubuntu 24.04 for a non-root user, with
  `kernel.apparmor_restrict_unprivileged_userns=1`), commands fail in the
  sandbox and, under `on-request`, Codex asks to run them without it:
  approving that lets a command reach the socket. It doesn't
  stop the overlord typing into sessions: `send_to_session` does that, on
  every agent, without a prompt (below).
- **`send_to_session` runs unasked, by design.** rlord expects to be run
  by its user on a machine they trust, or over a remote connection they
  trust. Little untrusted text reaches the overlord: it works from rlord's
  own data and the user's requests, not arbitrary content. So it can type
  into sessions without asking. That is what lets it orchestrate. Prompt
  injection would take a setup very different from the one rlord is built
  for. `[overlord] confirm_sends = true` makes each send (and `resume_after_limit`) ask, for anyone
  who wants that (`write_files`): Claude lists rlord's tools one by one in
  `--allowedTools` without them (`claude_allowed_tools`); Codex adds
  `mcp_servers.rlord.tools.<tool>.approval_mode="prompt"` for each
  (checked live on 0.167.2: Codex asks "Allow the rlord MCP server to run
  tool send_to_session?"); OpenCode adds an ask rule for
  `rlord_send_to_session` and `rlord_resume_after_limit` after `rlord_*`.
  Both tools are `agents::SEND_TOOLS`.
- **Codex's folder trust is passed as a whole table**
  (`-c 'projects={"<dir>"={trust_level="trusted"}}'`): a dotted override
  path can't hold a quoted key, and without trust Codex stops at a "trust
  this folder?" screen and ignores `.codex/agents`. It replaces the user's
  whole `projects` table for that run only (their other folders' trust is
  unknown to it); nothing is written under `~/.codex`.
- **OpenCode's MCP server process belongs to its shared server**, not the
  pane (hence `--socket`). OpenCode calls MCP tools from a Code Mode script
  (`execute`), which needs no rule of its own: OpenCode checks each tool a
  script calls (an rlord tool runs unasked; `fetch` asks as a web fetch),
  and a script can't import `fs` or `child_process` (checked live,
  OpenCode 2.2.7).
- **OpenCode merges rules by level, not by file**: the top-level rules of
  every config file (the user's global ones first, the folder's after),
  then every file's agent-level rules (`agents.<name>.permissions`). A
  user's own global agent-level `allow` would otherwise come last and
  override the overlord's `*` ask, so the folder config repeats its rules
  under every agent a conversation there can run as: `build`, `plan`,
  `general`, `explore`, rlord's subagents, and the user's own global
  agents (`opencode::user_agents`: only the names, from the `agent` /
  `agents` keys of OpenCode's global config files and the Markdown agents
  in its `agent(s)/` folders, read when the folder is written). An agent
  the user adds after that is covered from the overlord's next start.
- **Codex reports nothing until a prompt.** It creates the conversation,
  and runs SessionStart, with the first prompt of a new conversation and
  the next prompt of a resumed one. `ask` types into the overlord only once
  it has left `starting`, so `daemon::discover` settles such a session once
  its pane has been up a moment and shows Codex's composer
  (`codex::composer_shown`: the lowest `›` line of the bottom input area,
  not a numbered dialog choice such as an update or trust question;
  `first_prompt_due`, and `follow` for a session still starting). After
  20s it settles on any screen without a numbered dialog
  (`codex::dialog_shown`), logged, so a composer drawn differently can't
  leave it starting for ever. While
  starting, only transcript lines written since the (re)start count
  (`transcript::working_since`): a conversation killed mid-turn ends on a
  turn that never closes, and resumed it is the user's turn. An overlord
  that stopped with no conversation bound looks in Codex's index once more
  (`discover::bind_now`) and resumes what it finds; only with nothing
  there does it start fresh.
- Subagent definitions are kept once, in Claude's format (`tidy.md`,
  `import.md`), and converted (`overlord::subagent_parts`). A Codex or
  OpenCode subagent inherits the overlord's tools. Claude's file narrows
  them (`tools:`); OpenCode's are narrowed in the folder's
  `opencode.json` (`agents.<name>.permissions`: everything asks, then the
  rlord tools from the `tools:` line are allowed); a Codex one can't be. A plain copy goes to `<folder>/subagents/` for an overlord
  that can't start one.

### One per agent, one active

Each agent (Claude Code, Codex, OpenCode) can have its own overlord. One is
**active** and gets everything addressed to "the overlord": the overlay,
`overlord.ask` and the `n` form, `ui.goto` on its row, Remote Control's
`overlord: true`. The others keep running with their own conversation and
are sent nothing. Only the user switches (`overlord::switch`, RPC
`overlord.switch {agent}`): the first switch to an agent creates its
overlord with that agent's defaults (`model_effort_for`), later ones find
it as it was (started again if it had stopped). Nothing switches by itself,
including when the active one hits a usage limit.

- **Out of usage: offered, not done.** When the active overlord's row
  carries a `limit` (the same transcript detection as any session,
  `limits::annotate`), `overlord.get` adds `limit.switch_to` (installed
  agents other than the active one whose overlord, if any, isn't limited
  too) and `limit.offer` (`overlord::limit_offer`: plain data over the
  roster, no tmux of its own; a limit without a reset time is not offered). The list opens a dialog on a refresh that
  sees `offer` (keys in its first 800ms are dropped; `y` switches through `overlord.switch` and, like esc, records the limit as answered; esc calls
  `overlord.answer_limit_offer`, which stores that limit's `resets_at`
  in kv `overlord_limit_offer_answered`, so the same limit isn't asked
  again but the next one is). Not offered once the limit has reset, or
  with nowhere to go. The overlay's top row says "out of usage until …"
  as before.

- **In the DB** every overlord has role `overlord`, so `is_overlord_role`
  and every refusal built on it (rename, tags, notes, archive, stop from a
  tool, `send_to_session`) hold for all of them without knowing which is
  active. The `agent` column tells them apart (one per agent:
  `overlord::for_agent`). kv `overlord_active` is the active one's id;
  with no kv, or one naming a row that is gone, it is the oldest
  (`Db::overlord`), which is how an install from before this has its one
  overlord active with no migration. All are named `overlord`;
  `resolve_ref("overlord")` means the active one.
- **`Db::overlord()` / `overlord::session()` are the active one.**
  `overlord::all()` is every one. Code that keeps overlords healthy
  iterates all (the idle fresh-restart keeper, the Remote Control keeper,
  `style`, tmux-name and profile sync via `include_overlord`); code that
  talks to the overlord uses the active one. kv `overlord_fresh_at:<id>`
  is per overlord (`overlord::fresh_at` falls back to the old unsuffixed
  key).
- **Folders**: one each. The first overlord has `<data>/overlord/` (or
  `mastermind/`, above); one added for another agent gets
  `<data>/overlord-<agent>/` (`dir_for`). `write_files` writes the same
  files whichever agent runs the folder, so sharing would be safe for the
  files, but every agent finds its conversations by folder (Claude lists
  and resumes per folder, Codex binds a thread by where it started:
  `discover::bind_now`), and separate folders keep each overlord's
  conversations its own. `overlord::dirs()` lists them all, for search
  and import to skip.
- **tmux**: the first is `overlord-rlord`, later ones
  `overlord-rlord-<agent>` (`free_tmux_name`; `is_tmux_name` recognizes
  both, and both are reserved names). A swap renames nothing, so pane
  ids, attached clients and a paste in flight stay valid.
- **Locks**: `Daemon::overlord_send` is still one lock for all overlords.
  `ask::send` reads which is active under it and `overlord.switch` takes
  it, so a request can't be pasted into the overlord the user just
  switched away from. Each overlord's own session lock is as for any
  session (locks.rs).
- **The switchers** all call `overlord.switch`: the overlay (it draws a
  top row (always there, ending in a ✕ close button: tmux swallows
  clicks outside a popup, so it is the mouse way out) with each installed
  agent's marker when there are two or more;
  a click, or `[tmux] overlord_switch`, default `M-s`, read by the
  overlay and never bound in tmux. The key goes round the overlords that
  exist and never creates one; a click on an agent without one, the
  list's menu, `:overlord <agent>` or the CLI does. If a tmux binding
  uses the same key (modifiers compared case-insensitively, as tmux
  does), the switch key is off with a warning. The overlay then shows the other
  pane without closing: `overlay::show`), the overlord row's right-click
  menu, `:overlord <agent>`, and `rlord overlord switch <agent>`.
  `overlord.get` and `overlord.status` carry `overlords`
  (`overlord::roster`: per agent installed / exists / active / running),
  which is what they draw from. `roster` runs tmux and the install
  checks, so it is built after the DB guard drops and cached for a few
  seconds while its rows are unchanged (`Daemon::roster`): the TUI asks
  for `overlord.get` on every refresh. The popup's title is set by tmux when it
  opens and can't change, so with several agents the "fresh 2h ago"
  label moves from the title to the overlay's own row.
- **`recall` reads every overlord's conversations**: all their earlier
  ones plus the other overlords' current ones (`recall::pasts`). The user
  thinks of one overlord desk and switches agents by hand, often because
  one ran out of usage, so "as I said earlier" may have been said to
  another one. The list's "fresh · N earlier" and the earlier
  conversations in the details pane stay the active overlord's own.
  `overlord.continue` given another overlord's conversation (an id from
  `recall`, whose hits name their overlord) makes that overlord active
  without starting it (`overlord::activate`), then continues it there,
  so it starts once, onto that conversation.

- **Asking** (`daemon/src/ask.rs`): `overlord.ask` pastes text into the
  overlord's agent pane (`send::type_into`: bracketed paste, then Enter),
  waiting out `starting` after a (re)start, and refusing while it needs
  the user or its screen shows a dialog (the Enter would answer it).
- **rlord's own prompts** (`daemon/src/own.rs`). The agent reports what
  rlord pastes like any prompt (its UserPromptSubmit hook, OpenCode's
  events, a turn `discover` reads from a transcript), and rlord's own
  messages must not count as the user typing: `last_user_msg_at` feeds
  the "last prompt" sort and resume jobs. So for `send_to_session`, a
  `when_idle` delivery and a resume, `type_into` leaves a marker right
  before the paste, in kv `own_prompts` (`{session id: [{at, source,
  hash, chars}]}`; `source` is `send` or `resume`; a daemon restart keeps
  it). `overlord ask` is not marked: its text is what the user typed, so
  its prompt is the user's. Every reported prompt goes through
  `own::prompt`:
  - **With its text**: Claude and Codex hooks (`prompt`), and OpenCode's
    `session.inbox.enqueued` (`item.payload.text`, per the 2.2.7 schema).
    Only what identifies the text is kept (`PromptMark`: a
    whitespace-insensitive hash, and whether it looks changed by a paste);
    `rlord hook emit` sends `prompt_hash`/`prompt_pasted`, never the
    prompt. A marker with the same hash, up to 10 minutes old, is that
    prompt. Any other text is the user's, however soon after a paste.
  - **A text the agent may have changed.** Agents show a large or
    multi-line paste as a placeholder in their input: Claude Code
    `[Pasted text #1 +30 lines]`, OpenCode `[Pasted ~30 lines]`, Codex
    `[Pasted Content 1234 chars]` (forms read from the 2.2.7 and 0.167
    binaries). Claude's transcripts hold the expanded text (checked
    against local input history), so its hook's `prompt` should too, but
    no hook payload of a large paste was observed. As a guard, a prompt
    with another hash within 5s of the paste is still taken as it when
    rlord's paste was 1000+ characters, or had several lines and the
    reported text holds a placeholder. Never for a short one-line paste
    such as "continue": a placeholder after that is the user pasting.
  - **With no text** (OpenCode's session row read after missed events, a
    turn found in a transcript, a cut-off hook payload): the first prompt
    from 1s before a marker to 30s after it. A user prompt that beats
    rlord's own into that window is read as rlord's; only these
    text-less paths have that gap.

  A matched prompt is logged as `rlord_prompt` (with `source`) and moves
  `last_rlord_msg_at`; any other is `prompt_submitted` and moves
  `last_user_msg_at`. A marker is used once per way of reporting: the
  same prompt told a second way within 5s of the first report (a turn
  read from the transcript, then its hook) is still rlord's; anything
  later, or told the same way, is the user's. It is taken back when its
  paste fails, and expires after 10 minutes. A paste whose marker can't
  be written is not made (held or refused, like any "not now"). "Updated" everywhere (the
  activity sort, the list's last-message time, `triage`) counts all
  three message times; code that means "any prompt" (the overlord's idle
  fresh start, `recall`'s dating, the quiet check before a
  remote-control restart) reads `Session::last_prompt_at`.
- **Typing into a pane** (`send::type_into`), for asks, sends and
  deliveries alike: right before the paste, under the session's lock, the
  pinned agent pane is captured and must be at its input prompt
  (`Agent::at_prompt`: Codex's composer; no Claude `❯ 1.` choice as the
  lowest `❯` line, where a `❯` under a `─` rule is the input box and
  higher ones are history; no OpenCode permission or question lines inside
  its bottom box). A status can't be trusted for this: a Codex session
  without hooks reads as working while an approval is up. (A daemon-mode
  Codex thread with a turn started from another client reads as at its
  prompt: see "Another client on the same thread" under "Codex on its
  shared background server".) Sends also hold
  off while the input holds a draft (read with attributes, so a dim
  placeholder isn't one: Claude's input box, Codex's composer, OpenCode's
  prompt box, whose placeholder is in the muted colour of the `·` on its
  agent line), within 10s (`SETTLE`) of the last paste into that
  session (checked under the paste guard, so two can't race), and while a
  tmux client attached to it was used in the last 5s (someone typing
  there; not the client the overlord's overlay is open on, where the user
  is typing to the overlord). Any of these is "not now": a send queues
  (`when_idle`) or is refused, a delivery waits for a later tick and logs
  `message_held` with the reason once per queued message.
- **Per-session lock** (`daemon/src/locks.rs`): taken by sends,
  deliveries and asks around the final re-read and the paste, and by
  restart, stop, archive, fresh restarts and `overlord.continue` (and the
  overlord's `ensure` / `set_model`). Order: `overlord_send`, then the
  session's lock, then the DB; never the session's lock while holding the
  DB. `TYPING` comes last.
- **Passing work to a session** (`daemon/src/send.rs`, MCP
  `send_to_session`, method `session.send`): the same paste into the
  target's pinned agent pane, for any agent. Only a session whose turn is
  over; working or needs-you is refused unless `when_idle`, which keeps one
  pending message per session (kv `send_pending`, a newer one replaces it,
  and a direct send drops it, logged as replaced) for a 2s tick to deliver
  once the turn has been over 2s, re-read under the session's lock. Stopped, starting, error, archived and the
  overlord itself are refused; a pending message is dropped when its
  session stops or goes. Logged as `message_sent` / `message_queued` /
  `message_held` / `message_delivered` / `message_dropped`. `overlord.new_session` formats the list's `n` form as a
  `[rlord new-session request]` message: `confirmed:`, the hints with
  whether each was picked or defaulted, then the description verbatim;
  `overlord.md` defines what the overlord does with one. On a contradiction
  it calls `ui.confirm_new_session`, which reopens the form (TUI
  `confirm_new_session` command) with corrected hints marked; sending it
  back says `confirmed: yes`. This describe → propose → confirm loop is the
  pattern for config-style input (CLAUDE.md). `rlord overlord ask` is the CLI.
  The overlord launches with the description as `prompt` and its own
  findings as `context` and `related`, never inside the prompt ("Session
  background"); `overlord.md` says so, and a test checks it no longer
  asks for one combined prompt.
- **Fresh restarts** (`overlord::start_fresh_keeper`): once a minute, an
  overlord idle past `[overlord] fresh_after_mins` is restarted on a fresh
  conversation (`session_fresh`), so its context doesn't grow without end.
  Only between turns, when its transcript ends in a final reply, that reply
  didn't ask the user anything (a `?`, or a form out for confirming), and
  nobody is looking at it (no overlay, no attached client). `ask::send` and
  the keeper share a lock (`Daemon::overlord_send`): text is never pasted
  into a pane that is being replaced.
- **Its past** (`daemon/src/recall.rs`): every move to another conversation
  is an event on the overlord's session, and those events are the one
  source for what its earlier conversations are. `fresh_conversation`
  carries a `reason`: `idle` (the keeper), `clear` (the user's `/clear`,
  from Claude's SessionStart `source: "clear"`, recorded in `hook_emit`),
  `manual`, `continued`; any other switch is `agent_session_changed`. Two
  readers use it and so agree: the overlord's `recall` tool (search its
  earlier transcripts, or a clipped digest of the previous one; search
  proper skips the overlord's folders) and the list ("fresh 2h ago · N
  earlier" on its row and overlay title, the earlier conversations in its
  details pane, one in the preview, `overlord.continue` to go back to one).
  Nothing is preloaded into a fresh conversation; it fetches on demand.
  kv `overlord_fresh_at:<id>` is when the current one started.
- **Triage** (`daemon/src/triage.rs`): the cheap first pass for the
  overlord's upkeep. `overlord.triage` lists live sessions updated since
  their per-session `reviewed:<id>` kv watermark, with missing-metadata flags
  and transcript excerpts (`claude::transcript_excerpts`, which reads only
  the head and tail of the file). Overlord edits through `update_session` /
  `organize`, and `overlord.mark_reviewed`, advance the watermark.
  The overlord delegates tidy passes to the `rlord-tidy` subagent
  (`daemon/src/tidy.md`, written to the overlord's folder in each agent's
  subagent format), which runs in the background with its own context (and,
  on Claude, a narrow tool list).
- **Import** (`core/src/import.rs`, `daemon/src/import.rs`): finds agent
  conversations rlord doesn't track, from the agents' own transcripts (Claude:
  `~/.claude/projects/<cwd slug>/<uuid>.jsonl`), with no AI and nothing
  specific to the tool that started them. `session.import_scan` pre-filters
  by file mtime, then measures activity by the last user/assistant message
  timestamp. The cwd must slugify to the transcript's folder, or
  `claude --resume` couldn't find it. It skips rlord's own folders, temp dirs,
  headless (`sdk-*`) runs, and conversations with no typed prompt.
  `session.import` adds a stopped row with trust `hook` (the id is the
  transcript's file name, so restarts resume it) and no tmux name until it
  first runs. It can recreate a removed worktree from its branch, or import
  as archived. A conversation id on a running process's command line, or a
  transcript written in the last 10 minutes, is flagged "maybe open
  elsewhere". The overlord's `rlord-import` subagent (`daemon/src/import.md`)
  proposes what to import and imports what the user approves.
- **MCP**: `rlord mcp` is a hand-rolled stdio JSON-RPC shim that speaks the
  protocol and forwards `tools/list` / `tools/call` to the daemon methods
  `mcp.tools` / `mcp.call`, one request at a time, in order (calls that
  change things and then read them back rely on it). Tools are defined in
  `daemon/src/mcp.rs` as thin, param-whitelisted mappings onto daemon
  methods. Since one slow call holds up the rest, the repo scan, which can
  block indefinitely on a macOS privacy prompt or a dead mount, never runs
  under `dispatch`'s DB lock nor makes a caller wait long. It runs on its
  own thread (`daemon/src/repo_scan.rs`), only when a look-up asks (so a
  missed privacy prompt never pops up unprompted); `repo.find` gets its
  last result, with `scanning: true`
  while roots never scanned have no result yet (waited on for a scan's
  first 2s). One scan per set of roots at a time, and a stuck one is not
  retried (the same folder would block the retry). The scan also predicts
  what would stall it and stays out (`git::scan_repos`,
  `core/src/privacy.rs`): by name, before any `stat`, the folders macOS
  guards with a privacy prompt (`$HOME/{Desktop,Documents,Downloads,Movies,
  Music,Pictures,Library}`, `/Volumes/*`; no public API reports TCC status
  without risking the prompt); on every platform, a folder on another
  filesystem than its root (`st_dev`, like `find -xdev`); and no symlink is
  followed. Only the roots themselves (the code folder and each
  `repo_roots` entry, all named by the user) are entered wherever they
  are: a root inside a protected folder or below a mount is walked as its
  own root, and a root nested in another, or named twice, is walked once.
  A folder where a session runs grants nothing: a macOS privacy grant
  belongs to the app that made the access, so a session under a terminal
  or an editor proves nothing about rlord's daemon (that session's repo
  still shows, from the DB, as a known repo). A folder whose metadata
  can't be read is skipped silently.
  Skipped folders come back in `repo.find` (`skipped`, at most 50) and the
  finder and `find_repos` name them.
- **UI control** (`daemon/src/ui.rs`): terminal navigation is done by the
  daemon with tmux. It switches the user's client (the most recently
  active one). TUI state is pushed
  as typed `{"notify":"ui","cmd":...}` lines on the subscription stream
  (`set_view`, `new_session` with a prefill). The TUI reports its current view
  back (`ui.view.report`) so the overlord can see it. A command sent while no
  TUI is subscribed is parked (30s) for the next TUI. If there is no TUI, the
  daemon starts one in a `rlord` tmux session ("home").
- **Overlay**: Option-O (a root-table binding that runs `rlord nav overlord`)
  asks the daemon (`ui.overlord`, passing the pressing client) to open a
  `display-popup` on that client running `rlord overlay`. That program is a
  small renderer: it sizes the overlord's tmux window to the popup, captures
  pane and cursor every ~40ms, and forwards keys (`send-keys`) and pastes
  (`paste-buffer -p`). It doesn't use a nested `tmux attach`, which fails
  inside popups. Inside a popup, keys reach the program rather than tmux's key
  table, so the program itself treats Option-O (Alt-O) as "close". Navigation tools
  close popups (`display-popup -C`) before moving the user.
- **tmux locale**: every tmux invocation forces a UTF-8 `LC_CTYPE` when the
  environment has none (`en_US.UTF-8` on macOS, `C.UTF-8` on Linux). Under
  launchd there is no `LANG`, and tmux then rewrites tabs and non-ASCII characters in command output to `_`.
- **Default view** persists in the `kv` table (schema v5).
- **Model freshness** (`core/src/models.rs`): a hand-maintained per-tool table
  of the newest model per family, checked against the configured model (alias
  or pinned id) and the observed model (last assistant turn in the
  transcript). The observed check catches an alias that resolves to an old
  model because the agent CLI itself is stale.

## Navigation between rlord and agent panes

The TUI is the hub; agent sessions are spokes. Moving around never requires
remembering tmux session names:

- **Enter a session** (`attach` key in the TUI): inside tmux this is
  `switch-client`, so the TUI keeps running in its own pane; outside tmux the
  TUI suspends and runs `tmux attach` as a child until you come back.
- **Back to rlord** — a *server-wide root-table tmux binding* (default `M-d`, Option-D on a Mac)
  that runs `rlord nav home`. The TUI records its own tmux session in the
  global option `@rlord_home` whenever you enter a session; `nav home`
  `switch-client`s there, or `detach-client`s when there is no home (the
  outside-tmux case). Verified 2026-09-11 with a scripted PTY against a live tmux 3.6
  server: a root binding + `run-shell` moves the pressing client.
- **Jumping** is done from the TUI, not by cycling: `/` opens a search box
  (rich syntax below) that filters as you type; `Enter` attaches to the top
  match, `Tab` keeps the filter and returns to the list, digits `1`–`9`
  attach to the Nth listed session. Cycling bindings (`rlord nav next|prev`)
  exist but are **off by default** — a fleet has no meaningful linear order,
  so prev/next tends to disorient rather than help.
- **Search syntax** (TUI `/` box and `rlord ls <query>`): free words match
  name, project, group, or tag (every word must match); tokens
  `status:waiting,idle` `tag:wip` `proj:wait2` `agent:claude` `group:work`
  `sort:user` set the typed filters. `o` cycles sorts, `O` reverses.

The bindings are installed by the daemon at startup and whenever it creates a
tmux session (idempotent), and by the TUI at launch; `rlord tmux unbind`
removes them. They live in the root table because tmux key tables are
per-client, not per-session — the cost is that these keys are global on your
tmux server, so each is configurable and can be set to `""` to disable.

## Configuration

`$XDG_CONFIG_HOME/robot-overlord/config.toml` (`rlord config init` writes a
commented template equal to the defaults; `rlord config show` prints the
effective config). Missing keys take defaults; a malformed file is an error,
never silently ignored. Sections: `layout` (`side` = list left / preview
right, `stack`), `list_width_pct`, `preview_refresh_ms`, `usage` (what the
Usage column shows), `[keys]` (every TUI action), `[tmux]` (the three
navigation bindings).

### Setup: the code folder

`[session] code_dir` is where the user keeps repos. The finder always
scans it, plus each `repo_roots` entry, a folder named twice once
(`SessionDefaults::scan_roots`). With neither set, the finder scans
`~` until setup is answered, then nothing (`finder_roots`). Setup is
answered when a kv row `setup_code_dir` exists (`done:` or `skipped:`): a
skip ("don't ask again" on the form) never writes the config; Esc on the
form records nothing, so it opens again next start.

The suggestion is plain code (`core/src/code_dir.rs`) over folders rlord
can read without walking the disk (`daemon/src/setup.rs`, bounded): the
newest Claude Code project dirs (a `cwd` whose slug matches, from the
first of a few transcripts that has one), Codex's `threads.cwd`, OpenCode's top-level
`session_v2.directory` (only that table: the DB holds credentials), rlord's
repos and session folders, and the launch folder. The vote is string work
only; then a candidate about to be shown that is a git repo is left out
(`.git` is looked for only under `$HOME` and where no macOS prompt
guards, so no stat reaches `/net` or `/mnt`). Two subfolders of one repo
still count as two projects (a known limit). Each project counts
once; the folder holding the most within `repo_scan_depth` wins (ties: the
deeper), never `$HOME` or above (as given or resolved), nor a system or
mount parent (`/Volumes`, `/mnt`, `/run/media/<user>`, `/var`, …:
`code_dir::NEVER`). A single explicit root outranks it when it
could be suggested itself (not `$HOME` in any spelling, a system parent
or a repo). Only the folders shown are `stat`-ed, and only when no
privacy prompt guards them.

It follows describe, then confirm (`tui/src/setupform.rs`), with one
difference from the new-session form: the confirm saves by plain code
(`setup.apply`, through `Config::set_code_dir`, which keeps the file as
written). The overlord only proposes (`confirm_setup` reopens the form);
it isn't called again. First run opens the form after the overlord
dialog (or the model-upgrade or Remote Control dialog), only during that
startup sequence, never when a dialog closes mid-session; `:setup`,
`rlord setup` and the overlord open it later. The form
opens at once with the configured roots (`setup.status`, cheap) and fills
in the suggestion from another thread; it asks before creating a folder
that doesn't exist. A folder macOS guards is saved unchecked
(`checked: false`): checking or creating it would raise the prompt.
`setup.apply` refuses what the suggestion never offers (a system or
mount parent, a git repo where it can check), and takes `$HOME` itself
only with `allow_home` (the form and the CLI warn and ask twice;
`rlord setup --allow-home` without a terminal or with `--json`). The
form looks at the disk from the UI thread only under `$HOME`; anything
else is "not checked here" and left to `setup.apply`.
What saving rlord's own suggestion does with `repo_roots` is the
daemon's call (`setup::default_roots`, returned as `roots_default` and
used as given by the TUI and the CLI): an old `["~"]` is dropped only
until a folder has been saved once. An overlord proposal without
`repo_roots` always leaves them. `confirm_setup` never reads the agents'
files; the form loads the suggestion itself.

## Session creation

Creation is the most-used flow, so it has its own defaults (`[session]` in
config: agent, model + picker list, workspace on/off, `workspace_dir`,
`default_repo`) and its own state: the `repos` table remembers every git repo
a session was created in (last used, use count) so the picker can order
**default first, then most-recently-used**.

A session's location is one of: a **repo** (default) — resolved to the main
repository root even from inside a worktree, so all workspaces nest under one
`<repo>/.worktrees/`; or a **plain folder** (no repo, no workspace; the folder
picker with fuzzy search is a later addition). With `workspace = true` the
daemon creates `git worktree add -b <branch> <repo>/.worktrees/<dirname>`
(existing branch → checkout; dirname = branch with `/`→`-`), excludes the dir via `.git/info/exclude`, and launches the agent
*inside the worktree*. `sessions` records `repo_path`, `workspace_path`,
`branch`; the model is kept in `launch_spec` so restarts resume with the same
`--model`.

A **new branch starts from the latest default branch**, fetched.
`Daemon::call` works the start out before the DB lock
(`repo_latest::set_launch_base`, the same fetch and pick rule as
`repo_latest`, with its wait capped) and puts it in
`params["base_resolved"]`, always overwriting what a caller sent. The
location it worked out (`methods::new_location`) goes along as
`params["location_resolved"]` (also never a caller's), so `session_new`
doesn't work it out again; it carries the branch name too
(`new_location` has the one rule for it). `session.new`'s cheap checks
(`methods::precheck_new`: the background's limits, the agent installed,
Remote Control possible) run before any of this, so a launch that will
be refused waits on no fetch; `session_new` runs the same function, the
one set of checks. The manual form (`N`) sends `session.new` from a
thread of its own, one launch at a time, and reopens with the error if
it fails. Editing its Base clears a `prefer` that came with a proposal. The
worktree is then `git worktree add --no-track -b <branch> <dir> <commit>`
(`--no-track`: a private ref never becomes the upstream). An existing
branch is checked out as it is. `[session] base = "head"` starts it at the main
checkout's `HEAD`, with no fetch; `create_session`'s `base` names a branch (or
`<remote>/<branch>`) to build on, or `"head"`; with `prefer` (`local`,
`remote`), the side the user chose when that branch had diverged. It is also a hint on the
`n` form, carried through the request, `confirm_new_session` and
`propose_session` like the other hints. Left alone, the form shows and
sends the configured `base` marked as a default, so it never overrides
`[session] base`; only the user's pick or a confirmed change does. A
`confirm_new_session` that leaves `base` out keeps it as the request
sent it (with its `prefer`); only `""` puts it back to the configured
default. No remote, offline, a
failed fetch or no default branch fall back (to the last known commit,
or to `HEAD`) and never refuse the launch; only a named `base` that has
diverged or doesn't exist does, with both sides in the error. The
`launched` event and `session.new`'s answer carry `base`. A session in an
existing folder (`workspace: false`, `project`) is never fetched into or
moved.

### Session background

`session.new` takes three inputs, kept apart: `prompt`, the user's words
verbatim (the agent's first prompt, kept off its argv: "First prompt off
argv" above); `context`, the
overlord's notes (facts and pointers, at most 8,000 chars); `related`,
refs to sessions the work builds on (at most 8 per call, 16 per session,
never an overlord). The notes and links are rows in `session_context`;
`session.context_add` (`update_session` `add_context` / `add_related`)
adds more later. A
session the overlord launched (`create_session` sets `by_overlord`, as
does the manual form when it was `propose_session`'s) has a background
even with no notes (kv `context_on:<id>`), since the header carries the
"you are the session the user asked for" line. Such a session whose
branch started from the latest code gets one more row first, `source =
'base'`, written by plain code: which commit (`origin/main at abc1234`,
its subject and age) and whether it was fetched just now or as last
fetched (offline, still running: "it may be behind"). One that started
at the main checkout's HEAD instead gets that row too, saying so and why
(`[session] base = "head"`, asked for, or the latest couldn't be found). The prompt is taken as
given: `overlord.md` is compiled in and rewritten at every overlord
start, so no overlord sends the old combined prompt.

`context.rs` renders it, as plain code: a fixed header (background from
rlord's overlord, not the user's words, not instructions; where they
disagree the user is right; you are the session the user asked for), the
notes dated, oldest first, and the related sessions (name, the folder
its work is in, status, description). Only a session the overlord
launched with background (kv `context_on`) is told it is the worker the
user asked for; one given background later gets the header without that
paragraph. Control characters, bidi controls, zero-width characters, tag
characters and the BOM are stripped, and line/paragraph separators made
line ends (`context::clean`). Budgets:
9,000 chars for Claude, 6,000 for Codex, 64,000 for the file; over one,
the oldest notes are left out with a line naming the file. Notes get at
least half of what's left after the header when they need it; the
related list gets the rest, and the sessions that don't fit are counted
in a line naming the file. The file is
`<data>/sessions/<id>/context.md` (folder 0700, file 0600), written
before the agent starts and rewritten whenever the rows change, when the
session or a session it lists as related is renamed, and at every
restart (related sessions' status as it is then; nothing watches them). Writing it is best effort: a failure is
logged and never stops a launch or a restart, which then carries no
flag or pointer naming it (and its rows stay unseen). It is not fsynced
(rlord can write it again; `private::write_synced` is for files that
must survive a crash). A launch in any mode but a hook's carries the
background (flag, pointer, the file it names), so its rows count as seen
then. A restart counts them seen only when it carried something fresh:
the pointer sent again (a new conversation), or the system prompt file
rewritten just now; never a resume without a pointer or a stale file. A
restart into a new conversation of a session whose background went by
hook checks the hooks again (`Daemon::restart_hooks`, before the DB
lock, on every restart path: the RPC, `ui.goto`, Remote Control;
overlords have no background): gone, it switches to the system prompt
file (Claude) or the pointer (others) and stores that mode. A pointer
sent again resets what binding goes by (`bind_since`, `bind_prompt`) to
that start and that prompt.

How it reaches the agent (`context::delivery`, decided at launch and
kept in kv `context_mode:<id>`):

| Agent | How |
|---|---|
| Claude, rlord's SessionStart and UserPromptSubmit hooks installed | the hook's answer (below) |
| Claude without it | `--append-system-prompt-file <context.md>`, at every start (`context::launch_args`, for launch and restart) |
| Codex, those hooks installed and trusted | the hook's answer |
| Codex otherwise, OpenCode | one fixed line after the user's words naming the file (`context::pointer_line`); with no words, a fixed first prompt naming the file and asking it to read it and wait for the user (`context::pointer_only`). OpenCode reads it unasked: its `external_directory` permission (paths outside the project) defaults to allow |

Whether the hooks work (`context::hooks_work`: SessionStart and
UserPromptSubmit installed, and for Codex trusted) reads the agent's
settings files, so the daemon asks it before taking the DB lock and
passes it to `session.new` and `session.restart` as `hooks_work`
(`Daemon::restart_hooks`), and, for a session's first background after
launch, to `session.context_add` (`Daemon::first_background_hooks`).
`--append-system-prompt-file` works in an interactive Claude session
(checked live on Claude Code 2.1.325).

The hook's answer: `hook.emit` returns `reply` (the hook's JSON output,
`hookSpecificOutput.additionalContext`) and `reply_ack`, only for a live
event (never one replayed from the spool: `methods::hook_replay`), after
the routing and own-pane checks, and only for the session's own
conversation (not a sub-agent's thread). `SessionStart` gets the whole
background on a start, a clear, a compact (the context was lost) and in
any conversation it hasn't reached; a `resume` of one it has reached
gets only what is new. A conversation with no id yet (no `session_id`,
nothing bound) is recorded as such, and the first id seen after it is
taken as the same one, so it never gets the whole of it twice. An
update with nothing to show (only links to sessions that are gone) sends
nothing and marks those rows seen. A Pointer-mode session that restarts
into a new conversation (OpenCode's was never created, or is gone) gets
the pointer again as its first prompt (`context::restart_prompt`).
`UserPromptSubmit` gets only rows not yet seen, under "rlord background,
updated" (so a note added later reaches the agent with its next prompt),
or the whole background if none went to this conversation yet. No
background: no answer, as before. A session launched with its background
another way has its launch rows marked seen then, since they went out
with the launch (the system prompt flag, the pointer line): a hook that
fires anyway hands over only notes added later, at
`UserPromptSubmit`. The answer is best effort: an error building it is
logged, and the hook event is still processed.

Background added later (`session.context_add`) answers with `delivery`,
which `update_session` passes on as `background_delivery`:
from the stored mode only, since a running agent has the hooks it loaded
when it started: `next_prompt` (Hook; a missing mode counts as Hook, as
the hooks take it), `next_start` (Claude's system prompt file) or
`file_only` (only in `context.md`). A session given its first background
after launch gets a mode then: Hook if rlord's hooks work for its agent,
else the system prompt file (Claude) or the pointer. The overlord's own
row never gets background (refused, and hook answers skip overlords). `update_session` checks the background
before writing anything else, makes its other edits, then stores the
checked note and links in-process (`methods::context_store`; never
taken from a caller over RPC). `overlord.md` has the overlord also use `send_to_session` when
the session must know now and it isn't `next_prompt`.

Nothing counts as handed over until the hook client says it printed the
answer: `rlord hook emit` prints it, then sends `hook.ack {ack}`
(`send_hook_ack`, a second each way). The bookkeeping is two columns on
`session_context`: an answer stamps the rows it carries with its ack id
(`sent_ack`, `sent_at`; the ack id is `<session id>.<fresh id>`), and an
ack marks seen the rows still stamped with it (a newer answer that
carried them again took them over). A whole answer also leaves kv
`context_full_sent:<id>` (`{ack, conv}`): acknowledged, that
conversation counts as reached (kv `context_delivered:<id>`). The ack
logs `context_sent`. An answer never acknowledged (a slow daemon, a
client that died) goes again with the next prompt: at most one
duplicate. After `UNACKED_TRIES` (2) unacknowledged answers in a row to
one conversation (kv `context_unacked:<id>`), the rows they carried
count as seen and the conversation as reached, logged once as
`context_assumed_delivered`; a note no answer carried yet still goes, in
that same answer. A start, clear or compact, and a conversation the
count isn't about, always get the whole background and start the count
afresh. The answer is built last in `hook_emit` (after the event's own
work, which can fail), so its bookkeeping never runs for an event that
didn't go through; it names `context.md` only if the file is there.

### Forking a session

`session.fork` (`daemon/src/fork.rs`; the list's `f`, `rlord fork`, MCP
`fork_session`) starts a new session that goes on from a copy of another
session's whole conversation, on the same agent. The original is left as
it is. Plain code, no AI: the `f` form forks directly (no overlord turn,
no `confirm_*` tool: the options are few and each has a safe default).
The form sends off the UI thread; a fork that fails brings it back with
the error and what was typed, once nothing else is open. The CLI and the
TUI word the answer alike (`rlord_core::forked`). Like `import`, the slow and outside parts run without the DB lock (the
hooks check, the parent's usage, OpenCode's fork call, `git worktree
add`); reading and inserting under it, through `methods::start`, the tail
`session.new` uses too.

| | Claude Code | Codex | OpenCode |
|---|---|---|---|
| How | `claude --resume <parent> --fork-session --session-id <new>` | `codex fork <parent> --no-alt-screen [--model m] -c tui.resume_cwd="current" --no-daemon [prompt]` (`codex::fork_argv`) | `POST /api/session/<parent>/fork` (`{}`) on its server, else `opencode api …` run in the parent's folder (60s each); then `opencode -s <fork>` |
| New id | minted by rlord (`Agent::fork`) | Codex's: bound by hook, or from the thread index by `bind_fork` | the server's answer (`{data: {id}}`) |
| Resumed at a restart once | its transcript is on disk (from its first message) | it is bound and its rollout is on disk | at once |
| New worktree | yes (`--resume` finds the parent from another worktree of the repo, and from a subfolder of it) | yes (`tui.resume_cwd`: below) | no: the server gives a fork its parent's folder |

Until then a restart forks again (kv `fork_pending`, `fork::pending`,
written with the row before the pane starts, as is `bind_fork`), logged
as `reforked` with its new branch point, and sent the words it was
first given again (`spec["prompt"]`, `fork::restart_prompt`). Its file is written with the
fork's note renewed in memory (`fork::renewed_note`,
`context::refresh_file_with`; shared folder or own worktree as recorded
at the fork, kv `fork_folder`), and a Codex fork's `bind_fork` is
written, before the pane starts. Only once the pane is up is the note
stored, unseen (`fork::renote`), does a Codex fork drop the thread
bound before so the new one is bound (`fork::reforked`), and does the
start mark seen what it handed the agent. From the pane on nothing
fails the restart (each step is logged if it fails; the status, the
seen marks and the event are always written); a restart that fails
before it leaves the note and the bound thread as they were. "On disk"
is one check for both a parent and a restart
(`fork::conversation_on_disk`). The key goes once
a hook or Codex's index names the fork's own transcript and it is on
disk (`fork::conversation_seen`), or at a fresh start. Forks of one parent are made one at a time (a
`fork:<parent>` lock in `locks::SessionLocks`, from the fork call to the
row). An OpenCode fork is only ever the id OpenCode's answer gave: a
call that fails or times out is reported (with a word that OpenCode may
have made it anyway, in its own list), never guessed from the parent's
forks, which may be the user's; so `undo` can only ever delete a session
rlord made. The answer is read as JSON (`opencode::fork_response`, with
the HTTP status when there is one): only an error answer whose error is
`ForkEmpty` means "nothing to fork yet" (`opencode::fork_empty`).
OpenCode's client, and the delete in `undo`, run through
`process::run_agent` (not as one of rlord's sessions, output through
rlord's folder), as the archive summaries' commands do.

Checked live on Claude Code 2.1.326, codex-cli 0.169.0 and OpenCode
2.2.7 (scratch setups, one short prompt each).

- **Folder.** The parent's (`project_path`, its worktree, repo, branch),
  or with `workspace` a new worktree branched from the parent's `HEAD`
  (`git::worktree_add` with a start commit), the fork working in the parent's subfolder
  of it (`fork::plan_worktree`, which asks git outside the lock; a repo
  with nothing committed is refused: "nothing committed yet"). A
  subfolder that isn't committed isn't there: refused, the worktree
  removed. Changes not committed (untracked files too) don't come along,
  and the answer says so when there are some.
- **A fork that fails before its pane starts leaves nothing**: its row
  (if inserted) with its background rows, events and kv keys
  (`Db::delete_session`), its background folder, the worktree and branch
  made for it, and OpenCode's server-made fork (`fork::undo`, best
  effort). Once the pane is up the fork is never undone: `methods::start`
  can't fail after it (its last log line is best effort), and what
  follows in `finish` (group, tags, description, reading the row back) is
  logged, not returned, so the daemon's steps after a fork always run.
- **Name.** Given, or `<parent>-fork`, `-fork-2`… chosen under the first
  lock (after every read that can fail) and reserved while the fork is
  made (kv `fork_name:<name>`, ten minutes at most), so two forks at once
  never pick the same one (by `fork`, not `want_in`, which only reads).
  A worktree's branch follows it, so a worktree fork's default also
  skips a name whose branch or worktree folder git already has (read
  once before the lock, `fork::git_names`: no git runs under it). Checked again
  before the worktree is made and under the last lock: taken meanwhile by
  another session, a worktree fork stops cleanly; any other picks the
  next free name.
- **Codex in another folder.** `codex fork` started anywhere but the
  parent thread's last folder first asks which folder to work in, with
  the parent's highlighted; there its thread would be indexed under the
  parent's folder and never bound. The fork's argv sets
  `tui.resume_cwd="current"` for that run (`codex::FORK_CWD`): no
  question, and the thread's `cwd` is the folder it was started in.
- **What it copies**: agent, extra flags, a Remote Control the user
  turned off (and when the parent's was on, the fork's is set off: a
  `--resume` of the copy must never reconnect under the parent's
  Remote Control), group, tags, description (or the one given), and the
  parent's `session_context` rows with `at` and `source` (not which hook
  answer carried them), but not the parent's own fork note (a fork of a
  fork gets its own). Its model is the one asked for, else the parent's
  launch spec model, stored in the fork's spec and passed as `--model`
  at the fork and every resume (`claude --resume` and `codex fork` don't
  restore a conversation's model); an OpenCode fork has its parent's
  already, and only a model asked for is set. The rows are copied unseen: the fork's own
  delivery mode hands them over, as for a launch (whatever the parent's
  conversation already had). Not notes (the user's own, per session), pin,
  summary, usage, Remote Control's name, `codex_host`, role. It adds a
  note (`context::fork_note`) and a link to the parent, both `source =
  'fork'`, then the overlord's `context` and `related`. Past `LINKS_MAX`
  the oldest copied links are dropped, never the link to the parent nor
  a session named in `related`. A fork always has background
  (`context_on`); its delivery is worked out afresh, as for a launch.
  Its file, written before the rows are stored, shows only what will be
  stored (`fork::rows` drops, through `context::kept_rows`, what `context_add`
  would skip).
  Nothing is added to the parent.
- **Lineage**: `sessions.forked_from` (migration 0011); rows carry
  `forked_from_name`. List rows (`session.list`) carry counts only
  (`fork_count`, `live_fork_count`: one grouped query over these rows'
  forks); `session.get` carries the `forks` themselves (id, name,
  archived; `Db::attach_forks`); a plain lookup (`resolve_ref`, which
  every hook makes) neither. MCP `list_sessions` rows give `live_forks`,
  `get_session` the list. The list marks a fork `⑂`; details and `rlord
  show` say both ways: the TUI's details pane loads the selected row's
  fork names with its preview, once the highlight settles, only for a
  row with forks and again only when its count changes. The fork's
  first event is `forked` (`from`, `from_conversation`, argv) instead of
  `launched`.
- **Refused**: an overlord; no conversation yet (none bound, Claude's
  transcript or a bound Codex thread's rollout not on disk, OpenCode's
  `ForkEmptyError`); a `scavenged` id; a parent whose folder is gone
  (archived ones fork while it's there); an agent not installed; OpenCode
  into a new worktree; a `branch` without `workspace`; a parent whose
  conversation changed while the fork was made (checked again in
  `finish`: "try again"; what was made is undone).
- **Warnings** (`warnings` in the answer, by plain code, of the parent as
  read when the fork starts): a working
  parent (the copy ends at its last finished step), a shared folder with
  a live parent, a context window at or over 70% (as the list knows it:
  `usage::annotate` with `single` false, never a full transcript read;
  unknown says nothing), uncommitted changes
  left behind by a new worktree.
- **Binding a Codex fork.** Its first message is its parent's, so the
  prompt rule can't pick its thread. kv `bind_fork:<id>` holds the
  parent's thread: `discover::pick_thread` then takes only a thread whose
  rollout's first `session_meta` says `forked_from_id` is that one
  (`codex::rollout_first_line`; a candidate whose first line can't be
  read yet is skipped, never a reason to wait while another names the
  parent). Several forks of one parent waiting in one folder queue
  oldest first (`discover::place_of`, by their `bind_since`): each takes
  the thread naming the parent made closest after its own start; the
  oldest binds once there is a thread for each of them (or it waited
  `UNREAD_GRACE_MS`), a later one only after waiting, so none stays
  unbound. Any other waiting session takes the one thread its rules
  narrow to; with no rlord fork waiting in that folder its rollout isn't
  read. With one waiting, it is taken unless its rollout says it was
  forked from the thread that fork is looking for (a fork of anything
  else is the user's own, made in the session's pane), and one not
  readable yet is taken 15 seconds after the thread was indexed
  (`UNREAD_GRACE_MS`), so no session stays unbound. A waiting fork holds
  such a session back (as another session waiting in its folder) only
  for `UNREAD_GRACE_MS`. No other rollout is read. An answer is
  remembered per thread once its first line is complete
  (`Daemon::fork_flags`, the oldest forgotten first past 4096); a
  complete first line that can never be read (over 4 MB, not JSON) is
  remembered as no fork once the grace has passed. Under the lock,
  `record_bind` never binds a thread another session already has.
  The overlord's `bind_now`, under the DB lock, reads none: the overlord
  is never forked, and no fork runs in its folder. A Codex fork runs no
  `SessionStart` before its first
  prompt, so it is usually bound from the index first and confirmed by
  its hook later.
- **A user fork always runs in the pane**, whatever the parent's host:
  its hooks carry its own `RLORD_SESSION_ID` and bind by pane
  (`Route::Pane`, `SessionStart` of any `source`), and
  `codex_gate::starts_fresh` stays as it is (a `fork` never binds a
  daemon-mode session waiting by folder).

**User fork and summary fork.** "User fork" is `session.fork`: a session
with its own row, tmux session and `RLORD_SESSION_ID`. "Summary fork" is
the archive summary's one-shot copy (`claude::summary_argv` with
`--no-session-persistence`, `codex exec fork --ephemeral`, OpenCode's
archived throwaway found by `opencode::fork_of`, archived only): it never
becomes a session, and nothing in forking can pick it up (a user fork
isn't archived, so `fork_of` never returns one; `descendants` goes by
`parent_id`, which a fork doesn't have).

## Naming

- Default name: the project directory basename + short discriminator (not
  whimsical wordlists); `auto` flag until the user or an agent names it.
- `rename` via CLI/TUI/MCP sets `name_locked`; Claude `/rename` sync honors
  the lock in both directions (push `--name` on next launch, adopt Claude's
  rename only when unlocked).
- Names are display-only; every API takes ids (with name-prefix matching as
  CLI sugar).

## Failure model

- **Daemon crash/restart**: clients reconnect with backoff; hook `emit` spools
  to disk and is drained on startup; tmux panes are untouched (agents keep
  running — the daemon is an observer/controller, not a parent process).
- **Reboot**: the launchd agent or systemd unit (if installed; else the first client) starts daemon → reconcile → sessions marked
  resumable → auto-resume policy or one keystroke in the TUI (`R` = restart
  selection / all resumable).
- **tmux server death**: same as reboot from the daemon's perspective.
- **Wrong-conversation resume**: structurally prevented by the id trust rule;
  refusal starts a fresh conversation and logs an event explaining why.

## Phased plan

Each phase ends runnable and used-for-real (dogfood from phase 2 onward).

- **Phase 0 — scaffold** (small): cargo workspace, CI (fmt/clippy/test),
  `rlord daemon` skeleton with UDS JSON-RPC echo, SQLite migrations
  harness, systemd unit.
- **Phase 1 — Claude sessions end-to-end**: sessions table + event log; tmux
  supervisor (create/respawn/attach, control-mode client); Claude adapter
  (minted id, hooks install, hook ingest → status, resume); CLI `ls/new/
  attach/restart/stop/log`. *Acceptance: create a Claude session, reboot the
  VM, `rlord restart` it, land back in the conversation; `ls` reflects
  status changes within ~1s of hook events.*
- **Phase 2 — meta-context + TUI**: derived timestamps, tags/groups/pins/
  archive, query object across CLI, TUI v1 (list, live status, sort/filter,
  attach, archive view). *Acceptance: good enough to daily-drive for
  Claude work.*
- **Phase 3 — MCP + messaging + transcripts**: MCP server + stdio shim +
  identity; mailbox with the three delivery channels; transcript mirror
  (raw + rendered) + rg-backed `rlord search` / `search_history`;
  `get_context` served from the transcript store; `report_done`; the skill
  snippet. *Acceptance: from one Claude session, list the fleet, read another
  session's context, task it, and receive its completion — without any Bash
  round-trips or conductor session.*
- **Phase 4 — Codex adapter**: notify install, resume gates, fork fallback,
  pane patterns; messaging via pull + nudge channels. *Acceptance: phase-1
  acceptance test passes for Codex; cross-agent messaging Claude↔Codex works.*
- **Phase 5 — quality of life** (pick by pain): global transcript search,
  cost view, generic TOML adapters, auto-resume policies, worktree-per-session
  helpers, desktop notifications.

## Open questions (deferred, not blocking)

- Control-mode coverage: which tmux operations misbehave in `-C` and need the
  exec fallback (spike early in phase 1).
- MCP crate maturity (`rmcp`) vs. hand-rolling the protocol over the shim.
- Whether `get_context` should optionally summarize via a cheap model call
  (explicitly out of v1 — raw tail + event summary first; keep AI out of the
  hot path).
- Multi-machine (SSH remotes): out of scope until the single-box
  product is right.
