-- When rlord itself last typed a prompt into the session (send_to_session,
-- a held when_idle message, a resume after a usage limit).
-- `last_user_msg_at` is the user's own prompts only; `overlord ask` carries
-- what the user typed, so it counts as the user's.
ALTER TABLE sessions ADD COLUMN last_rlord_msg_at INTEGER;
