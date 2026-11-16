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
- [x] Migration 0010, `Db::context_add` / `context_rows` /
  `context_unseen` / `context_mark_seen`. Rows cascade with their session
  (sessions are never deleted today, only archived).
- [x] `context.rs`: render, files, `split_legacy`, `frame_message` (the
  last unused until PR 4), `check` / `store`, `hook_reply`, `delivery`.
- [x] `session.new` takes `context`, `related`; RPC `session.context` /
  `session.context_add`; MCP `update_session` `add_context`,
  `add_related`; `get_session` shows `background`; `propose_session`
  checks and carries them; the manual form shows a read-only preview line
  and launches with them.
- [x] Hook replies (SessionStart, UserPromptSubmit) via `hook.emit`'s
  `reply`; `send_hook_event_for_reply`; `rlord hook emit` prints it.
- [x] Fallback pointer (Codex untrusted, OpenCode); Claude without
  rlord's SessionStart hook gets `--append-system-prompt-file` at launch
  and every restart (`methods::background_args`).
- [x] `overlord.md` (prompt / context / related, `add_context`; "always
  ends with" dropped; test `instructions_keep_the_users_words_apart`),
  `docs/architecture.md` ("Session background", private files, hook
  path), `docs/guide.md`.
- Live check V2: not run (optional in PR 1).

### Decisions made in PR 1 (beyond the plan)

- **Header without notes.** The "you are the session the user asked for"
  line left `overlord.md`, so a session the overlord launches with no
  notes would lose it. `session.new` takes an internal `by_overlord`
  (set by `create_session`, and by the manual form when
  `propose_session` filled it); such a session has a background (kv
  `context_on:<id>`) even with no rows. A session the user launches
  themselves has none.
- **No `Pull` argument to `render` yet.** Nothing to point at until PR 2;
  PR 2 adds the "Looking up more" section.
- **Note length is refused, not clipped** (over 8,000 chars, more than 8
  related, an unknown or overlord ref): the overlord gets the error and
  can shorten it.
- **Bare launch with background:** a Codex (untrusted) or OpenCode
  session launched with no prompt gets no pointer line (it would start a
  turn). Its background is only in the file until PR 2 lets it pull.
- **`bind_prompt:<id>`** holds the first prompt as typed, pointer line
  included, since Codex binding matches the thread's first message.
- **Runtime dir** keeps `Daemon::bind`'s own 0700 (set every start; the
  socket is the trust boundary), outside `rlord_core::private`.

### Review round 1 (PR #64)

Fixed:

1. Spool replay: `methods::hook_replay` ingests a spooled event with no
   answer and marks nothing.
2. Marked only on acknowledgement: `hook.emit` returns `reply_ack`;
   `rlord hook emit` prints the reply, then sends `hook.ack` (best effort,
   1s each way). `context::ack` marks rows seen and records the
   conversation. Unacknowledged answers go again with the next prompt
   (one pending answer per session, a newer one replaces it).
3. Legacy notes are stored in pieces of at most 8,000 chars
   (`context::legacy_notes`), never refused. Explicit `context` still is.
4. At most 16 links per session (`LINKS_MAX`); `render` budgets the
   related list, notes keep at least half of what's left after the
   header when they need it; left-out related sessions are counted with
   the file named.
5. Delivery mode kept in kv `context_mode:<id>` at launch. Launched by
   system prompt file or pointer: launch rows are marked seen, a hook
   never sends the full render, later notes go at `UserPromptSubmit`.
   Restarts take the flag from the stored mode (and rewrite a missing
   file), not from a fresh hooks check.
6. `split_legacy` drops the old instruction per paragraph, ignoring case,
   line breaks and quote marks; text before it in the paragraph stays.
7. `Db::context_exists` (`SELECT EXISTS`); `hook_reply` does one row
   query (the `context_on` kv only when there are no rows).
8. `context::launch_args(mode, file)` is the one helper for launch and
   restart.
9. `methods::ref_list` is the one parser for `related` / `add_related`
   (MCP, `ui.propose_session`, `session.new`, `session.context_add`).

