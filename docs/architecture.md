# Robot Overlord — architecture plan

Status: v1 plan, 2026-09-11. Follows from `research-other-tool.md`; read that
first for the evidence behind the choices here.

## Locked decisions

| Decision | Choice | Why |
|---|---|---|
| Language | **Rust** | Commodity crates cover every need (tokio, rusqlite, notify, ratatui, rmcp); we want a resident daemon that idles at ~zero cost. |
| Frontend | **Terminal (TUI)** | Matches how we develop. GUI stays cheap to add later because the core is a daemon any client can speak to. |
| Process substrate | **tmux** | It buys real things: sessions survive SSH disconnects and client crashes, `respawn-pane -k` gives atomic in-place restart, attach UX is free, and every agent CLI runs unmodified in a pane. Not worth reinventing with a hand-rolled PTY supervisor. |
| Control plane | **Live daemon (`rlordd`)** + thin clients | other-tool's CLI-only model is slow because every call is a cold start (process spawn → SQLite open → several tmux round-trips → pane scrapes). A resident daemon keeps state hot, holds one persistent tmux control-mode connection, and pushes events instead of being polled. |
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
daemon is `rlord daemon` (run as a systemd user service). Crates:

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
  `tmux` binaries per query (other-tool forks tmux constantly; this is a large
  share of its sluggishness). Plain `tmux` exec remains as fallback for the
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
  kind       TEXT NOT NULL,         -- prompt_submitted | agent_stopped | tool_use |
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
`last_user_msg_at`, `last_agent_msg_at`, `last_state_change_at`,
`last_delivery_at`. These are **distinct and honest** — no max-of-six-sources
badge heuristic. Anything unknowable (e.g. pre-rlord history) is NULL and
displays as "unknown", never as epoch or a guess.

Sort/filter/pivot in CLI, TUI, and MCP all take the same query object:
filter by status/agent/tag/group/archived/project, sort by any derived
timestamp or name or manual order. This is the number-one product feature and
it is just SQL.

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
  policy flag (default off, matching other-tool's "can't distinguish crash from
  intentional kill" caution — but opt-in-able per session or group, which
  other-tool never offered).

## Agent adapter contract

```rust
trait AgentAdapter {
    fn launch_argv(&self, s: &Session, first_prompt: Option<&str>) -> Argv;
    fn resume_argv(&self, s: &Session) -> Result<Argv, ResumeRefusal>;
    fn install_integration(&self) -> Result<()>;   // hooks / notify program
    fn on_hook_event(&self, s: &mut Session, ev: HookEvent) -> StatusEvidence;
    fn pane_patterns(&self) -> &PanePatterns;       // busy/prompt/error regexes
    fn read_transcript_tail(&self, s: &Session) -> Option<TranscriptTail>;
    fn delivery_channels(&self) -> &[DeliveryChannel]; // ordered preference
}
```

**Claude adapter (first-class):**
- Mints the session UUID; launches with `--session-id <uuid>`; resumes with
  `--resume <uuid>`. Trust rule inherited from research: only `minted` or
  `hook`-confirmed ids may authorize `--resume`; a disk-scavenged id never
  does (start fresh instead). Fork = `--session-id <new> --resume <old>
  --fork-session`.
- `rlord claude install-hooks` merge-writes `rlord hook emit claude`
  into Claude's `settings.json` for SessionStart, UserPromptSubmit, Stop,
  PermissionRequest, Notification, SessionEnd, PreCompact (preserving user
  hooks). Hook events drive status, capture/confirm the session id, feed the
  event log, and carry mailbox delivery (below).
- Transcript JSONL read-back for last message / latest prompt / usage-limit
  detection; `/rename` ↔ name sync with `name_locked` respected both ways.
- Cost events from Stop-hook usage fields → `events` (kind `cost`).

**Codex adapter (second):**
- `install_integration` writes `notify = ["rlord","hook","emit","codex"]`
  into `$CODEX_HOME/config.toml` between markers (auto-offered, not
  manual-only as in other-tool).
- Resume `codex resume <sid>` gated on the rollout file existing; subagent
  threads rewritten to `codex fork <sid>`. Id binding: env → notify payload →
  bounded rollout scan (scavenged ids get the same no-resume taint rule).

**Generic tier** (later): a TOML-defined adapter (launch cmd, resume flag, id
source, patterns) so a new CLI costs config, not code — other-tool proved the
shape works.

## Status engine

Same three-tier degradation the research validated, implemented once:

1. **Native events** (Claude hooks / Codex notify), freshness-windowed.
   Vocabulary: `starting | working | waiting_input | idle | stopped | error`.
2. **Pane hints** — OSC pane-title spinner check (zero-capture fast path).
3. **Pane capture + patterns** — via the control-mode stream where possible;
   activity-gated capture otherwise; error-banner classification for Claude.

Every status transition is an `events` row and a push to subscribers; that is
what makes the TUI live and `last_state_change_at` trustworthy.

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
latency, disk for description/tool-use id/backfill); `sessions` carry
`agents_running`/`agents_total`. Surfaces: TUI details card + `⚙N` in the
list, `rlord agents`, `rlord show`. Sub-agent transcripts will join the
transcript store so `rlord search` covers them.

## Inter-session messaging

Durable mailbox in the daemon; **delivery is channel-ranked per adapter**,
keystroke injection last:

1. **Pull (MCP)** — the recipient agent calls `check_messages` whenever it
   likes; the tool also appears in results of other rlord tool calls as a
   pending-count hint. Zero-risk, always available.
2. **Next-turn injection (Claude)** — recipient busy: its Stop hook drains the
   mailbox and returns `{decision:"block", reason:<messages>}`, so mail
   becomes the next turn. Consecutive-block cap (3) to prevent token-burn
   loops. This is other-tool's proven mechanism, kept.
3. **Idle nudge (fallback)** — recipient idle at a confirmed prompt: inject
   text via `send-keys`/`paste-buffer` with post-send verification. Only used
   when 1–2 can't apply (non-Claude agents without a hook channel), because
   this is where all of other-tool's fragility lived.

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
- `list_sessions(filter?, sort?)` — same query object as the CLI; returns
  meta-context (status, timestamps, tags, pending-message counts).
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
("prefer get_context over asking the agent", "check_messages when idle") —
the cheap part of other-tool's skills approach, kept.

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
right, `stack`), `list_width_pct`, `preview_refresh_ms`, `[keys]` (every TUI
action), `[tmux]` (the three navigation bindings).

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
(existing branch → checkout; dirname = branch with `/`→`-`, other-tool's
convention), excludes the dir via `.git/info/exclude`, and launches the agent
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
- **Reboot**: systemd user service starts daemon → reconcile → sessions marked
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
  attach, archive view). *Acceptance: daily-drive replaces other-tool for
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
- Multi-machine (other-tool's SSH remotes): out of scope until the single-box
  product is right.
