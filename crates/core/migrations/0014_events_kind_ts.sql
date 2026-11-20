-- Events of a kind over a time span (the attention report, pane deaths)
-- across every session: an indexed read.
CREATE INDEX idx_events_kind_ts ON events (kind, ts);
