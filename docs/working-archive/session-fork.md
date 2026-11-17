# Session forking: residue

How forking works is in `docs/architecture.md` ("Forking a session") and
`docs/guide.md` ("Forking a session"). The full working doc is in git
history (`docs/working/session-fork.md`, removed 2026-11-16).

## Owed

- **Seeded fallback for agents with no native fork** (Gemini, any later
  adapter). Give `agents::Fork` a variant for it and have
  `daemon::fork::launch` handle it. Design: an ordinary new conversation
  (`agent.launch`) with everything else a fork gets (lineage, copies, the
  fork note, the link to the parent), plus, by plain code, the parent's
  history: once session-context PR 2 lands, a note telling it to read its
  linked parent with `read_history` / `session_summary`; for agents that
  can't run those tools, `<data>/sessions/<fork id>/parent.md` (0600,
  `rlord_core::private`) holding the parent's archive summary and the
  tail of its conversation as text (tool output clipped, 64,000 chars at
  most), named in the fork note. The same mode would later serve an
  OpenCode fork into a worktree and a fork onto another agent.
- **Live check: a pane fork of a daemon-hosted Codex parent.** Needs
  Codex's shared background server; run it with a scratch `CODEX_HOME`
  (as the daemon-mode check in `codex-daemon.md` did). If it fails,
  refuse forking a daemon-mode parent while its turn runs.
- **Fork at an earlier message**: OpenCode's fork call takes `before`
  (a message id), Codex's `thread/fork` `lastTurnId`.
- **Cross-agent fork**: the seeded fallback above, onto another agent.
- **Several hookless Codex forks at once in one folder** (deferred at
  the last review; each case needs that, and the first hook rebinds
  correctly):
  - forks of one parent matched by creation time
    (`discover::pick_thread`) can swap threads until their first hooks
    rebind them;
  - `tracked` (threads already taken) is read once per `discover` pass:
    a thread bound elsewhere during the pass isn't seen until the next;
  - the 15s grace for an unreadable rollout can let an ordinary session
    take a slow fork's thread.

## Open questions

- **Daemon-mode Codex forks** through the app-server's `thread/fork`
  (`ThreadForkParams`: `threadId`, `cwd`, `model`, `lastTurnId`; the
  answer's `thread.id`, `thread.forkedFromId`)? rlord's client only reads
  and interrupts today, a thread made over the socket has shown "no
  rollout found" on `codex resume` until its first turn, and it would put
  the fork on the daemon. Until then a user fork always runs in the pane.
