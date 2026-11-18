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

**No legacy split.** The plan's `split_legacy` (old combined prompts)
was dropped in review round 7: `overlord.md` is rewritten at every
overlord start, and the user wants no compatibility code.

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
2. **1.3.0: pull** (shipped in this order after 1.2.0): token,
   `session_tools.rs`, `sctx.*`, `rlord mcp --session`, `rlord context`,
   wiring at every start, pane env.
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
- [x] `context.rs`: render, files, `split_legacy` (removed in round 7), `frame_message` (the
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
- **Bare launch with background:** a Pointer-mode session (Codex with
  untrusted hooks, OpenCode) launched with no prompt gets the pointer
  alone as its first prompt (`context::pointer_only`, since review
  round 3): the background is in `<path>`, read it, then wait for the
  user's request.
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

### Review round 7 (PR #64)

1. The give-up path marks every row seen too, and the retry cap covers
   every answer, whole or update (`UNACKED_TRIES` = 2 in a row per
   conversation, kv `context_unacked:<id>`, reset by an ack) (test).
2. A restart marks rows seen in every mode but Hook when the start
   carries the background (as `set_mode`).
3. The background file is best effort at launch and restart: written
   before the agent starts (`context::launch_file_text`), a failure is
   logged and the start carries no flag or pointer naming the missing
   file; its rows then stay unseen (`context::record_mode`) (test that
   the launch file matches the stored rows' render).
4. `private::write` no longer fsyncs (context.md, the Codex gate are
   regenerable); `private::write_synced` is the opt-in.
5. `hook.ack` is mutating (`changed()` fires).
6. `Db::context_mark_all_seen` (one `UPDATE … WHERE seen_at IS NULL`),
   used by `set_mode`, the empty update, the give-up and restart.
7. `methods::agent_param` is the one agent choice: `session_new`,
   `new_params_on_codex_daemon`, the dispatcher's `hooks_work`.
8. Legacy machinery removed (`split_legacy`, `without_worker_line`,
   `legacy_notes`, `first_prompt` and their tests): `overlord.md` is
   compiled in and rewritten at every overlord start, and the user wants
   no compatibility code for old state. `by_overlord` stays: it decides
   the background and the worker paragraph.
9. `autostart::log_place` reports whether the log folder is rlord's own.
10. `compose` renders note lines once and hands them to `notes_section`;
    `update_session` adds background first (one `context::check`, in
    `session.context_add`), so a refusal still writes nothing.

### Review round 8 (PR #64)

A. Ack tracking redone as columns: `session_context.sent_ack` /
   `sent_at` (migration 0010 edited in place, unreleased). An answer
   stamps the rows it carries; `ack` marks seen the rows still stamped
   with its id (`Db::context_ack`), plus the conversation (kv
   `context_full_sent:<id>` for the latest whole answer). The ack id
   names its session (`<session>.<id>`). Give-up marks seen only rows
   some answer carried (`Db::context_mark_sent_seen`). Gone: the
   `context_ack:*` / `context_ack_for:*` blobs and the `older` list.
   Kept: the unacked counter. Tests: at most one duplicate, give-up,
   replaced answer (its late ack marks nothing; the newer one marks
   all), a note added after the give-up still goes.
2. `session.context_add`: the file write after storing is best effort
   (test: blocked folder, note stored).
3. A restart marks rows seen only when it carried something fresh (a
   pointer sent again, or the system prompt file rewritten this time).
4. Restart into a new conversation re-checks `hooks_work` (the daemon,
   before the lock, for sessions in Hook mode); gone: SystemPromptFile
   for Claude, Pointer for the others, stored.
5. `update_session` validates the background first, makes the other
   edits, then adds it (passing the checked note and ids, not checked
   again). A note the same as an unseen one or the latest one is not
   added again (`Db::context_add`).
6. A rename also rewrites `context.md` of the sessions that list it as
   related (`Db::context_linkers`), best effort.
8. Bare launches: corrected here and in the PR description (they get
   `pointer_only`).
9. `tmp_beside` is pid + counter.
10. `context_exists_or` split back: `Db::context_exists` plus the
    `context_on` kv in `has_background`.

### Review round 9 (PR #64)

1. The give-up never swallows a reset or a new conversation: SessionStart
   `startup` / `clear` / `compact`, and a conversation the counter isn't
   about, always get the whole background and reset the count. A note no
   answer carried yet goes in the answer that gives up (tests).
2. `later_delivery` goes by the stored mode only: Hook `next_prompt`,
   SystemPromptFile `next_start`, else `file_only` (test).
3. `Daemon::restart_hooks` computes `hooks_work` before the DB lock for
   every restart path that can need it: the RPC, `ui.goto`, Remote
   Control. Overlords (`overlord.rs`, `recall.rs`) have no background.
4. A Pointer restart that sends `pointer_only` resets `bind_since` /
   `bind_prompt` to that start and prompt (`note_first_prompt`, shared
   with the launch) (test).
5. The hook answer is built at the very end of `hook_emit`, after all
   the event's fallible work.
6. `checked` is no longer accepted over RPC: `update_session` stores the
   checked values in-process (`methods::context_store`).
7. Hook answers name `context.md` only if it exists.
8. `needs_background` decides on the cleaned `context`.
9. `Db::context_ack` filters by session (from the ack id) and
   `Db::context_mark_sent` is one statement.
10. A restart calls `agent.resume` once, then adds the flag or the
    pointer to that command.

### Review round 10 (PR #64)

1. `update_session` refuses background for the overlord's own row
   before checking or storing (`refuse_overlord`, also in
   `context_store`); `hook_reply_from` returns None for overlord-role
   sessions as a backstop (tests).
2. A session's first background after launch records a mode
   (`context_store`: Hook if hooks work, from
   `Daemon::first_background_hooks` outside the lock, else the system
   prompt file for Claude or the pointer); `later_delivery(None)` counts
   as Hook, as `hook_reply_from` does. Clears the owed item about sessions
   with no stored mode (tests).
3. `hook_emit`'s answer returns at once for events other than
   SessionStart / UserPromptSubmit.
4. `ui.goto` asks `restart_hooks` only when it will restart the session.
5. `architecture.md`: `hooks_work` goes to `session.new` and
   `session.restart` (and to a first `session.context_add`).
6. A restart's fallback mode (and a pointer's binding reset) is stored
   only after the pane was respawned or created.
7. `Db::context_mark_seen` and `private::write_synced` are
   `#[cfg(test)]`.
8. `RELATED_MAX` counts distinct resolved sessions, without the session
   itself (test).

## PR 2 progress (1.3.0)

- [x] Token (`session_tools::mint` / `revoke` / `resolve`): 32 bytes
  from `/dev/urandom`, hex; kv `session_token:<id>` and 0600
  `<data>/sessions/<id>/token`; kept across restarts, removed on
  archive, made again at the next start. Constant-time compare.
  Overlords (and sessions in an overlord's folder) get none and are
  refused; archived sessions are refused.
- [x] `session_tools.rs`: `session_brief`, `related_sessions`,
  `session_summary`, `search_history`, `read_history`, with the plan's
  limits; rate limit 30/min, 300/h in memory; answers capped at 16,000
  chars (a longer one comes back clipped as text).
- [x] `search::search_in` (id filter, nothing outside rlord) and
  `search::read_with` (budget); only `session_tools` calls them.
- [x] RPC `sctx.tools {token}` / `sctx.call {token, name, arguments}`,
  a dispatch of its own in `call_ui`.
- [x] `rlord mcp --session --token-file <p> [--socket]` (server
  `rlord_session`, conflicts with `--overlord`, never autostarts) and
  `rlord context [--token-file] [--socket] <tool|help> key=value…`.
- [x] Wiring per agent at every start; pane env `RLORD_SESSION_LOOKUP_FILE`
  (not for Codex daemon mode).
- [x] Background "Looking up more" section.
- [x] `overlord.md` (link rather than copy), `docs/architecture.md` (MCP
  server: three modes).
- Live check V5 (Codex `-c mcp_servers.rlord_session` in a worker): not
  run. Until it passes, Codex sessions aren't wired
  (`session_tools::CODEX_MCP = false`, review round 5): they get
  `lookup` `cli` and the overlord copies the facts they need. Owed: run
  V5, then turn the flag on.

### Decisions made in PR 2 (beyond the plan)

- **Wiring not inside `spec_args`.** `spec_args(agent, spec)` has no
  session id, and the token must exist before the background file is
  rendered. `session_tools::wiring(db, base, id, agent, role, daemon)`
  is called next to `spec_args` at the three starts (`session_new`,
  `session_restart`, `fork::finish`), before the file is written.
  Same effect as the plan: computed at every start, never stored in
  `extra_args`, so old sessions get it at their next restart.
  `session_fresh` (overlords only) gets none.
- **Claude flags in `=` form** (`--mcp-config=<p>`,
  `--allowedTools=mcp__rlord_session`): both are variadic, and the
  first prompt may follow.
- **Visible "linked"** = background links either way (it links them, or
  they link it), its fork parent and its forks. "Same group" = a shared
  tag. Sessions that ran in an overlord's folder are left out like
  overlords.
- **Lookup section only once the token exists.** A session started
  before 1.3.0 gets no section (and no tools) until its next start; the
  plan's "minted when context is first rendered" isn't done: rendering
  stays free of writes.
- **`read_history`** takes a hit's `transcript` (review round 2), and
  `search::read_with` refuses any file that isn't that session's.
- **`session_brief`'s first prompt**: kv `bind_prompt`, else the launch
  spec's `prompt`, else the transcript's first prompt. Includes
  `forked_from` (owed from forks).
- **Token not minted for a launch that fails** after `wiring`: no undo;
  the kv row and file stay until the id is reused (never) or archived.
  Harmless (no session row resolves it).

### Review round 2 (PR #70)

1. `read_history` passes a hit's `transcript` through (test: a hit
   before a `/clear`; a foreign path refused).
2. `RLORD_SESSION_LOOKUP_FILE` is scrubbed wherever `RLORD_SESSION_ID`
   is (`agents::PANE_ENV` / `scrub_pane_env`): the Codex-daemon pane's
   unset list, autostart, `process::run`, OpenCode's shared-server
   environment and `run_bounded`, Codex's server commands, install's
   probes (tests: pane command, OpenCode config).
3. `rlord context`: only `line`, `before`, `after`, `limit` are numbers.
4. Refs resolve only within the visible set: id, exact name, unique
   name prefix (test).
5. A failed worktree add removes only its own worktree (`worktree remove
   --force` if git registered it) and the branch; no repo-wide prune.
6. `lookup` (`mcp` / `cli` / `none`, `session_tools::lookup_kind`) in
   `get_session` and the create and fork answers. `overlord.md`: link
   for `mcp`; for `cli` / `none` also copy the few facts needed into
   `context`. The CLI isn't relied on inside Codex's sandbox.
7. `wiring` refuses an overlord's folder too, before making a token.
8. `base` is passed to `lookup_section` (now the session folder),
   `pane_env`, the file renderers (`Start.base`); no `data_dir()` there.
9. `visible()` once per call, passed down.

Not changed: version stays 1.3.0 (coordinator: feature PRs are stable
minor releases now).

### Review round 3 (PR #70)

1. A failed worktree add removes only a registration at `dir` and a
   branch that weren't there before the call (test: the user's
   "missing but already registered" worktree survives).
