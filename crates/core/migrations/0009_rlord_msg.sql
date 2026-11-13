-- When rlord itself last typed a prompt into the session (send_to_session,
-- a resume after a usage limit, overlord ask). `last_user_msg_at` is the
-- user's own prompts only.
ALTER TABLE sessions ADD COLUMN last_rlord_msg_at INTEGER;
