# Fresh repo code: what is still owed

Residue of `docs/working/fresh-context.md` (in git history), reconciled
2026-11-16 with 0.23.0-dev.30. How `repo_latest` and a new session's
base work now is in `docs/architecture.md` ("The latest code
(`repo_latest`)", "Session creation", "Session background") and
`docs/guide.md` ("Creating sessions").

## Owed

- **A read-only tool for sessions**: "which commit did I start from, what
  is the latest", in session-context PR 2's per-session tools
  (`repo_latest` is mutating, so a plain `rlord mcp` session can't call
  it).
- **Fork workflows**: an `upstream` remote that is the real source of the
  default branch. Today: `origin`, else the only remote (user's call).
- **Tags and commit ids as `ref`**: branches only today.
- **Submodules and LFS content** in the tree: warnings only today.
- **`git()` in `git.rs`** still has no time limit or no-prompt setup; move
  its callers to `latest::run` when next touched.
- **Two daemons on one machine** (the dev daemon beside the real one)
  share the tree in the repo but not the per-repo lock: a move by one
  during the other's can hit `index.lock`: the loser's call errors and
  leaves the tree as it is (no rebuild on a lock), and a retry works. A
  lock file in the tree's admin dir would serialize them. Each daemon records the tree in its own kv; the
  other one adopts it (detached `.rlord-latest` at the expected path).
- **A stale `index.lock` in the tree** makes `repo_latest` fail (it
  leaves the tree alone rather than rebuild it) until the lock is removed;
  the error says so.
- **From a linked worktree of a `--separate-git-dir` repo** the main
  checkout can't be known: an error asks for its path.
- **One safe git runner for all of rlord**: `latest::run` (time limit,
  no prompts, no hooks) lives in `latest.rs`; move it to a module of its
  own that `git.rs` and the rest use too.
- **`remote_env` worked out lazily**: it runs `git config` twice per
  resolve even when nothing talks to the remote (a cached fetch, a
  cached default branch).
- **One `clip` and one repo-root resolver**: `latest.rs` has its own
  `clip` and `locate` beside other helpers that clip text and
  `git::main_repo_root`; keep one of each.
- **A stale `origin/HEAD` after a default-branch rename**: while the
  old name still exists on the remote, `refs/remotes/origin/HEAD` keeps
  naming it and rlord follows it; only a branch gone from the remote
  makes it ask again (`ls-remote`).
- **A sweep racing a just-handed-out tree**: the sweep can remove a tree
  (unused 30 days) right after a call returned its `path`, before the
  caller reads it; the next `repo_latest` makes it again.
- **`drop_old_refs` works from the record read at the start of the
  move**: a branch fetched by another call meanwhile can have its private
  ref dropped; the next call fetches it again.
- **Repeated logic**: the failure classes of `Fetch` (offline, failed,
  missing) and the "is it rlord's tree" checks appear in a few places;
  one helper each.
- **Git spawns per call**: a `repo_latest` call still runs a dozen or so
  git commands (locate, remote, refs, relation, status, checkout); batch
  the plumbing (`for-each-ref`, one `rev-parse` for several refs).
- **No end-to-end launch test**: `session_new` needs an installed agent
  and tmux, so the base is tested through `set_launch_base` and
  `methods::new_worktree`, and the note through `base_note`; the stored
  `source = 'base'` row and the background file text are untested.
