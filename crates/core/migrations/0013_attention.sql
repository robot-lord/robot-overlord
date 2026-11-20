-- The attention queue (`rlord_daemon::attention`).
-- When a person's terminal last showed the session: a finished turn it
-- showed since is seen.
ALTER TABLE sessions ADD COLUMN last_viewed_at INTEGER;
-- Why it waits: while `waiting_input`, `permission` or `question`; while
-- `idle`, how the turn ended, `finished` or `failed`.
ALTER TABLE sessions ADD COLUMN attention_reason TEXT;