2. No `RLORD_SESSION_LOOKUP_FILE` in OpenCode or Codex daemon-mode panes
   (shared servers run their commands); architecture.md fixed.
3. The background's CLI line always passes `--token-file` and
   `--socket`.
4. `pick` also takes a unique id prefix, after the exact id and name.
5. `worktree_registered` compares paths with symlinks resolved, also for
   a missing folder (nearest existing ancestor resolved) (test).
6. `render_as` leaves the lookup section out when it doesn't fit with
   the header; never over budget for it (test).
7. `scrub_pane_env` in autostart, install and Codex's server commands.
8. One `overlord::overlords_at`, used by `visible`, `resolve`,
   `lookup_kind`, `wiring` and `search`.

### Review round 4 (PR #70)

1. Every pane unsets the `PANE_ENV` names it isn't given (`env -u`);
   `rlord daemon` drops them from its own environment at start
   (`scrub_own_env`) (tests).
2. `lookup` is `mcp` only when the last start's wiring produced flags
   (kv `session_lookup:<id>`), else `cli` (test).
3. `render_as` renders the rest, then adds the lookup section only if
   the whole stays in budget; `compose` leaves out a notes section that
   can't fit. Test over every budget from the header up.
4. `rlord mcp --token-file` requires `--session`.
5. The rate limiter is per `Daemon` (`session_tools::Limiter`), drops
   idle ids; tests use their own.