Declined (coordinator): `--append-system-prompt-file` being print-only;
`claude --help` 2.1.325 lists it for interactive sessions. V2 still not
run.

A session given background only after launch (no stored mode) gets it
through hooks, or from the file; it gets no system prompt flag.

### Review round 2 (PR #64)

Fixed:

1. Honest delivery for later background. `hooks_work` needs both
   SessionStart and UserPromptSubmit. `session.context_add` answers with
   `delivery` (`next_prompt` / `next_start` / `file_only`,
   `context::later_delivery`); `update_session` passes it on as
   `background_delivery`. `overlord.md` and the schema: not
   `next_prompt` and it must know now, also `send_to_session`.
2. `set_mode` marks launch rows seen only when they went out (the system
   prompt flag; the pointer line only after a prompt).
3. The hook's background answer is best effort: an error is logged, no
   answer, and the event's own processing goes on.
4. `split_legacy` runs only for the overlord's launches
   (`context::first_prompt(prompt, by_overlord)`); the user's prompts are
   never split.
5. `context_sent` is logged on every path that answers (also the early
   "unchanged" one).
6. The Codex gate file is written with `private::write` (0600, atomic).
7. `check` counts links with `Db::context_links`.
8. `hook_reply` returns after one query (`Db::context_exists_or`) for a
   session without background.
9. `docs/architecture.md`: files are *created* private; existing installs
   are not tightened.

Declined (coordinator): tightening existing installs at start (user's
decision); moving `frame_message` to PR 4 (the plan puts it in PR 1).

### Review round 3 (PR #64, rebased on main after #63 = 0.23.0-dev.27)

Live check V2 (2026-11-16, Claude Code 2.1.325): scratch tmux socket
`rlord-v2-scratch` (never `rlord` or the default server), interactive
`claude --append-system-prompt-file <tmp file: "The secret word is
PELICAN.">` with the prompt "What is the secret word? Reply with just
the word." It started normally (not rejected) and answered `PELICAN`.
Server killed after. So `--append-system-prompt-file` works
interactively; Claude without hooks keeps `SystemPromptFile`.

Fixed:

1. `compose` and `notes_section` share one overhead figure
   (`notes_overhead`, passed in): short notes are never clipped by a
   long related list (test).
2. V2 above.
3. Pointer mode with no words: the first prompt is the pointer alone
   (`context::pointer_only`: background in `<path>`, read it, wait for
   the user). `context::launch_prompt` builds the first prompt and says
   whether the background went out with it; rows are marked seen only
   then (test).
4. `update_session` runs `context::check` on `add_context` /
   `add_related` before writing name, description or notes (test).
5. The overlord's text is cleaned (`context::clean`) in the TUI preview
   and in `ui::propose_session`'s prefill (test).
6. `hooks_work` is computed by the daemon before the DB lock, for
   `session.new` and `session.context_add` (param `hooks_work`).
7. `context_sent` is logged in `context::ack`, when delivery is confirmed
   (test).
8. `later_delivery` takes the current hook state: hooks working now means
   `next_prompt`, whatever the launch mode (test).
9. Removed `Db::context_exists`, `paths::config_dir`, and the
   `send_hook_event` wrapper (tests call `send_hook_event_for_reply`).
   `frame_message` kept (plan: PR 1).

