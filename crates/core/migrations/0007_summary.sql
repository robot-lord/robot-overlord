-- Schema v7: what a session did, in a paragraph (made when it's archived).
ALTER TABLE sessions ADD COLUMN summary TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN summary_at INTEGER;
