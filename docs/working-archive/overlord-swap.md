# Overlord swap and usage-limit resume: owed items

**Status:** shipped (0.23.0-dev.1, follow-ups in 0.23.0-dev.2). What lasts
is in `docs/architecture.md` ("Usage limits", "One per agent, one active")
and `docs/guide.md`. The working doc is in git history.

## Owed

- **Codex limit detection.** No rollout on the dev machine ever hit a
  limit, so its record's shape is unverified. Likely signal: `token_count`
  `rate_limits.rate_limit_reached_type` non-null, `resets_at` epoch
  seconds. Needs one real sample.
- **OpenCode limit detection.**
- **The swap key on the key screen** (`keys_list` / `:keys`): it is an
  overlay key, not a tmux binding, and the screen's live test goes
  through tmux.
- **Cancelling a scheduled resume.** Today only using the session ends a
  job.
- **`status:limited` as a list filter.**
- **`recall` treats the active overlord as the caller.** `rlord mcp`
  knows the calling session's id (`agents::MCP_ENV`, only logged);
  passing it through would make a background overlord's `recall` exact.
- **rlord's prompts without text.** rlord's pastes are marked at the
  source (`daemon/src/own.rs`, "rlord's own prompts" in
  `docs/architecture.md`). Left:
  - Turns found in a transcript (Codex without hooks) and OpenCode's
    session row (events missed) match by time only: a user prompt in
    the 30s after a paste can read as rlord's, and a paste the agent
    queues for more than 30s reads as the user's. Reading the prompt's
    text from the transcript or OpenCode's store would close it.
  - Whether Claude Code's and Codex's UserPromptSubmit `prompt` is the
    expanded text for a large paste was inferred (Claude's transcripts
    hold it expanded), not observed on a live hook; so was OpenCode's
    `item.payload.text` (read from the 2.2.7 schema). One live sample
    of each would settle it; a 5s time match covers a miss meanwhile.
  - A marker whose prompt never came matches the user typing the very
    same text within 10 minutes. A paste that lands in a draft doesn't
    match by hash and counts as the user's.
- No test drives the "job rescheduled while it runs" race end to end;
  the guard (`limit_at`/`asked_at` compare) is small and reviewed only.

Done since: rlord's pastes no longer count as the user's prompts
(0.23.0-dev.5); the `M-s`/`m-s` overlap check compares modifiers
case-insensitively; `continue_elsewhere` starts the target overlord once.
