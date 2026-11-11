-- Schema v5: the overlord. `role` marks special sessions ('' = ordinary,
-- 'overlord' = the singleton management session); `kv` holds small durable
-- UI state such as the default list view.
ALTER TABLE sessions ADD COLUMN role TEXT NOT NULL DEFAULT '';
CREATE INDEX idx_sessions_role ON sessions (role);

CREATE TABLE kv (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
