-- Schema v1. See docs/architecture.md "State model".
-- Derived last_* columns are maintained by the daemon on event insert so
-- sorting is an indexed read, never a scan of `events`.

CREATE TABLE sessions (
  id                   TEXT PRIMARY KEY,           -- ULID
  name                 TEXT NOT NULL,
  name_locked          INTEGER NOT NULL DEFAULT 0,
  agent                TEXT NOT NULL,              -- 'claude' | 'codex' | 'shell' | ...
  agent_session        TEXT,                       -- agent CLI's native conversation id
  agent_session_trust  TEXT NOT NULL DEFAULT 'minted'
                         CHECK (agent_session_trust IN ('minted','hook','scavenged')),
  project_path         TEXT NOT NULL,
  tmux_session         TEXT,                       -- NULL when not running
  status               TEXT NOT NULL DEFAULT 'stopped'
                         CHECK (status IN ('starting','working','waiting_input','idle','stopped','error')),
  launch_spec          TEXT NOT NULL DEFAULT '{}', -- JSON
  group_path           TEXT NOT NULL DEFAULT '',
  pinned               INTEGER NOT NULL DEFAULT 0,
  sort_key             INTEGER NOT NULL DEFAULT 0,
  created_at           INTEGER NOT NULL,           -- ms epoch
  archived_at          INTEGER,                    -- NULL = live
  -- derived meta-context (NULL = unknown, never epoch)
  last_user_msg_at     INTEGER,
  last_agent_msg_at    INTEGER,
  last_state_change_at INTEGER,
  last_delivery_at     INTEGER
);

CREATE INDEX idx_sessions_archived ON sessions (archived_at);
CREATE INDEX idx_sessions_last_agent_msg ON sessions (last_agent_msg_at);
CREATE INDEX idx_sessions_last_user_msg ON sessions (last_user_msg_at);

CREATE TABLE tags (
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  tag        TEXT NOT NULL,
  PRIMARY KEY (session_id, tag)
);

CREATE TABLE events (
  seq        INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  ts         INTEGER NOT NULL,                     -- ms epoch
  kind       TEXT NOT NULL,
  payload    TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX idx_events_session ON events (session_id, seq);

CREATE TABLE messages (
  id           TEXT PRIMARY KEY,                   -- ULID
  to_session   TEXT NOT NULL,
  from_session TEXT,                               -- NULL = human/CLI origin
  body         TEXT NOT NULL,
  kind         TEXT NOT NULL DEFAULT 'info'
                 CHECK (kind IN ('task','info','reply','completion')),
  created_at   INTEGER NOT NULL,
  delivered_at INTEGER,
  dedup_key    TEXT,
  UNIQUE (to_session, dedup_key)
);

CREATE INDEX idx_messages_pending ON messages (to_session) WHERE delivered_at IS NULL;

-- Transcript mirror bookkeeping (text lives on disk; see architecture.md).
CREATE TABLE transcripts (
  session_id     TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  source_path    TEXT NOT NULL,                    -- the agent CLI's file
  raw_offset     INTEGER NOT NULL DEFAULT 0,       -- bytes mirrored so far
  render_version INTEGER NOT NULL DEFAULT 0,
  unparsed_lines INTEGER NOT NULL DEFAULT 0,
  first_ts       INTEGER,
  last_ts        INTEGER
);