6. `methods::start_args` (spec args plus wiring) for every start path:
   new, restart, fork, fresh. (`limits` resumes through restart.)
7. Token bytes from two v4 uuids.
8. `is_overlords` gone; `overlords_at` imported at the top; fork test
   block un-nested.

### Review round 5 (PR #70)

1. Deadlock: `mcp::lookup_of` took the DB lock twice (`get_session`,
   `create_session`, `fork_session`); one lock now. Test calls
   `get_session` through `mcp::call` with a timeout (create and fork
   launch an agent in tmux, so aren't run in tests; they share
   `lookup_of`). No other double lock found in this PR's code.
2. `search_history` leaves out the calling session (test).
3. `capped` clips long strings, then halves long lists: valid JSON,
   never cut text (test).
4. A launch that fails in `session_new_with` removes its token, MCP
   config and kv keys (`session_tools::forget`, test).
5. `tmux_cmd` runs `scrub_pane_env`.
6. The CLI line shell-quotes its paths (test).
7. `render_as`: the content gets the whole budget; the lookup section is
   added only if it still fits.
8. Codex MCP wiring off (`CODEX_MCP`), see V5 above.
9. `TOKEN_ENV` in core beside `PANE_ENV`; `main.rs` uses it and
   `SERVER`; `lookup_section` uses `token_in`.

