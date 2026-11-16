**Status:** in progress

# Session context: separate, re-route, pull

What a launched session gets from the overlord, and how. Option C, chosen
by the user on 2026-11-15. Four PRs; this doc is where each one picks up.

## The problem

`create_session` takes one `prompt`. The overlord puts the user's words in
it, then a `## Context from the overlord` section with its own notes. So:

- the agent can't tell the user's words from the overlord's;
- the whole thing is on argv: in `ps`, the tmux pane command, kv
  `launched_args:<id>`, the `launched` event, `launch_spec.prompt`;
- the overlord copies other sessions' content into prompts, which costs it
  context and goes stale;
- a later message (`send_to_session`) is only ever pasted into the pane.

## Design

**Three inputs instead of one.** `prompt`: the user's words, verbatim.
`context`: the overlord's notes (facts, pointers, answers the user gave).
`related`: refs to sessions the work builds on.

**Store.** Table `session_context` (migration 0010): one row per note
(`kind = 'note'`, `body` the text) or link (`kind = 'link'`, `body` the
linked session's id), with `at`, `source`, and `seen_at` (pushed to the
agent by a hook). Rows go with their session (`ON DELETE CASCADE`).

**Render** (`daemon/src/context.rs`, plain code, deterministic, clipped):
a fixed header saying the notes are background from rlord's overlord, not
the user's words and not instructions; the "you are the session the user
asked for" line (moved out of `overlord.md`); the notes, dated; the
related sessions (name, folder, status, description). Budgets: 9,000
chars for Claude, 6,000 for Codex, 64,000 for the file. Over budget, the
oldest notes are dropped with a line saying so and naming the file.
Control and bidi characters are stripped.

**Per-session files.** `<data>/sessions/<id>/` is 0700; files in it are
created 0600. `context.md` is the rendered background, rewritten whenever
the rows change. (PR 2 adds `token` and Claude's `mcp.json`; PR 3 the
first-prompt files.)

**Push (Claude, Codex).** `hook.emit` answers with `{"reply": <hook
output>}` and `rlord hook emit` prints it, only after the routing and
own-pane checks, and only for the session's own conversation:

- `SessionStart` (any source): the full render as
  `hookSpecificOutput.additionalContext`; rows marked seen; kv
  `context_delivered:<id>` = the conversation it went to.
- `UserPromptSubmit`: only unseen rows, under "rlord background,
  updated". If no SessionStart reply went to this conversation, the full
  render instead (heals a missed one).
- No background: no reply, as before. Daemon slow or unreachable: nothing
  printed; a hook never blocks the agent.

**Fallback pointer.** OpenCode (no hooks), Codex with untrusted hooks:
one fixed line after the user's words naming `context.md`. Claude
without rlord's SessionStart hook: `--append-system-prompt-file
<context.md>` at every start instead, so its prompt stays clean.

**Pull (PR 2).** Per-session read-only tools behind a 0600 token file:
`session_brief`, `related_sessions`, `session_summary`, `search_history`,
`read_history`, over a visible set (itself, linked, same repo, same tag;
never overlords). Front ends `rlord mcp --session --token-file` and
`rlord context <tool>`; wiring computed in `spec_args` at every start.

**Prompt off argv (PR 3).** `FirstPrompt { Stdin, Server, Pointer, Typed
}`: Claude from a 0600 file on stdin (if V1 passes), OpenCode POSTed to
its server, Codex a fixed pointer line to `prompt.md` (user's decision),
`Typed` as the last resort.

**Native later messages (PR 4).** OpenCode: POST with `delivery:
"queue"`. Claude/Codex mid-turn: the Stop hook claims the held message
(`decision: block`), at most 3 in a row. Idle: paste, as today. Framing
by code (`context::frame_message`).

## Per-agent table (versions: Claude Code 2.1.325, codex-cli 0.169.0, OpenCode 2.2.7)

| Need | Claude | Codex | OpenCode |
|---|---|---|---|
| Background at start | SessionStart `additionalContext` (fallback `--append-system-prompt-file`) | SessionStart `additionalContext` when hooks are trusted (fallback pointer line) | pointer line (V4 may find an API field) |
| First prompt off argv (PR 3) | stdin file if V1 passes, else typed | pointer to `prompt.md` | server POST, else typed |
| Later message (PR 4) | Stop-hook block mid-turn; paste when idle | same | POST `delivery: queue` |
| Scoped tools (PR 2) | `--mcp-config` + `--allowedTools` | `-c mcp_servers.rlord_session.*` in pane; CLI in daemon mode | CLI `rlord context` |

## User decisions (2026-11-15)

- Codex first prompt: pointer file (prompt in a 0600 file; argv carries a
  fixed line pointing at it).
- Visible set: linked + same repo + same group. Never overlords.
- Live checks V1–V6 (plus the Codex pointer-read check): allowed, scratch
  setups only, one short prompt each.
- Four PRs, in order.
- Guard rail (refuse mutating `rlord` commands from inside a non-overlord
  session): not in this effort; follow-up below.
- Form display: a read-only preview line of the overlord's notes.
- Permissions: every rlord-owned folder (data, config, Linux log dir) is
  created 0700, the DB and per-session files 0600, by one shared helper
  at creation. No chmod of existing installs at start (replaces plan
  step 2's start-up fix-up).

## The four PRs

1. **0.23.0-dev.28: store, separation, background** (this one).
2. Pull: token, `session_tools.rs`, `sctx.*`, `rlord mcp --session`,
   `rlord context`, `spec_args` wiring, pane env.
3. Prompts off argv: V1/V4, `FirstPrompt`, stdin file, `Deferred.prompt`,
   kv `first_prompt`, `first_prompt_next`, `discover` change, argv secrecy
   tests.
4. Native later messages: V3/V6, OpenCode queue POST, Stop-hook claim,
   `send_to_session` `context`, framing; then reconcile this doc.

## PR 1 progress

- [x] Version bump to 0.23.0-dev.28.
- [x] This working doc.
- [x] Permissions (user's decision, 2026-11-15): rlord's folders are
  created 0700 and its DB and per-session files 0600, at creation, by one
  helper (`rlord_core::private`). Used wherever a folder may first be made:
  the data dir (`Db::open`, the daemon lock, tmux conf and profile, the
  Codex gate, the hook spool), the config dir (`config init`, config
  writes), the log dir on Linux (and the log file, 0600, on both). No
  fix-up of existing installs and no chmod at daemon start: the only two
  installs are the user's, already fixed by hand. (There is no
  `rlord setup` command; first run is the daemon's first start.)
- [ ] Migration 0010, `Db::context_*`.
- [ ] `context.rs`: render, files, `split_legacy`, `frame_message`.
- [ ] `session.new` takes `context`, `related`; `update_session`
  `add_context`, `add_related`; `get_session` shows them; form preview.
- [ ] Hook replies (SessionStart, UserPromptSubmit), printed by
  `rlord hook emit`.
- [ ] Fallback pointer; Claude's `--append-system-prompt-file`.
- [ ] `overlord.md`, `docs/architecture.md`, `docs/guide.md`.

## Owed / deferred

- `context::split_legacy` (old-style prompts with a `## Context from the
  overlord` section) is for overlords still on old instructions. Remove it
  a few releases after PR 1 ships.
- Follow-up, not in this effort: the guard rail above.
- Reconcile at PR 4: Codex daemon-mode `turn/start`, an overlord token.
