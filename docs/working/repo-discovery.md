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
  - One scan per set of roots at a time. A stuck one is not retried,
    since whatever blocked it would block the retry too.
  - Results are kept per set of roots.
- `find_repos` (MCP) and the TUI finder report "still scanning".
- **Parallel MCP calls were tried and reverted.** Ordering broke: a write and
  then a read could swap, and so could a create and then a go-to. The shim
  stays serial, and the daemon-side fixes remove the stall.

## In progress: items 2-4 (branch `repo-discovery-setup`, 0.23.0-dev.27)

One PR for items 2, 3 and 4. The user's calls on the plan's open questions:

- rlord saves the confirmed code folder itself (plain code); the overlord
  only proposes (`confirm_setup`). No `set_code_dir` tool.
- The other-filesystem (`st_dev`) rule applies on every platform.
- Creating a new repo (mkdir, `git init`) is not in this PR: owed.
- With neither `code_dir` nor `repo_roots` set, the finder keeps scanning
  `~` (with the item 4 guards) until setup is done or skipped.

Progress: (updated per commit)

- [x] config: `code_dir`, `scan_roots`, `set_code_dir`
- [x] predictor: `privacy.rs`, `git::scan_repos`, `skipped`
- [x] finder and `find_repos` show skipped folders
- [x] suggestion: `code_dir.rs`, sources, `setup.*` methods
- [x] `rlord setup`
- [x] TUI setup form, first run, `:setup`
- [x] overlord: `confirm_setup`, setup request, `overlord.md`
- [ ] fresh-install steps
- [ ] docs and reconcile

## Plan (agreed shape)

1. **The user's config:** `repo_roots = ["~/ko"]`. Done (the user's own file).
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

- **A stuck walk is never retried.** A dead mount or an unanswered prompt
  keeps those roots on their last result until the daemon restarts. Plan
  item 4 (`st_dev`, `/Volumes`) keeps walks out of such places. A
  per-folder time limit would skip them. Revisit if it bites.
- **The TUI finder reads `repo.find` once.** While `scanning`, it says
  "reopen to refresh" instead of re-polling.

- **MCP cancellation.** A cancelled or timed-out MCP call isn't
  passed on (`notifications/cancelled` is dropped), so the daemon can't abort
  work. That's harmless now that nothing blocks for long.
- **`guarded_home` relies on `$HOME`.** The macOS daemon runs from a user
  LaunchAgent, so `$HOME` is set.
