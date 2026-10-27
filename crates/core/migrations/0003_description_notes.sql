-- Schema v3: human-authored context on a session (the control layer).
ALTER TABLE sessions ADD COLUMN description TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN notes TEXT NOT NULL DEFAULT '';
