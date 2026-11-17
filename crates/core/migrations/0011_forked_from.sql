-- A session started as a fork of another (`session.fork`): the parent's
-- rlord id. No foreign key: a session that ran is archived, not deleted,
-- so its forks keep naming it; the only session ever deleted is a fork
-- that never started (`Db::delete_session`), which nothing names yet.
ALTER TABLE sessions ADD COLUMN forked_from TEXT;
CREATE INDEX sessions_forked_from ON sessions (forked_from) WHERE forked_from IS NOT NULL;
