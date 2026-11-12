**Status:** in progress (planning)

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
  scrollback. Alt-O/Alt-M unbound; Alt-D deletes a word forward.

## Plan

1. OpenCode agent behind the interface: launch/resume with a minted
   `ses_` id, install detection (v2 gate), models (`opencode models`).
2. Status: a daemon task subscribed to the service's SSE, mapping events to
   working / needs you / your turn, with resync; stopped from pane death
   (and the service idle).
3. Transcripts: a SQLite-backed reader into the common shape (excerpts,
   stats, search, read); search "everywhere" over `session_v2`.
4. Summaries via `/generate`, fallback fork + delete.
5. Later: import, messaging via the API, OpenCode overlord seats.

## Open decisions

- Full TUI (no scrollback) vs `opencode mini` (inline, scrollback).
- Status source: SSE (recommended) vs a server plugin.
- Summaries: `/generate` vs fork + delete (verify `/generate` live first).

## Also queued: Codex Remote Control

Codex's Remote Control is machine-wide (`codex remote-control start/pair`)
and reaches threads run by Codex's background app-server, not rlord's
in-pane (`--no-daemon`) sessions. Supporting it means running remote Codex
sessions through the app-server and matching their hooks by conversation
id; it pairs naturally with Codex overlord seats.