### #65 owed items

Fixed here:
- A failed `git worktree add -b` no longer leaves the branch behind
  (`git::worktree_add`: its own worktree removed if registered, then
  `branch -D`; test).
- Claude's `conversation_on_disk` also checks where Claude files the id
  when the stored `transcript_source` is stale.
- Cleanups: `FORK_MINTS_ID` and `FORK_ELSEWHERE` removed (their reasons
  moved to `Agent::fork` / `forks_elsewhere`); `rollout_forked_from` is
  now a test helper over `rollout_first_line`.

Still owed:
- Binding: several hookless Codex forks of one parent can bind each
  other's thread (oldest-first match, the `mine` filter does nothing);
  `Whose::Bind` skips `record_bind`'s duplicate check; a waiting fork
  can take the thread of a hand-made `codex fork` in the same folder.
  Binding logic, needs its own careful change and tests.
- An auto-named worktree fork with an explicit branch is refused when
  the default name is taken meanwhile.
- TUI `load_forks`: cache keyed on (id, count), so a fork rename doesn't
  refresh it; and its `session.get` blocks the UI thread.
- `related` refs resolved twice in `fork::want` (filter, then
  `context::check`).
- The demo OpenCode stand-in embeds Python (shell-is-glue rule).

## PR 3 progress (1.4.0)

- [x] `daemon/src/first_prompt.rs`: `Way { Argv, Stdin, Server, Pointer }`
  per agent; `place` writes the first prompt (user's words, with the
  background pointer line when there is one) to 0600
  `<data>/sessions/<id>/prompt.md`. Used by `session_new`, `fork::finish`
  and a re-fork's restart. Fixed text with no words
  (`context::pointer_only`) stays on argv. A file that can't be written
  leaves the prompt on argv (logged).
