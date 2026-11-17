**Status:** in progress

# Background-work status (`◔ bg N`) and status in words

A Claude Code session whose turn ended while async subagents or forked
skills it started still run shows `◔ bg N` instead of "your turn".
Background shells are listed only as information. Hovering a list row, or
the details card, spells out every symbol. How it works now:
"Background work" in `docs/architecture.md` (official; it wins over this
file). PR #68.

## Decisions

- Plain transcript reads only, no model calls (the user picked option 1).
- Only subagents and forked skills change the status. Background `Bash`
  (dev servers, watchers) is information only (the user's call, round 4).
- Finishes count only in shapes the harness writes: a `queued_command`
  attachment with `commandMode: "task-notification"`, or a `user` line
  with `origin.kind: "task-notification"`. `queue-operation` lines carry
  no origin, so they are ignored.
- Row fields are `async_tasks` / `async_labels` / `async_shells`, not
  `background_*`: `background` is the session's context. The overlord sees
  the status as `background work`.
- The hooks' `SubagentStop` closes an agent the tracker still holds open
  (round 6), so the `⚙` count and `bg` don't disagree.
- `/clear` (a new transcript, same process) carries the open work over.
- Status filters: the overlays and your turn match the shown status;
  `stopped`, `working`, `error` and the rest match the stored one.
- Declined (round 6): re-checking a stored status under the overlay. The
  overlay applies to `idle` only and the stored status is trusted.
- Declined (round 6): TaskOutput `block: true` as a finish. That tool was
  removed from Claude Code: the installed version's own help says "the
  TaskOutput tool was removed", and no transcript here has a TaskOutput
  call to take a shape from.
- Round 7: the file stamp is saved only once the whole file was read
  without an error. A task with no launch time is dropped on a restart.
  Each agent keeps the sidechain folder it was launched next to. Lists cap
  at 5 with "+N more". `triage` speaks the same status words.
- The stale guard (30 min) hides work; it never forgets it. Work is
  forgotten on a stop, or when a new agent process starts (an rlord
  resume, restart or refork event, or a `SessionStart` hook with source
  `startup` or `resume`).

## Owed

- **Codex and OpenCode adapters.** `Tracker::advance` is the hook. Neither
  agent has a known background-task record yet.
- **Needs-you reason.** The popup and details say "blocked on a permission
  prompt or a question". rlord doesn't store which; the hook payload
  (`PermissionRequest` tool, `AskUserQuestion`) could be kept for it.
- **Hover.** The popup is placed once when it opens on a row and stays
  there while the pointer moves within that row (round 6 reversed round
  5's follow-the-pointer, because of the motion flood).
- **Live hover check.** A scratch tmux socket confirmed that tmux records
  all-motion mode (`mouse_all_flag=1`) for a pane that asks for it, with
  mouse on. Motion events have not been watched reaching a live TUI.
  Mouse capture (1003) is on for the TUI's whole life; the popup shows
  only in list mode. There is no "popup for the selected row" fallback;
  the details card covers that.
- **`/clear`.** Whether `/clear` kills background tasks is not checked.
  It is treated as keeping the process (`clear` and `compact` don't reset).
- **KillShell.** Its result shape (`success`, `shell_id`) is assumed; no
  real one was seen (TaskStop's was).
- **A resume outside rlord before its first hook.** The first read after
  state was dropped re-reads the last 4 MB. Work from before the restart
  is dropped only once the `SessionStart` hook is recorded. Until then,
  old shells may show as information.
- **Rows read 4 MB back at most.** Work launched further back on the
  first look is missed.
