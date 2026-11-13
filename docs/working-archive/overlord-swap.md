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
- No test drives the "job rescheduled while it runs" race end to end;
  the guard (`limit_at`/`asked_at` compare) is small and reviewed only.

Done since: rlord's pastes no longer count as the user's prompts
(0.23.0-dev.5; what that still owes is in `docs/working/own-prompts.md`); the `M-s`/`m-s` overlap check compares modifiers
case-insensitively; `continue_elsewhere` starts the target overlord once.
