//! Why a session waits (`sessions.attention_reason`), kept beside its
//! status by every path that changes the status: the hooks
//! (`methods::hook_emit`), OpenCode (`opencode.rs`), the transcript
//! follower (`discover.rs`), rlord's held messages (`send.rs`). One rule
//! for all of them, here. A stop or a (re)start clears it in
//! `Db::set_status` itself, whoever writes it.
//!
//! - While it needs you: `question` (a question dialog, Claude's
//!   `AskUserQuestion`, an elicitation, an OpenCode form) or `permission`.
//!   A question isn't turned back into a permission prompt while the same
//!   wait goes on (Claude fires both for one question).
//! - A turn's end: `finished`, or `failed` (it ended on an API error).
//! - Anything that starts a turn, (re)starts or stops the session, or ends
//!   a turn without a real end (OpenCode's server gone): none, unless the
//!   session still waits (a prompt while a sub-agent's wait goes on).

use anyhow::Result;

use rlord_core::db::Db;
use rlord_core::types::{Session, Status};

/// The event kinds that ask the user something (the session then waits).
const ASKS: &[&str] = &[
    "permission_request",
    "question_asked",
    "question",
    "notification",
];

/// The event kinds that leave no reason, among those `record` is called
/// with: a turn starting (the user's, rlord's, or one the agent began
/// without a prompt hook), a start, a turn that ended without a real end.
/// (A stop or a restart: `Db::set_status` clears it.)
const CLEARS: &[&str] = &[
    "prompt_submitted",
    "rlord_prompt",
    "turn_started",
    "message_delivered",
    "session_start",
    TURN_LOST,
];

/// The kind for an `agent_stopped` that wasn't a real turn end (OpenCode's
/// server gone, or the turn not on the server any more).
pub(crate) const TURN_LOST: &str = "turn_lost";

/// What an event of `kind` leaves as session `s`'s reason, once its
/// status is `after`: `Some(r)` to write, None to leave it. `question`:
/// what it asks is a question (only read for an asking kind). `failed`:
/// the turn ended on an API error (only read for `agent_stopped`).
pub(crate) fn after(
    s: &Session,
    kind: &str,
    after: Status,
    question: bool,
    failed: bool,
) -> Option<Option<&'static str>> {
    if after == Status::WaitingInput && ASKS.contains(&kind) {
        // (The question kinds are questions whatever `question` says.)
        let question = question || matches!(kind, "question" | "question_asked");
        let kept =
            s.status == Status::WaitingInput && s.attention_reason.as_deref() == Some("question");
        return Some(Some(if question || kept {
            "question"
        } else {
            "permission"
        }));
    }
    match kind {
        "agent_stopped" if after == Status::Idle => {
            Some(Some(if failed { "failed" } else { "finished" }))
        }
        k if CLEARS.contains(&k) && after != Status::WaitingInput => Some(None),
        _ => None,
    }
}

/// `after`, written if it changes the reason.
pub(crate) fn record(
    db: &Db,
    s: &Session,
    kind: &str,
    status: Status,
    question: bool,
    failed: bool,
) -> Result<()> {
    if let Some(r) = after(s, kind, status, question, failed) {
        if r != s.attention_reason.as_deref() {
            db.set_attention_reason(&s.id, r)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session(status: &str, reason: Option<&str>) -> Session {
        serde_json::from_value(json!({
            "id": "s", "name": "s", "name_locked": false, "agent": "claude",
            "agent_session": null, "agent_session_trust": "minted", "project_path": "/tmp",
            "tmux_session": null, "status": status, "group_path": "", "pinned": false,
            "created_at": 1, "archived_at": null, "last_user_msg_at": null,
            "last_agent_msg_at": null, "last_state_change_at": null, "repo_path": null,
            "workspace_path": null, "branch": null, "model": null,
            "attention_reason": reason}))
        .unwrap()
    }

    #[test]
    fn one_rule_for_every_path() {
        use Status::*;
        let working = session("working", None);
        assert_eq!(
            after(&working, "permission_request", WaitingInput, false, false),
            Some(Some("permission"))
        );
        assert_eq!(
            after(&working, "question", WaitingInput, true, false),
            Some(Some("question"))
        );
        // A question it still waits on stays the reason.
        let asking = session("waiting_input", Some("question"));
        assert_eq!(
            after(&asking, "permission_request", WaitingInput, false, false),
            Some(Some("question"))
        );
        assert_eq!(
            after(&working, "agent_stopped", Idle, false, false),
            Some(Some("finished"))
        );
        assert_eq!(
            after(&working, "agent_stopped", Idle, false, true),
            Some(Some("failed"))
        );
        for k in [
            "prompt_submitted",
            "message_delivered",
            "session_start",
            TURN_LOST,
        ] {
            assert_eq!(after(&working, k, Working, false, false), Some(None), "{k}");
        }
        // A prompt while still waiting (a sub-agent's): leaves it.
        assert_eq!(
            after(&asking, "prompt_submitted", WaitingInput, false, false),
            None
        );
        // Anything else: leaves it.
        assert_eq!(after(&working, "renamed", Working, false, false), None);
    }
}