- [x] Codex: argv carries `first_prompt::pointer_line` (user's decision);
  that line is `bind_prompt`. `session_brief`'s first prompt now reads
  the launch spec's words before `bind_prompt`.
- [x] OpenCode: `Service::prompt` (`POST /api/session/<id>/prompt`,
  `{text}`; route and body read from the installed 2.2.7 binary's API
  table, not run live). kv `first_prompt:<id>`;
  `opencode::apply_first_prompt` posts once the session exists and no
  model `Deferred` is pending (or it ran out). Failed POST, or no session
  within 30s: queued with `send::queue` (typed at the first idle turn).
  Event `first_prompt` either way; the file is removed after.
- [x] Claude: stdin (`exec … < prompt.md`, `first_prompt::with_stdin`)
  wired but off (`CLAUDE_STDIN = false`): V1 was not run in this PR (the
  live check was refused by the agent's permission gate). Claude's
  prompt stays on argv until V1 passes.
- [x] #70 owed: the pane env var is `RLORD_SESSION_LOOKUP_FILE` (Codex's
  default env policy drops `*TOKEN*`, `*KEY*`, `*SECRET*`; test); the
  OpenCode module doc no longer says the prompt rides on argv.
- Not done from the plan: a `FirstPrompt::Typed` mode for Claude (the
  queue fallback covers OpenCode only), `discover` changes (none needed:
  Codex is still launched with a prompt).

Owed from PR 3:
- Run V1 (Claude interactive with the prompt file on stdin, also with
  `--resume … --fork-session`), then set `CLAUDE_STDIN = true`.
- Live-check the OpenCode POST (V4) on a scratch server: that the TUI
  opened with `-s` shows the posted turn, and that the 30s fallback is
  long enough.
- Live-check that Codex reads `prompt.md` from the pointer line in its
  default sandbox (the plan's Codex pointer-read check).
- (Fixed in round 2) The typed fallback's prompt was marked rlord's, not
  the user's.
- `launch_spec.prompt` still holds the words in the DB (not argv).
- (Review round 1) A Pointer-mode session's background rows are marked
  seen at launch, before an OpenCode POST has gone (`methods.rs` start).
- (Review round 1) The POST runs on the OpenCode follower thread; a slow
  server delays that tick's event reading (10s limit).
- (Review round 1) `apply_first_prompt` and `apply_deferred` share their
  shape (held kv, late clock, service lookup): fold into one; `Placed`
  could be an enum (exactly one of argv / stdin / server).

### Review round 1 (PR #73)

1. `Service::prompt` returns `Posted { Sent, Refused, Unknown }`: only
   no connection or an error status falls back to typing; a request sent
   with no answer back is logged as unknown, never typed again.
2. A restart keeps a held first prompt (fresh clock, set once the pane
   is up) instead of deleting it.
3. The typed fallback has its own slot (`send::FIRST_KEY`,
   `queue_first`): delivered before the overlord's queued messages, never
   replaced or dropped by them; a direct send waits while it is there.
4. `first_prompt::task` / `launch_words`: triage, archive summaries and
   `session_brief` show the launch's words where a Codex transcript's
   first prompt is the pointer line. (`recall` reads overlords' own
   transcripts, which never get one.)
5. A failed `session.new` removes `prompt.md` and kv `first_prompt`.
6. Held first prompts of sessions no longer live are settled each
   OpenCode tick: logged as not sent, file removed.
7. The pointer line starts `[rlord session <id>]`.

### Review round 2 (PR #73, merged main at 1.5.0; now 1.6.0)

1. The overlord's sends (direct and queued) wait while kv
   `first_prompt:<id>` is held too, not only the typed slot.
2. `Service::prompt`: a failure to connect or to write the whole request
   is `Refused`; only a missing answer after that is `Unknown`.
3. A typed first prompt that fails is tried again at later ticks (up to
   `FIRST_TRIES` = 5), then dropped and logged.
4. Archive summaries read the launch words before reading the transcript
   (the DB lock is let go first).
5. `first_prompt::task` replaces only session `id`'s own pointer line.
6. A typed first prompt is the user's prompt (`type_message` source
   None), not marked rlord's.

Owed (round 2): the POST (10s limit) on the follower thread; each pass
reads the file, asks `has_session` and scans kv `first_prompt:*`; the
private-write helper is repeated in `context` and `first_prompt`; the
restart's "has words" check repeats `fork::restart_prompt`'s.

### Review round 3 (PR #73)

1. The stopped-session sweep reads each held id's own row under the same
   DB lock as the kv: a session that just started is never touched.
2. A stopped session keeps its held first prompt (file and kv, marked
   `stopped`, logged once) for its restart (`restamp` clears the mark);
   only an archived or gone one is settled.
3. `first_prompt::task` also drops another session's full pointer line
   (a Codex fork's history starts with its parent's): the fork's own
   words, or nothing; never a path.

Owed (round 3): the POST on the follower thread; per-pass file read,
`has_session` and kv scan; fold `apply_first_prompt` / `apply_deferred`;
a held typed first prompt logs `message_held` (not a first-prompt kind);
`tick` checks kv `first_prompt:<id>` inline rather than via
`first_waiting`.

### Review round 4 (PR #73)

1. A first prompt queued for typing waits through a stop for the
   restart; only an archived or gone session drops it.
2. After the POST, kv and file are settled only if the hold still names
   the same file with the same words (under the DB lock).
3. A POSTed first prompt holds the overlord's messages for 15s
   (`first_prompt::mark_sent` / `waiting`, kv `first_prompt_sent:<id>`),
   used by `send` and `tick` alike.
4. `launch_words` falls back to a fork's parent's (up to 4 levels).
5. A restart with a held first prompt (it carries the pointer) puts no
   new pointer on argv.
6. Docs say `RLORD_SESSION_LOOKUP_FILE` throughout.

Owed (round 4): `apply_first_prompt` / `apply_deferred` dedupe; the dead
`Way::Argv` arm in `place`; triage reads `launch_words` under the DB
lock (one query per row).

### Review round 5 (PR #73)

1. "First prompt goes first" lives in `send::type_message` (rlord's own
   texts wait; the first prompt itself is exempt). `tick` no longer
   skips messages: one for a stopped or failed session is dropped as
   before; one that must wait is held (logged once).
2. A POST that went out while the hold changed still marks the prompt
   sent (`mark_sent`), so it isn't POSTed again.
3. A session whose pane is gone (stopped or in error) has its held first
   prompt marked `stopped`, and `waiting` ignores such a hold.

Owed (round 5): the POST's 10s timeout (the route admits asynchronously
per the binary's description, so it should answer quickly; unverified
live); `message_held` as the kind for a held first prompt; triage cost;
`apply_*` dedupe; the restart's words check repeating
`fork::restart_prompt`; the dead `Way::Argv` arm.

