//! Sessions whose agent isn't reporting through hooks: learn what the hooks
//! would have said from the agent's own files. Plain code, no AI.
//!
//! Codex assigns its conversation id itself and runs a hook only once the
//! user has trusted it (`/hooks`). Until then, a Codex session's
//! conversation is found in Codex's thread index (the thread started in the
//! session's folder since it was launched), and its status is read from the
//! end of its transcript (a turn started / completed) and its pane (the
//! program exited). Once a hook has confirmed the session
//! (`agent_session_trust = hook`) it is left to the hooks.
//!
//! One slow tick over the few sessions this applies to; a transcript is
//! read only when it was written since the last look. All file and tmux
//! reads happen without the DB lock.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde_json::json;

use rlord_core::agents::{codex, transcript, Agent};
use rlord_core::tmux;
use rlord_core::types::{now_ms, Session, Status};

use crate::{methods, Daemon};

const TICK: Duration = Duration::from_secs(3);

/// Start the watcher.
pub fn start(d: &Arc<Daemon>) {
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        // When each session's transcript was last looked at.
        let mut seen: HashMap<String, SystemTime> = HashMap::new();
        loop {
            std::thread::sleep(TICK);
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pass(&d, &mut seen)));
            match r {
                Ok(Err(e)) => tracing::debug!(error = %e, "following sessions without hooks"),
                Err(_) => tracing::warn!("following sessions without hooks panicked"),
                Ok(Ok(())) => {}
            }
        }
    });
}

/// Live sessions of agents that assign their own id, not confirmed by a hook.
fn unhooked(d: &Daemon) -> Result<Vec<Session>> {
    Ok(d.db()
        .running_sessions()?
        .into_iter()
        .filter(|s| Agent::parse(&s.agent).is_some_and(|a| !a.mints_id()))
        .filter(|s| s.agent_session.is_none() || s.agent_session_trust != "hook")
        .collect())
}

fn pass(d: &Daemon, seen: &mut HashMap<String, SystemTime>) -> Result<()> {
    let sessions = unhooked(d)?;
    seen.retain(|id, _| sessions.iter().any(|s| &s.id == id));
    let mut changed = false;
    for s in sessions {
        // Its program exited (no SessionEnd hook to say so).
        let dead = s
            .tmux_session
            .as_deref()
            .is_some_and(|t| !tmux::has_session(t) || tmux::format(t, "#{pane_dead}") == "1");
        if dead {
            let db = d.db();
            let now = now_ms();
            db.set_status(&s.id, Status::Stopped, now)?;
            db.insert_event(&s.id, now, "session_end", r#"{"from":"pane"}"#)?;
            changed = true;
            continue;
        }
        if s.agent_session.is_none() {
            changed |= bind(d, &s)?;
        } else {
            changed |= follow(d, &s, seen)?;
        }
    }
    if changed {
        d.changed();
    }
    Ok(())
}

/// Find the conversation a session launched, in its agent's index: the
/// first thread started in its folder since, that no session holds.
fn bind(d: &Daemon, s: &Session) -> Result<bool> {
    let (since, tracked) = {
        let db = d.db();
        let since = db
            .kv_get(&methods::bind_since_key(&s.id))?
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(s.created_at);
        (since, db.agent_session_ids()?)
    };
    let found = codex::threads_started_in(&codex::home(), &s.project_path, since)
        .into_iter()
        .find(|t| !tracked.contains(&t.id));
    let Some(t) = found else {
        return Ok(false);
    };
    let db = d.db();
    // A hook may have bound it meanwhile.
    if db.resolve_ref(&s.id)?.agent_session.is_some() {
        return Ok(false);
    }
    // rlord's own attribution ("minted"), until a hook confirms it.
    db.set_agent_session(&s.id, &t.id, "minted")?;
    db.upsert_transcript_source(&s.id, &t.rollout_path)?;
    db.insert_event(
        &s.id,
        now_ms(),
        "agent_session_bound",
        &json!({"to": t.id, "from": "codex thread index"}).to_string(),
    )?;
    tracing::info!(session = %s.id, thread = %t.id, "bound a Codex session from its thread index");
    Ok(true)
}

/// Read a session's status from the end of its transcript.
fn follow(d: &Daemon, s: &Session, seen: &mut HashMap<String, SystemTime>) -> Result<bool> {
    let Some(src) = d.db().transcript_source(&s.id)? else {
        return Ok(false);
    };
    let path = Path::new(&src);
    let Some(modified) = std::fs::metadata(path).and_then(|m| m.modified()).ok() else {
        return Ok(false);
    };
    if seen.get(&s.id) == Some(&modified) && s.status != Status::Starting {
        return Ok(false);
    }
    seen.insert(s.id.clone(), modified);
    let Some(next) = next_status(s.status, transcript::working(path)) else {
        return Ok(false);
    };
    let db = d.db();
    // The hooks started reporting meanwhile: theirs stands.
    let cur = db.resolve_ref(&s.id)?;
    if cur.agent_session_trust == "hook" || cur.status != s.status {
        return Ok(false);
    }
    let now = now_ms();
    db.set_status(&s.id, next, now)?;
    let kind = match next {
        Status::Working => {
            db.touch_last_user_msg(&s.id, now)?;
            "prompt_submitted"
        }
        _ => {
            if s.status == Status::Working {
                db.touch_last_agent_msg(&s.id, now)?;
            }
            "agent_stopped"
        }
    };
    db.insert_event(&s.id, now, kind, r#"{"from":"transcript"}"#)?;
    Ok(true)
}

/// The status a transcript's tail implies, if it differs: `working` is
/// whether a turn is open (None: no turn yet). A permission prompt can't be
/// seen this way, so "waiting" is left alone only when a hook set it.
fn next_status(cur: Status, working: Option<bool>) -> Option<Status> {
    let want = match working {
        Some(true) => Status::Working,
        // Up and between turns (or not yet in one).
        Some(false) | None => Status::Idle,
    };
    match cur {
        Status::Starting | Status::Working | Status::Idle if cur != want => Some(want),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_follows_the_open_turn() {
        use Status::*;
        assert_eq!(next_status(Starting, None), Some(Idle));
        assert_eq!(next_status(Starting, Some(true)), Some(Working));
        assert_eq!(next_status(Idle, Some(true)), Some(Working));
        assert_eq!(next_status(Working, Some(false)), Some(Idle));
        // Nothing to change.
        assert_eq!(next_status(Idle, Some(false)), None);
        assert_eq!(next_status(Working, Some(true)), None);
        assert_eq!(next_status(Idle, None), None);
        // Not a state the transcript can speak to.
        assert_eq!(next_status(WaitingInput, Some(true)), None);
        assert_eq!(next_status(Stopped, Some(true)), None);
    }
}
