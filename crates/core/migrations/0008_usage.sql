-- Schema v8: what the session list's Usage column needs from a transcript
-- read position: prompts typed, tokens taken in new or written, the last
-- request's context, the reported context window (Codex) and the model.
ALTER TABLE transcript_cursors ADD COLUMN turns INTEGER NOT NULL DEFAULT 0;
ALTER TABLE transcript_cursors ADD COLUMN tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE transcript_cursors ADD COLUMN context INTEGER NOT NULL DEFAULT 0;
ALTER TABLE transcript_cursors ADD COLUMN window INTEGER;
ALTER TABLE transcript_cursors ADD COLUMN model TEXT;

-- The transcript parse version (`STATS_PARSE_VERSION`) a sub-agent's token
-- totals were counted with; 0: not by the current one (or not from its
-- transcript at all). A count of an older version is overwritten by the
-- next count from its transcript, even when lower; otherwise the larger
-- value is kept.
ALTER TABLE subagents ADD COLUMN counted INTEGER NOT NULL DEFAULT 0;
