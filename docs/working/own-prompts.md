**Status:** in progress

# rlord's own prompts

The fix shipped in 0.23.0-dev.5: rlord's pastes into a session
(`send_to_session`, a held `when_idle` message, a resume after a usage
limit) are marked at the source and no longer count as the user's
prompts. How it works is in `docs/architecture.md` ("rlord's own
prompts") and `crates/daemon/src/own.rs`. This doc holds what is still
owed; it goes away once those are done or dropped.

## Decisions

- `overlord ask` is not marked: its text is what the user typed, so its
  prompt is the user's (review of #23).
- "Updated" (activity sort, last-message time, `triage`) counts rlord's
  prompts; the "last prompt" sort is the user's own only. A session
  therefore shows as changed in `triage` right after the overlord sends
  to it.
- Prompts stay out of the daemon and the hook spool: only an unsalted
  hash and a "looks pasted" flag go. Local socket and spool only, owned
  by the user who owns the transcripts.
- `sent_hit` stays on resume jobs: it identifies an undated limit
  record, unrelated to who typed.

## Owed

- **One live sample of a large paste's hook payload**, Claude Code and
  Codex: is `prompt` the expanded text or the placeholder? Inferred so
  far (Claude's transcripts hold the expanded text). A 5s time match
  covers a miss for long or multi-line pastes.
- **One live sample of OpenCode's `session.inbox.enqueued`**:
  `item.payload.text` was read from the 2.2.7 schema, not seen on the
  stream.
- **Text-less reports match by time only**: a turn found in a
  transcript (Codex without hooks) and OpenCode's session row (events
  missed). A user prompt in the 30s after a paste can read as rlord's,
  and a paste the agent queues for more than 30s reads as the user's.
  Reading the prompt's text from the transcript or OpenCode's store
  would close it.
- **A marker whose prompt never came** matches the user typing the very
  same text within 10 minutes.
- **A paste that lands in a draft** doesn't match by hash and counts as
  the user's. (`type_into` holds off on a draft where it can read one;
  OpenCode's isn't read yet.)
- **Resume jobs in flight across the upgrade** from before 0.23.0-dev.5:
  `last_user_msg_at` may still hold the old daemon's paste time, so a
  job inside its 15-minute watch can end as a takeover. One-time.
