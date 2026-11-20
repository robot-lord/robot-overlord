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
- Removed in round 9: the round-6 cross-check, where the hooks'
  `SubagentStop` closed open agents. `subagents.finished_at` is partly
  guessed (`warm.rs`, `close_open_subagents` when a session stops), so it
  closed tasks that were still running. The `⚙` count and `bg` can still
  disagree.
- Round 9: a stopped session keeps its state for 15 s, because `/clear`
  fires `SessionEnd` (stopped) in the same process.
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
- Round 8: a resumed agent gets back its label and files (32 finished
  agents remembered). Only `SessionStart` `startup` counts as a new process.
  `resume` does not, since an in-process `/resume` may keep the tasks.
  Reads share 16 MB a tick across sessions. `rlord show` prints the status
  sentences.
- The stale guard (30 min) hides work; it never forgets it. Work is
  forgotten on a stop, or when a new agent process starts (an rlord
  resume, restart or refork event, or a `SessionStart` hook with source
  `startup`; `resume` doesn't count).

## Owed

- **Reconcile with `scan_subagents_read`.** Its rules for which
  sub-agents run differ from the tracker's. Bring them together, or say
  why they differ.
- **`hover_still_there`.** It rechecks the popup at the stored pointer
  position, which may be stale once rows move under a still pointer.
- **Popup height.** `theme::wrapped_rows` approximates ratatui's own
  wrapping; a long word or a wide character can leave the popup one row
  short.
- **Hooks-first redesign.** Take the subagent count from the `subagents`
  table (the hooks) and read the transcript only for forked skills, shells
  and labels. Caveat: `subagents.finished_at` is partly guessed today
  (`warm.rs` backfills it, and `close_open_subagents` sets it when a
  session stops). It can't close tasks until only real `SubagentStop`
  hooks set it (or the guessed values are marked).
- **`claude -c` / `--resume` outside rlord.** That sends `SessionStart`
  `resume`, which isn't counted as a new process, so the old tasks stay
  until the stale guard hides them. A sure sign of a new process would
  fix it (the pane's pid changing).
- **`json_extract` in `live_transcripts`.** Each tick reads the payload
  of the session's `session_start` events. Storing the start kind in its
  own column (or a distinct event kind for `startup`) would make the
  lookup index-only.
- **Codex and OpenCode adapters.** `Tracker::advance_within` is the hook. Neither
  agent has a known background-task record yet.
- **Needs-you reason.** Stored since the attention queue's first PR
  (`sessions.attention_reason`: `permission` | `question`). Owed: the popup
  and details still say "blocked on a permission prompt or a question";
  they can say which.
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

### Owed from the final review (1.2.0)
- Right after a `/clear`, before the first prompt creates the new file, shells are hidden and agents rely on their own files (refresh's stat-failed branch passes main = 0).
- A Bash call moved to the background by its timeout has no `run_in_background`, so its call line is skipped and the shell is labelled "task".
- Shells still show during the 15 s stop grace.
- The first wheel event over the popup is swallowed (no `drop_popup` on wheel).
- `live_transcripts`' OR plus `json_extract` subquery may not seek the index; log a startup as its own event kind.
- Copies of the shown-status fallback (statustext, main.rs, mcp.rs) and the task-phrase wording.
