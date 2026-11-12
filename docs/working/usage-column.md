**Status:** in progress (built on `usage-column`, 0.22.0-dev.2; PR open, not merged)

# Usage column

Goal: a "Usage" column in the session list showing turns (default), tokens
or context %, chosen in config and switchable in the UI; a usage sort; all
three in the details pane; exposed to the overlord. From data rlord already
has, read cheaply, no model calls.

## Decisions

- **Definitions.** turns = prompts the human typed. tokens = input not read
  from a cache (cache writes count) + output, so Claude and Codex compare
  (Codex's input includes cached tokens; it's subtracted). context = the
  last request's whole context; % of the model's window.
- **Claude double counting.** Claude writes one API message over several
  lines, each repeating the same usage (checked on real transcripts: about
  2x). Usage now counts once per `message.id`. This also fixes the existing
  input/output totals (sub-agent token counts were roughly doubled).
  `STATS_PARSE_VERSION` 2 drops saved cursors so everything is recounted
  once.
- **Storage.** The counts ride on the existing incremental
  `transcript_stats` cursor and are saved with it (migration 0008), so a
  restarted daemon reads forward. OpenCode: three indexed look-ups in
  `session_v2` / `session_message`, cached per session until opencode.db or
  its WAL changes size/mtime.
- **Window.** Codex reports its own; otherwise a per-family table in
  `models.rs`. Claude transcripts don't say `[1m]`, so a 200k model that
  sent more than 200k is taken as 1M. Unknown models (e.g. the demo's
  deepseek on OpenCode) show `–`.
- **Sort.** `SortMode::Usage` ("usage") can't be SQL: SQL orders by last
  message, the daemon re-sorts by the shown measure (pinned first, unknown
  last). The measure travels in `SessionQuery::usage`; default config
  `usage`.
- **No stall.** A transcript rlord hasn't read, or one >4 MB behind, goes
  to the warm-up worker (`Job::Usage`, once per run, so an unreadable file
  can't loop); the row shows as loading meanwhile.
- **Header click sorts, doesn't switch the measure.** Every other header
  click means "sort by this / reverse", so the Usage header does the same.
  Switching is `:usage turns|tokens|context`, config, or the overlord
  (`set_list_view usage`).
- **Layout.** Column 7 wide ("Usage ▼"), values 4 wide right-aligned under
  "Usage". Narrowing order: Project goes first, then Usage, then Groups.
- **UI switch is per TUI run** (like the sort); config sets the start. The
  view state carries `usage`, so a saved default view can set it too.
- `rlord ls` got a USAGE column and `--usage`.

## Owed / deferred

- Context windows for future model families: keep `CONTEXT_WINDOWS` in
  `models.rs` current, like `CLAUDE_LATEST`.
- A Claude message split across a daemon restart can count twice (the
  last counted message id isn't persisted). Small; left.
- The list refetches on daemon change notices (hooks), so tokens/context
  update at turn boundaries, not mid-turn.
- Screenshots not retaken (demo now produces plausible numbers).
