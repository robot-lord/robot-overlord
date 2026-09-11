# AI Overlord — architecture plan

Status: v1 plan, 2026-09-11. Follows from `research-other-tool.md`; read that
first for the evidence behind the choices here.

## Locked decisions

| Decision | Choice | Why |
|---|---|---|
| Language | **Rust** | Commodity crates cover every need (tokio, rusqlite, notify, ratatui, rmcp); we want a resident daemon that idles at ~zero cost. |
| Frontend | **Terminal (TUI)** | Matches how we develop. GUI stays cheap to add later because the core is a daemon any client can speak to. |
| Process substrate | **tmux** | It buys real things: sessions survive SSH disconnects and client crashes, `respawn-pane -k` gives atomic in-place restart, attach UX is free, and every agent CLI runs unmodified in a pane. Not worth reinventing with a hand-rolled PTY supervisor. |
| Control plane | **Live daemon (`overlordd`)** + thin clients | other-tool's CLI-only model is slow because every call is a cold start (process spawn → SQLite open → several tmux round-trips → pane scrapes). A resident daemon keeps state hot, holds one persistent tmux control-mode connection, and pushes events instead of being polled. |
| AI control surface | **First-class MCP server** (CLI kept as the human/scripting surface) | Agents call tools directly — no Bash spawn, no arg parsing, no output re-parsing. Both surfaces are thin facades over one internal API, so nothing is MCP-only or CLI-only. |
| Agent support | **Claude Code first-class, Codex second**, generic adapter trait for the rest | Matches the measured cost curve: Claude is the easiest deep integration (accepts a minted `--session-id`); Codex has a known-good recipe (notify program + rollout gates + fork fallback). |

## System overview

```
                                   ┌────────────────────────────┐
  ┌──────────┐   UDS (JSON-RPC)    │  overlordd (daemon, tokio) │
  │ overlord  │◄──────────────────►│                            │
  │   CLI     │   + event stream   │  ┌──────────────────────┐  │
  └──────────┘                     │  │ core API (one impl)  │  │
  ┌──────────┐                     │  └──────────────────────┘  │
  │ overlord  │◄──────────────────►│   │        │         │     │
  │   TUI     │                    │   ▼        ▼         ▼     │
  └──────────┘                     │ state    status   mailbox  │
  ┌──────────┐    UDS (proxied     │ store    engine   /events  │
  │ overlord  │◄──────────────────►│ (SQLite)                   │
  │ mcp shim  │     MCP)           │        tmux supervisor     │
  └────┬─────┘                     │   (persistent control-mode │
       │ stdio                     │    client + exec fallback) │
  ┌────▼─────┐                     └──────┬──────────────┬──────┘
  │ Claude / │                            │              ▲
  │ Codex in │◄── pane is the agent ──────┘              │ UDS
  │ tmux pane│                                     ┌─────┴─────┐
  └──────────┘                                     │ overlord   │
                                                   │ hook emit  │ ← Claude hooks /
                                                   └───────────┘   codex notify
```

One cargo workspace, one installed binary `overlord` with subcommands; the
daemon is `overlord daemon` (run as a systemd user service). Crates:

- `overlord-core` — domain types, state store, event log, status engine,
  mailbox, adapter trait + claude/codex adapters, tmux supervisor.
- `overlord-daemon` — API server (UDS JSON-RPC + subscriptions), MCP server,
  hook ingest, reconciliation loops.
- `overlord-cli` — thin client subcommands, `hook emit`, `mcp` stdio shim.
- `overlord-tui` — ratatui client.

### Why the daemon fixes the responsiveness problem

- State reads (`ls`, sort, filter) never touch tmux or disk on the hot path:
  the daemon holds the session table in memory, persisting write-through to
  SQLite. A CLI `overlord ls` is one UDS round-trip.
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
  `overlord hook emit` does no SQLite, no tmux — it writes one length-prefixed
  JSON frame to the daemon socket and exits. Budget: <10ms. If the daemon is
  down, it appends to a spool file that the daemon drains on startup, so hook
  evidence is never lost.

## State model

SQLite (WAL) at `$XDG_DATA_HOME/ai-overlord/state.db`. The schema encodes the
lesson from the research: **meta-context is derived from an event log, not
from throttled ad-hoc columns.**

