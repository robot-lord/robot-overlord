# rlord's own prompts: residue

Shipped in 0.23.0-dev.5: rlord's pastes into a session (`send_to_session`,
a held `when_idle` message, a resume after a usage limit) are marked at
the source and don't count as the user's prompts. How it works is in
`docs/architecture.md` ("rlord's own prompts") and
`crates/daemon/src/own.rs`. The full working doc is in git history
(`docs/working/own-prompts.md`, removed 2026-11-14).

## Owed

- **One live sample of a large paste's hook payload**, Claude Code and
  Codex: is `prompt` the expanded text or the placeholder? Inferred so
  far (Claude's transcripts hold the expanded text). A 5s time match
  covers a miss for long or multi-line pastes.
- **One live sample of OpenCode's `session.inbox.enqueued`**:
  `item.payload.text` was read from the 2.2.7 schema, not seen on the
  stream.
- **Text-less reports match by time only**: a turn found in a transcript
  (Codex without hooks) and OpenCode's session row (events missed). A
  user prompt in the 30s after a paste can read as rlord's, and a paste
  the agent queues for more than 30s reads as the user's. Reading the
  prompt's text from the transcript or OpenCode's store would close it.
- **A marker whose prompt never came** matches the user typing the very
  same text within 10 minutes.
- **A paste that lands in a draft** doesn't match by hash and counts as
  the user's. (`type_into` holds off on a draft where it can read one:
  Claude's, Codex's and, since 0.23.0-dev.19, OpenCode's.)
