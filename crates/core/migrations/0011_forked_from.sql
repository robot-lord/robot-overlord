-- A session started as a fork of another (`session.fork`): the parent's
-- rlord id. No foreign key: sessions are never deleted, only archived.
ALTER TABLE sessions ADD COLUMN forked_from TEXT;
CREATE INDEX sessions_forked_from ON sessions (forked_from) WHERE forked_from IS NOT NULL;
