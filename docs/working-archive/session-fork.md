# Session forking: residue

Forking shipped in 0.23.0-dev.29; how it works is in
`docs/architecture.md` ("Forking a session") and `docs/guide.md`
("Forking a session"). The full working doc is in git history
(`docs/working/session-fork.md`, removed 2026-11-16).

## Owed

- **Seeded fallback for agents with no native fork** (Gemini, any later
  adapter). Give `agents::Fork` a variant for it (a `Seeded` arm was
  removed in review round 1 until an agent needs it), and have
  `daemon::fork::launch` handle it. Design: an ordinary new conversation (`agent.launch`)
  with everything else a fork gets (lineage, copies, the fork note, the
  link to the parent), plus, by plain code, the parent's history: once
  session-context PR 2 lands, a note telling it to read its linked parent
  with `read_history` / `session_summary`; for agents that can't run
  those tools, `<data>/sessions/<fork id>/parent.md` (0600,
  `rlord_core::private`) holding the parent's archive summary and the
  tail of its conversation as text (tool output clipped, 64,000 chars at
  most), named in the fork note. The same mode would later serve an
  OpenCode fork into a worktree and a fork onto another agent.
- **Daemon-mode Codex forks** through the app-server's `thread/fork`
  (`ThreadForkParams`: `threadId`, `cwd`, `model`, `lastTurnId`, …; the
  answer's `thread.id`, `thread.forkedFromId`). Not used now: rlord's
  client only reads and interrupts, a thread made over the socket has
  shown "no rollout found" on `codex resume` until its first turn, and it
  would put the fork on the daemon. A user fork always runs in the pane.
- **Live check F3, daemon-hosted parent**: not run. A pane fork of a
  parent whose thread runs on Codex's shared background server would
  need that server, which is the user's (started by their rlord, its
  hooks sent to it). Run it with a scratch `CODEX_HOME` (as the daemon
  mode check in `codex-daemon.md` did). If it fails, refuse forking a
  daemon-mode parent while its turn runs.
- **Fork at an earlier message**: OpenCode's fork call takes `before`
  (a message id), Codex's `thread/fork` `lastTurnId`.
- **Cross-agent fork**: the seeded fallback above, onto another agent.

## Accepted edge cases

- An OpenCode fork whose start fails after the fork call leaves that
  OpenCode session behind (a worktree fork's worktree and branch are
  removed).
- An unhooked Codex user fork of a session started while that session's
  archive summary runs: the summary's `--ephemeral` thread isn't in the
  index, so it can't be picked.

## Live checks (2026-11-16)

Scratch rlord (own XDG dirs, tmux socket `rlord-fork-live`,
`RLORD_NO_LOGIN_SHELL=1`, a stand-in `SHELL`), one short prompt each,
server killed after.

- F1 (Claude Code 2.1.326): `--resume <p> --fork-session --session-id
  <n>` started on `<n>` with the history, its hooks reported `<n>`, the
  transcript `<n>.jsonl` appeared only with the first message, the
  parent's was untouched.
- F2: the same from a new worktree of the parent's repo found the parent;
  the fork's transcript went under the new folder. That worktree of a
  trusted repo started without Claude's folder-trust question (a nested
  repo elsewhere did ask; left unanswered).
- F3 (codex-cli 0.169.0, parent in the pane): `codex fork <p>
  --no-daemon` showed "Thread forked from …" with the history; no
  `SessionStart` until its first prompt; bound from the thread index by
  `bind_fork` within a second, confirmed by its hook at the first prompt.
- F2b (review round 1, Claude Code 2.1.326, print mode in a temp repo,
  which skips the folder-trust question): a parent in `repo/sub`, forked
  from `repo/.worktrees/fk/sub`, quoted the parent's message; the fork's
  transcript went under the subfolder of the new worktree.
- F6 (review round 1, codex-cli 0.169.0, temp repo, scratch tmux socket,
  that folder trusted for the run only with `-c projects=…`): `codex fork`
  from a new worktree first asked "Working directory · fork", the
  parent's folder highlighted. Picking the current folder, and on a
  second run `-c tui.resume_cwd="current"` with no question, indexed the
  thread with the worktree as `cwd` (`source = cli`, rollout
  `forked_from_id` = the parent). So Codex worktree forks pass that
  override.
- F4 (OpenCode 2.2.7, its own server on another port): the fork call
  answered `{data: {id}}`; the row had `fork_session_id` = parent,
  `parent_id` NULL, not archived, the parent's messages copied; `opencode
  -s <fork>` opened it with its history.
