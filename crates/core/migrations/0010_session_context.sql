-- What the overlord gives a session besides the user's words: notes
-- (facts, pointers, answers the user gave) and links to related sessions.
-- Pushed to the agent by its SessionStart / UserPromptSubmit hook
-- (`seen_at`), or read from <data>/sessions/<id>/context.md.
-- See docs/working/session-context.md.
CREATE TABLE session_context (
  id         INTEGER PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  at         INTEGER NOT NULL,                  -- ms epoch
  kind       TEXT NOT NULL CHECK (kind IN ('note','link')),
  body       TEXT NOT NULL,                     -- note text, or the linked session's id
  source     TEXT NOT NULL DEFAULT 'overlord',
  seen_at    INTEGER,                           -- handed to the agent (its answer acknowledged)
  sent_ack   TEXT,                              -- the latest hook answer that carried it
  sent_at    INTEGER                            -- when that answer was made
);

CREATE INDEX session_context_by_session ON session_context (session_id, id);