### Review round 6 (PR #73, merged main at 1.7.0; now 1.8.0)

1. Only a 4xx answer to the POST is `Refused`; 5xx and anything else
   is `Unknown` (never typed again).
2. `Unknown` sets the sent marker too (15s settle).
3. A typed first prompt sets the sent marker, so nothing rlord types
   lands right after it.
4. Archive removes `prompt.md`, the held POST, the sent marker and a
   typed-fallback entry (`first_prompt::forget`); the sent marker is
   deleted once its window is over. Declined: removing Codex's file on
   binding, since Codex binds (SessionStart, the thread index) before
   it has read the file. Owed: remove it after its first turn ends.
5. `session_brief`'s transcript fallback goes through
   `first_prompt::task`.

Owed (round 6): recovery when a `stopped` mark is left on a session that
runs again without a restart through rlord; the restart's words check
and the never-taken server arm on restart; `Placed` as an enum; the POST
on the follower thread.

## Owed / deferred

- `session_tools::visible()` is computed once per call for every tool;
  make it lazy per tool (`session_brief` needs only links, a named ref
  could be checked without the whole set).
- Token resolve scans every `session_token:*` row: add a reverse key
  for an O(1) look-up.
- Cache `methods::rlord_bin()` (resolved at every start and render).
- `search_in` reloads every session and `visible()` loads them all
  again: share one load per call.

- Forks (`session.fork`, 0.23.0-dev.29, landed between PR 1 and PR 2;
  `docs/architecture.md` "Forking a session"): a fork starts through
  `methods::start` like a new session, so change that one path.
  - PR 2 (done): a fork gets its token and tools at its start; its
    parent and forks are linked in the visible set; `session_brief`
    says `forked_from`. Still owed: the seeded fallback for agents with
    no native fork, built on these tools
    (`docs/working-archive/session-fork.md`).
  - PR 3 (prompts off argv): apply `FirstPrompt` in `methods::start`, so
    a fork's prompt leaves argv with everyone's: Claude's stdin file with
    `--resume … --fork-session` (extend V1 to that argv), Codex's pointer
    to `prompt.md` as `codex fork`'s `[PROMPT]`, OpenCode's POST to the
    fork's id. `bind_prompt` doesn't apply to forks (`bind_fork`).
- Follow-up, not in this effort: the guard rail above.
- The first prompt left argv in PR 3 for Codex and OpenCode; Claude's
  waits on V1 (see "PR 3 progress").
- `get_session` (a read-only tool) shows any session's background to any
  session that added plain `rlord mcp`, like its notes. PR 2's scoped
  tools are the real surface for workers.
- Reconcile at PR 4: Codex daemon-mode `turn/start`, an overlord token.
- Edge cases left (review round 9):
  - A running agent whose hooks were installed after it started gets
    `file_only` reported for later background until its next start
    (`later_delivery` goes by the stored mode only).
  - An older answer's late ack marks nothing when a newer answer carried
    the same rows again (the newer one's ack does): one duplicate at
    most, as designed.
  - `context.md` of sessions that link to one are refreshed on a rename,
    not on the linked session's status changes (only at their restart).