Declined (coordinator): tightening existing installs (user's decision).

### Review round 4 (PR #64)

Fixed:

1. `context_sent` was still also logged when the reply was built: that
   insert is gone, so it is logged once, on ack (test through
   `hook_emit` + `hook_ack`).
2. With it gone, nothing background-related can abort the hook event
   before its status, binding and needs-you work (the reply itself was
   already best effort).
3. `SessionStart` `resume` of a conversation already reached sends only
   unseen rows; `startup`, `clear`, `compact`, an unknown source and
   another conversation get the whole of it
   (`context::hook_reply_from`, source from the payload) (test).
4. A delivery to a conversation with no id yet records `""`, and the
   first real id seen is taken as that conversation, once (test).
5. `private::write` uses its own temp name (pid, thread, counter),
   taken with `create_new`, and never removes another writer's (test).
6. An update with nothing to render (only gone links) sends nothing and
   marks those rows seen (test).
7. `clean` also strips U+200B–U+200F, U+061C, U+2060–U+2064 and U+FEFF
   (test).
8. `hooks_work` is asked (outside the lock) for `session.new` only when
   it has `by_overlord`, `context` or `related`.

Declined (coordinator): moving `frame_message` to PR 4 (plan: PR 1).

### Review round 5 (PR #64)

Fixed:

1. `clean` strips tag characters U+E0000–U+E007F and turns U+2028 /
   U+2029 into line ends (test).
2. `Related::of` shows `project_path` (the worktree), not `repo_path`.
3. The worker paragraph is only for sessions the overlord launched with
   background (kv `context_on`); late background gets the header without
   it, in the file and in hook answers (`render_as`) (test).
4. A Pointer-mode session restarted into a new conversation gets the
   pointer again as its first prompt (`context::restart_prompt`,
   `Agent::add_first_prompt`; rows marked seen) (tests).
5. A replaced, unacknowledged answer stays acknowledgeable for its own
   rows (up to 8 kept); acknowledging the newer one marks the union and
   retires the older ones (test).
6. `private::write` calls `sync_all` before the rename.
7. `context::needs_background(params)` is the one test, used by
   `session.new` and by the daemon deciding to ask `hooks_work`.
8. `hook_reply` (the source-less wrapper) is `#[cfg(test)]`.
9. `propose_session` stores the resolved ids for `related`, with
   `related_names` for the form's preview.

### Review round 6 (PR #64)

1. OpenCode reading the pointer's file: OpenCode's permission docs
   (opencode.ai/docs/permissions, checked 2026-11-16 against the
   installed 2.2.7) say `external_directory` governs paths outside the
   project, applies to `read`, and "The default policy allows
   `external_directory`, so external paths are not prompted unless you
   add a rule". The config schema gives it no other default. So the
   pointer works unasked on a default setup; no live check needed, and
   neither (a) inline background nor (b) a copy in the session folder.
   A user's own `external_directory: ask/deny` rule would make OpenCode
   ask or refuse: their choice, left alone (we never write their
   config).
2. `context.md` is rewritten at every restart and on a rename of the
   session, as well as when its rows change.
3. Lost acks: at most two unacknowledged full deliveries per
   conversation (`FULL_TRIES`, kv `context_full_tries:<id>`); then it
   counts as reached, logged once (`context_assumed_delivered`) (test).
4. Pointer-mode restart marks rows seen only after the pane is
   respawned or created.
5. `private.rs` test no longer asserts the parent folder's mode (umask).
6. `split_legacy` removes only the old instruction (to its last words,
   else its sentence), keeping text before and after it in the paragraph
   (test with no blank line).
7. `set_mode` lost its `sent` flag: every mode but `Hook` sends at launch
   (the pointer goes alone when there are no words); `launch_prompt`
   returns just the prompt.
8. `send_hook_ack` uses `client_call_within` (`client_call` with an
   optional timeout).
9. `overlord.md` keeps one line ("follow what its answer says about
   delivery"); the values and advice are in the `add_context` schema.

Declined (coordinator): `frame_message` (planned for PR 1); the
test-only wrapper (already `#[cfg(test)]`).

## Owed / deferred

- `context::split_legacy` (old-style prompts with a `## Context from the
  overlord` section) is for overlords still on old instructions. Remove it
  a few releases after PR 1 ships.
- Follow-up, not in this effort: the guard rail above.
- The first prompt (with the pointer line for Codex untrusted / OpenCode)
  is still on argv, `launched_args`, the `launched` event and
  `bind_prompt`: PR 3.
- `get_session` (a read-only tool) shows any session's background to any
  session that added plain `rlord mcp`, like its notes. PR 2's scoped
  tools are the real surface for workers.
- Reconcile at PR 4: Codex daemon-mode `turn/start`, an overlord token.