```sql
sessions (
  id            TEXT PRIMARY KEY,   -- overlord id (ULID)
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
badge heuristic. Anything unknowable (e.g. pre-overlord history) is NULL and
displays as "unknown", never as epoch or a guess.

Sort/filter/pivot in CLI, TUI, and MCP all take the same query object:
filter by status/agent/tag/group/archived/project, sort by any derived
timestamp or name or manual order. This is the number-one product feature and
it is just SQL.

## tmux supervisor

- One tmux session per overlord session, named `ovl_<ulid>` — the name is an
  id, never derived from the display name (rename ≠ tmux churn).
- Launch: `tmux new-session -d -s ovl_<id> -c <project> <agent argv>` with
  `OVERLORD_SESSION_ID=<id>` (and agent-specific env) exported into the pane.
- Restart/resume: adapter builds the resume argv → `respawn-pane -k` if the
  tmux session survives, else recreate. Spawn-lock per session prevents
  restart storms.
- Attach: TUI suspends itself and execs `tmux attach -t ovl_<id>` (detach
  returns to the TUI); CLI `overlord attach <name|id>` does the same.
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
- `overlord claude install-hooks` merge-writes `overlord hook emit claude`
  into Claude's `settings.json` for SessionStart, UserPromptSubmit, Stop,
  PermissionRequest, Notification, SessionEnd, PreCompact (preserving user
  hooks). Hook events drive status, capture/confirm the session id, feed the
  event log, and carry mailbox delivery (below).
- Transcript JSONL read-back for last message / latest prompt / usage-limit
  detection; `/rename` ↔ name sync with `name_locked` respected both ways.
- Cost events from Stop-hook usage fields → `events` (kind `cost`).

**Codex adapter (second):**
- `install_integration` writes `notify = ["overlord","hook","emit","codex"]`
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

## Inter-session messaging

Durable mailbox in the daemon; **delivery is channel-ranked per adapter**,
keystroke injection last:

1. **Pull (MCP)** — the recipient agent calls `check_messages` whenever it
   likes; the tool also appears in results of other overlord tool calls as a
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

JSON-RPC over `$XDG_RUNTIME_DIR/ai-overlord/daemon.sock` (perms 0700 dir /
0600 socket — the socket is root-of-trust; no TCP listener at all in v1).
Methods, grouped: `session.{list,get,create,launch,restart,stop,archive,
unarchive,rename,set_tags,set_group,fork}`, `msg.{send,list,ack}`,
`events.{query,subscribe}`, `agent.{install,doctor}`. `subscribe` upgrades the
connection to a push stream (status changes, new messages, session CRUD).

### MCP server

The daemon serves MCP; each agent session gets it via a stdio shim in its
`.mcp.json`: `overlord mcp` (proxies stdio ↔ UDS). Because the shim runs
inside the pane, it inherits `OVERLORD_SESSION_ID` — **caller identity is
automatic**, so tools can default "me" and enforce policy (e.g. rate limits,
no self-restart).

Tools (v1):
- `list_sessions(filter?, sort?)` — same query object as the CLI; returns
  meta-context (status, timestamps, tags, pending-message counts).
- `get_session(id)` — detail + recent event summary.
- `get_context(id, {last_message | recent_activity | transcript_tail})` —
  "go get context from that other agent" without a round-trip through the
  agent itself: served from transcript read-back and the event log.
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

Thin UDS client, ~1 round-trip per command: `overlord ls` (rich table,
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
  `overlord daemon` skeleton with UDS JSON-RPC echo, SQLite migrations
  harness, systemd unit.
- **Phase 1 — Claude sessions end-to-end**: sessions table + event log; tmux
  supervisor (create/respawn/attach, control-mode client); Claude adapter
  (minted id, hooks install, hook ingest → status, resume); CLI `ls/new/
  attach/restart/stop/log`. *Acceptance: create a Claude session, reboot the
  VM, `overlord restart` it, land back in the conversation; `ls` reflects
  status changes within ~1s of hook events.*
- **Phase 2 — meta-context + TUI**: derived timestamps, tags/groups/pins/
  archive, query object across CLI, TUI v1 (list, live status, sort/filter,
  attach, archive view). *Acceptance: daily-drive replaces other-tool for
  Claude work.*
- **Phase 3 — MCP + messaging**: MCP server + stdio shim + identity; mailbox
  with the three delivery channels; `get_context`; `report_done`; the skill
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
