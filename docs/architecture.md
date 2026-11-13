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
  `rlord hook emit` does no SQLite, no tmux — it writes one length-prefixed
  JSON frame to the daemon socket and exits. Budget: <10ms. If the daemon is
  down, it appends to a spool file that the daemon drains on startup, so hook
  evidence is never lost.

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
  archived_at   INTEGER             -- NULL = live
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
  transcript. A transcript with no position,
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
  width).

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
  path when rlord can choose them), `resume`, and the same `launch` for a
  fresh conversation in the pane.
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
  (`opencode -s <ses_…> [--prompt …]`). `Launch::deferred` names the model
  for `daemon::opencode` to confirm afterwards (set it if the create step
  didn't take; log it if it can't be).
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
  what tells a limit. Seen on Claude Code 2.1.322. Codex and OpenCode
  are not detected yet.
- A session is limited while that record is the last thing in its
  conversation (no prompt the user typed and no real answer after it:
  `LimitHit::touched`) and the reset time is ahead. A tick (15s) stats
  each live session's transcript and re-reads only those that changed
  (one scan at a time; a file's stamp is kept only once what was read
  from it is saved; a tail without `usage_limit_reached` or `"rejected"`
  isn't parsed; a record with no readable time is dated when first
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
  prompt since the send (`last_user_msg_at` later than the job's
  `sent_at`): then the session is theirs and the job ends. What rlord
  types (the resume itself, a `when_idle` delivery, `send_to_session`)
  never moves `last_user_msg_at` ("Typing into a pane"). The
  record a resume was sent on is kept as `sent_hit` (a hash of its line),
  so it stays the old limit even if nothing gets written after it. The overlords are never
  resumed this way: the user switches to another agent's.

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
described here are the original design and are not built.

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
`.mcp.json`: `rlord mcp` (proxies stdio ↔ UDS). Because the shim runs
inside the pane, it inherits `RLORD_SESSION_ID` — **caller identity is
automatic**, so tools can default "me" and enforce policy (e.g. rate limits,
no self-restart).

Tools (v1):
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
`--sort last-agent-msg --status waiting --tag foo`), `new`, `attach`, `send`,
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
| OpenCode | `<folder>/opencode.json`: `mcp.servers.rlord` (`opencode::overlord_config`); no flags | its `permissions`: `*` asks, then `read`/`glob`/`grep`, rlord's own subagents by name, `rlord_*` allow (last match wins); repeated under `agents.<name>` | `.opencode/agents/*.md` |

Things that are not visible at the call site:

- **`rlord mcp --socket <path>`**: every agent's config names the daemon's
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
  OpenCode subagent inherits the overlord's tools: only Claude's file can
  narrow them. A plain copy goes to `<folder>/subagents/` for an overlord
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
  top row with each installed agent's marker when there are two or more;
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
  - **A text the agent may have changed.** Claude Code shows a large or
    multi-line paste as `[Pasted text #1 +30 lines]` in its input. Its
    transcripts hold the expanded text (checked against local input
    history: every sampled placeholder entry was stored expanded), so
    the hook's `prompt` should carry it too, but that was not observed
    directly. As a guard, when the hash differs and either the reported
    text holds such a placeholder or is 1000+ characters, or rlord's
    paste was that long, a prompt within 5s of the paste is taken as it.
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
  paste fails, and expires after 10 minutes. "Updated" everywhere (the
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
  without hooks reads as working while an approval is up. Sends also hold
  off while the input holds a draft (read with attributes, so a dim
  placeholder isn't one: Claude's input box, Codex's composer; OpenCode's
  isn't read yet), within 10s (`SETTLE`) of the last paste into that
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
  `mcp.tools` / `mcp.call`. Tools are defined in `daemon/src/mcp.rs` as thin,
  param-whitelisted mappings onto daemon methods.
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
