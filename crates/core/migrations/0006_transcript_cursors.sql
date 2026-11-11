-- Schema v6: how far each sub-agent transcript has been read, and the totals
-- so far, so a restarted daemon reads forward instead of from the start.
CREATE TABLE transcript_cursors (
  path          TEXT PRIMARY KEY,
  ino           INTEGER NOT NULL,
  offset        INTEGER NOT NULL,
  input_tokens  INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  first_at      INTEGER,
  last_at       INTEGER,
  ended         INTEGER NOT NULL DEFAULT 0,
  -- claude::STATS_PARSE_VERSION when saved: rows from older counting logic
  -- are dropped (and recounted) at startup.
  version       INTEGER NOT NULL DEFAULT 0
);
