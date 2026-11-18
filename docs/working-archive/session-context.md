**Status:** shipped (residue)

# Session context: what is still owed

The effort (four PRs: #64 store and background, #70 per-session look-up
tools, #73 first prompts off argv, and the PR for 1.11.0, native later
messages) is reconciled into `docs/architecture.md` ("Session
background", "First prompt off argv", "MCP server", "Passing work to a
session") and `crates/daemon/src/overlord.md`. The full working doc is in
git history (`docs/working/session-context.md`).

## Live checks not run

- V1 (Claude's first prompt on stdin) on Linux and other Claude Code
  versions.
- Codex's Stop-hook claim on a daemon-mode session (hooks run in Codex's
  daemon); only the in-pane TUI was run.

Passed 2026-11-18 (Codex 0.169.0, OpenCode server with "Big Pickle"),
on scratch tmux sockets and a scratch rlord (own XDG dirs and tmux
socket):

- Codex reading `prompt.md` from the pointer line, default sandbox: it
  read the file and answered as told (also end to end from `rlord new`).
- Codex Stop-hook block: a `-c hooks.Stop=...` hook answering once with
  `decision: block`; the TUI shows "Blocked by hook" and the reason, and
  the agent answers it in the same turn. The second Stop carries
  `stop_hook_active: true`. `CODEX_STOP_CLAIM` is on. Testing note: a
  hook given by `-c` needs trust, given per run as
  `-c 'hooks.state={"/<session-flags>/config.toml:stop:0:0"={trusted_hash="sha256:..."}}'`
  (the hash from a stdio `codex app-server`'s `hooks/list`), and any
  `-c` makes Codex run embedded (no shared daemon).
- V5: an in-pane Codex session launched with
  `-c mcp_servers.rlord_session.*` called `session_brief` with no
  approval prompt. `CODEX_MCP` is on.
- V4: the OpenCode first-prompt POST shows as the first turn in the TUI.
- V6: a `when_idle` send mid-turn went `via: server` (`delivery:
  "queue"`) and ran as its own turn after the current one finished.
- Codex import: a Codex conversation started outside rlord was listed,
  imported, and `codex resume <id>` reopened that thread; rlord followed
  its status from the transcript. (The scan flags a conversation touched
  in the last minutes "maybe open elsewhere", even after its TUI exited.)

V3 passed (2026-11-17, Claude Code 2.1.327): a scratch tmux socket, a
Stop hook from a scratch `--settings` file answering once with
`{"decision": "block", "reason": "[rlord] … reply with the word
PELICAN"}`. Claude answered the prompt, showed the hook's reason, and
went on to answer `PELICAN` in the same turn. Claude labels a blocking
Stop hook "Stop hook reported a problem" in its UI: cosmetic, the agent
gets the text.

## Owed

- Later messages: the Stop sets the status idle as usual and the claim's
  ack sets it working again. If the ack is lost, the idle paste (after
  10s) or the next Stop delivers the message again: at most one
  duplicate, as designed. The grace is time-based (10s), not tied to
  the ack.
- Later messages: `deliver` types, then takes the parts it carried,
  without a lock `stop_claim` shares (hooks hold only the DB): a Stop
  in that window can claim the same parts again.
- Later messages: every Stop and prompt of a session does a kv delete
  (`stop_claims:<id>`) on the hook's hot path.
- Later messages, cleanups (review of #79): `stop_claim`'s status and
  archive checks repeat `refusal()`; `framed()` builds each part by
  stripping `frame_message`'s header (`strip_prefix`) rather than sharing
  a body helper; the pending kv map is parsed twice on some paths
  (`claim_out` then `pending_parts`, `stop_claim` then `first_waiting`).
- `limits::limited_in` reads kv `limits` beside the daemon's in-memory
  copy (`limits::limited`): share one helper. Remove kv
  `stop_claims:<id>` on archive.
- Later messages (review of #79): a TTL for the `own::mark` of a
  message OpenCode queued behind a long turn; dedupe `deliver`'s server
  path with `sent`; remove kv `stop_claims:<id>` on archive.
- Later messages: the OpenCode POST (10s limit) runs inside the
  `session.send` RPC, under the session's lock.
- First prompts: `apply_first_prompt` / `apply_deferred` share their
  shape (fold into one); `Placed` as an enum; the dead `Way::Argv` arm in
  `place`; the restart's "has words" check repeats
  `fork::restart_prompt`'s; `message_held` is the log kind for a held
  first prompt.
- First prompts: the OpenCode POST runs on the follower thread; each pass
  reads the file, asks `has_session` and scans kv `first_prompt:*`.
- First prompts: a restart with a prompt queued for typing (`FIRST_KEY`)
  but no held POST may put a pointer on argv as well; a `stopped` mark
  left on a session that runs again without an rlord restart; Codex's
  `prompt.md` stays until archive (remove after its first turn).
- `launch_spec.prompt` still holds the user's words in the DB.
- A Pointer-mode session's background rows are marked seen at launch,
  before an OpenCode POST has gone.
- Look-up tools: `session_tools::visible()` per call (make it lazy);
  token resolve scans every `session_token:*` row (add a reverse key);
  cache `methods::rlord_bin()`; `search_in` and `visible()` load every
  session twice.
- Forks: the seeded fallback for agents with no native fork
  (`session-fork.md`); hookless Codex forks of one parent can bind each
  other's thread; an auto-named worktree fork with an explicit branch is
  refused when the default name is taken meanwhile; TUI `load_forks`
  cache and its blocking `session.get`; `related` resolved twice in
  `fork::want`; the demo OpenCode stand-in embeds Python.
- Codex daemon mode: `turn/start` as a delivery path; an overlord token.
- The guard rail (refuse mutating `rlord` commands from inside a
  non-overlord session): a separate effort.
- Edge cases: a running agent whose hooks were installed after it started
  gets `file_only` for later background until its next start;
  `context.md` of linking sessions is refreshed on a rename, not on the
  linked session's status changes; `get_session` shows any session's
  background to any session with plain `rlord mcp`.
