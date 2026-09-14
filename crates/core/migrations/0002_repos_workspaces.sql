-- Schema v2: git repos used by sessions (for the new-session picker, ordered
-- by recency) and the worktree a session lives in.
ALTER TABLE sessions ADD COLUMN repo_path TEXT;
ALTER TABLE sessions ADD COLUMN workspace_path TEXT;
ALTER TABLE sessions ADD COLUMN branch TEXT;

CREATE TABLE repos (
  path         TEXT PRIMARY KEY,
  last_used_at INTEGER NOT NULL,
  use_count    INTEGER NOT NULL DEFAULT 0
);
