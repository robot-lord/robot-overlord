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
