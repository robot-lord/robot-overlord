-- The latest event of a kind per session (`Db::live_transcripts`: when
-- rlord last started a session's agent again) is an indexed read.
CREATE INDEX idx_events_session_kind ON events (session_id, kind, ts);
