-- Schema v8: what the session list's Usage column needs from a transcript
-- read position: prompts typed, tokens taken in new or written, the last
-- request's context, the reported context window (Codex) and the model.
ALTER TABLE transcript_cursors ADD COLUMN turns INTEGER NOT NULL DEFAULT 0;
ALTER TABLE transcript_cursors ADD COLUMN tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE transcript_cursors ADD COLUMN context INTEGER NOT NULL DEFAULT 0;
ALTER TABLE transcript_cursors ADD COLUMN window INTEGER;
ALTER TABLE transcript_cursors ADD COLUMN model TEXT;
