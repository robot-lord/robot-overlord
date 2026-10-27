-- Schema v4: sub-agents spawned by a session (Claude Code Agent tool).
CREATE TABLE subagents (
  agent_id        TEXT PRIMARY KEY,
  session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  agent_type      TEXT NOT NULL DEFAULT '',
  description     TEXT NOT NULL DEFAULT '',
  tool_use_id     TEXT,
  spawn_depth     INTEGER NOT NULL DEFAULT 1,
  background      INTEGER NOT NULL DEFAULT 0,
  started_at      INTEGER,
  finished_at     INTEGER,
  transcript_path TEXT,
  result          TEXT NOT NULL DEFAULT '',
  input_tokens    INTEGER NOT NULL DEFAULT 0,
  output_tokens   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_subagents_session ON subagents (session_id, started_at);
