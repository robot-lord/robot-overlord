# Repo discovery: what is still owed

Residue of `docs/working/repo-discovery.md` (in git history), reconciled
2026-11-15 with 0.23.0-dev.27. How the repo scan, its guard and the code
folder work now is in `docs/architecture.md` ("MCP server" bullet on the
repo scan, "Setup: the code folder") and `docs/guide.md` ("The overlord",
"Creating sessions").

## Owed

- **Creating a new repo.** The overlord has no shell and `session.new`
  rejects a folder that doesn't exist, so `overlord.md` says to launch the
  session in `code_dir` as a plain folder and tell it where the repo goes.
  A plain-code route (a `create_session` option that makes
  `<code_dir>/<name>` and runs `git init`) was left out of this effort by
  the user's call.
- **A stuck walk is never retried.** A dead mount or an unanswered prompt
  keeps those roots on their last result until the daemon restarts. The
  guard keeps walks out of other filesystems, but an `lstat` on a dead
  network mount point inside a root can still stall one scan. A per-folder
  time limit would fix it.
- **The TUI finder reads `repo.find` once.** While `scanning`, it says
  "reopen to refresh" instead of re-polling.
- **MCP cancellation.** A cancelled or timed-out MCP call isn't passed on
  (`notifications/cancelled` is dropped), so the daemon can't abort work.
  Harmless while nothing blocks for long.
- **`privacy::Guard` relies on `$HOME`.** The macOS daemon runs from a
  user LaunchAgent, so `$HOME` is set.
- **Subfolders of one repo count twice in the code-folder vote.** The
  vote is plain string work; only the candidates about to be shown are
  checked for being a repo (under `$HOME`). Sessions in `app/src` and
  `app/docs` can still lift `app`'s parent's score.
- **From the last `/code-review` of #63, not fixed:**
  - The TUI recognises a missing folder by matching `setup.apply`'s
    "doesn't exist" error text; it should be a structured `missing` field.
  - A second `setup.suggest` can start while one is still running (two
    forms opened quickly in one run before the first answer lands).
  - `claude_project_folders` reads each transcript's tail as well as its
    head; reading the head first and stopping there would read less.
  - `Guard::guarded_ancestor` builds a `PathBuf` per protected folder for
    every entry the walk meets; it could compare components in place.
- **Other code that touches folders unprompted doesn't ask the guard
  yet**: `methods::repo_list`'s `is_dir` on known repos,
  `import::add_git_info`.
