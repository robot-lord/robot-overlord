**Status:** in progress

# Repo discovery without privacy prompts

## What happened (2026-11-15)

The overlord's `find_repos` hung for 66 minutes. After it, `list_groups`,
`list_sessions` and `create_session` each failed in the client after 1800s.

- **The scan blocked on a prompt.** `find_repos` scanned `repo_roots = ["~"]`
  to depth 3. It read `~/Desktop`, `~/Documents` and similar folders, and macOS
  blocked each read on a privacy (TCC) consent dialog. The unified log shows
  the first dialog 117ms after the call. The user clicked through five
  dialogs from 14:32:30 to 14:32:43 local time.
- **The daemon ran the scan under its global DB lock** (`dispatch`), so every
  DB method waited for it, the TUI included.
- **`rlord mcp` is serial**, so the overlord's later calls queued behind the
  stuck one.

## Done: PR #62 (0.23.0-dev.26)

- `find_repos` skips `~/{Desktop,Documents,Downloads,Movies,Music,Pictures}`
  on macOS unless one is named as a root.
- **Scan off the DB lock, in the background** (`daemon/src/repo_scan.rs`).
  Callers get the last result for their roots, with `scanning: true` while
  roots never scanned have no result yet.
  - Those are waited on only during a scan's first 2s.
  - A scan running over 5 minutes is presumed stuck: one retry starts, with
    at most two in flight.
  - A generation counter keeps only the newest scan's result.
- `find_repos` (MCP) and the TUI finder report "still scanning".
- **Parallel MCP calls were tried and reverted.** Ordering broke: a write and
  then a read could swap, and so could a create and then a go-to. The shim
  stays serial, and the daemon-side fixes remove the stall.

## Plan (agreed shape, not started)

1. **The user's config:** `repo_roots = ["~/ko"]`. Waiting on the user's yes.
2. **`[session] code_dir`:** where repos live and new ones are created.
   - `repo_roots` defaults to `[code_dir]`, so scanning all of `~` becomes
     opt-in.
   - `get_session_defaults` returns it.
   - `overlord.md`: create new repos under `code_dir`, and ask when it's
     empty. Don't guess a path like `~/src/...`.
3. **Set `code_dir` at setup.** There's no setup flow yet: add a first run
   (TUI, overlord) plus `rlord setup`.
   - Suggest the most common parent of paths rlord can already read:
     Claude's `~/.claude/projects`, Codex session cwds, OpenCode's
     `session_v2` directory, past sessions' repos, and the launch folder.
   - Confirm it with describe-then-confirm. If there's no signal, ask.
4. **Predict prompts before touching a folder.** No public API reports TCC
   status without risking the prompt, and `TCC.db` needs Full Disk Access.
   So the predictor uses:
   - a fixed list (the home folders above, `~/Library`, `/Volumes/*`);
   - don't cross filesystems (skip a different `st_dev`, like `find -xdev`),
     which catches network, removable and File Provider mounts;
   - never follow symlinks (already true).

   A folder the user named, or one where a session already runs, counts as
   granted. The finder shows "skipped ~/Documents (macOS asks permission)…".

## Owed and open

- **MCP cancellation.** A cancelled or timed-out MCP call isn't
  passed on (`notifications/cancelled` is dropped), so the daemon can't abort
  work. That's harmless now that nothing blocks for long.
- **`guarded_home` relies on `$HOME`.** The macOS daemon runs from a user
  LaunchAgent, so `$HOME` is set.
