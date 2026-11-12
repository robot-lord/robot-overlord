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
//! read incrementally (only what was appended since the last look). All
//! file and tmux reads happen without the DB lock.
//!
//! Binding is deliberately cautious: only a thread started from the CLI in
//! the session's folder since its launch, whose first prompt is the one the
//! session was launched with (when it had one), and only when no other
//! session is waiting for a thread in the same folder. Otherwise the hooks
//! (or the user) decide.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use rlord_core::agents::{codex, transcript, Agent};
use rlord_core::db::Db;
use rlord_core::tmux;
use rlord_core::types::{now_ms, Session, Status};

use crate::{methods, Daemon};

const TICK: Duration = Duration::from_secs(3);

/// Start the watcher.
pub fn start(d: &Arc<Daemon>) {
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        // How many ticks in a row each session's pane has looked gone.
        let mut seen: HashMap<String, u8> = HashMap::new();
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

fn pass(d: &Daemon, gone: &mut HashMap<String, u8>) -> Result<()> {
    let sessions = unhooked(d)?;
    gone.retain(|id, _| sessions.iter().any(|s| &s.id == id));
    let mut changed = false;
    for s in &sessions {
        // Its program exited (no SessionEnd hook to say so).
        match pane_looks(s, gone) {
            0 => {}
            1 => continue,
            _ => {
                changed |= stop_from_pane(&d.db(), s)?;
                continue;
            }
        }
        if s.agent_session.is_none() {
            // Another session waiting for a thread in the same folder.
            let others = sessions.iter().any(|o| {
                o.id != s.id && o.agent_session.is_none() && o.project_path == s.project_path
            });
            changed |= bind(d, s, others)?;
        } else {
            changed |= follow(d, s)?;
        }
    }
    if changed {
        d.changed();
    }
    Ok(())
}

/// How many looks in a row session `s`'s pane has looked gone (its tmux
/// session missing, or its program exited), counted in `gone`. One failed
/// look isn't proof (tmux busy, a restart in between): act on two.
pub(crate) fn pane_looks(s: &Session, gone: &mut HashMap<String, u8>) -> u8 {
    let dead = s
        .tmux_session
        .as_deref()
        .is_some_and(|t| !tmux::has_session(t) || tmux::format(t, "#{pane_dead}") == "1");
    let looks = gone.entry(s.id.clone()).or_default();
    *looks = if dead { looks.saturating_add(1) } else { 0 };
    *looks
}

/// Session `s` is stopped: its pane ended. Not when its status moved since
/// `s` was read (restarted meanwhile: not this pane's end). Returns
/// whether it was written.
pub(crate) fn stop_from_pane(db: &Db, s: &Session) -> Result<bool> {
    // Gone (deleted meanwhile) or moved on: leave it to the next pass.
    match db.resolve_ref(&s.id) {
        Ok(cur) if cur.status == s.status => {}
        _ => return Ok(false),
    }
    let now = now_ms();
    db.set_status(&s.id, Status::Stopped, now)?;
    db.insert_event(&s.id, now, "session_end", r#"{"from":"pane"}"#)?;
    Ok(true)
}

/// Find the conversation a session launched, in its agent's index (see
/// `pick_thread`). `others`: another session waits for one in this folder.
fn bind(d: &Daemon, s: &Session, others: bool) -> Result<bool> {
    let (since, prompt, tracked) = {
        let db = d.db();
        let since = db
            .kv_get(&methods::bind_since_key(&s.id))?
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(s.created_at);
        let prompt = db
            .kv_get(&methods::bind_prompt_key(&s.id))?
            .filter(|p| !p.trim().is_empty());
        (since, prompt, db.agent_session_ids()?)
    };
    let candidates: Vec<codex::Thread> =
        codex::threads_started_in(&codex::home(), &s.project_path, since)
            .into_iter()
            .filter(|t| !tracked.contains(&t.id))
            .collect();
    let Some(t) = pick_thread(&candidates, prompt.as_deref(), others) else {
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
    // Bound: what the search for its thread needed is done with.
    db.kv_delete(&methods::bind_since_key(&s.id))?;
    db.kv_delete(&methods::bind_prompt_key(&s.id))?;
    db.insert_event(
        &s.id,
        now_ms(),
        "agent_session_bound",
        &json!({"to": t.id, "from": "codex thread index"}).to_string(),
    )?;
    tracing::info!(session = %s.id, thread = %t.id, "bound a Codex session from its thread index");
    Ok(true)
}

/// The thread a session launched, among the untracked CLI threads started
/// in its folder since (oldest first), or None when it can't be told:
///
/// - launched with a prompt: the thread whose first message is that
///   prompt. One that has no first message yet may still become it: wait.
/// - no prompt (a fresh conversation, or launched without one): the only
///   candidate, and only when no other session is waiting in this folder.
/// - an older Codex whose index keeps no first message: as with no
///   prompt (the only candidate, nobody else waiting).
/// - two that fit equally: neither (the hooks will say).
fn pick_thread<'a>(
    candidates: &'a [codex::Thread],
    prompt: Option<&str>,
    others_waiting: bool,
) -> Option<&'a codex::Thread> {
    let same = |a: &str, b: &str| {
        let (a, b) = (a.trim(), b.trim());
        // (Codex may keep only the start of a long first message.)
        !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
    };
    // (The index keeps first messages, or it doesn't: all or none.)
    let messages = candidates.iter().any(|t| t.first_user_message.is_some());
    let fit: Vec<&codex::Thread> = match prompt.filter(|_| messages) {
        Some(p) => candidates
            .iter()
            .filter(|t| same(t.first_user_message.as_deref().unwrap_or(""), p))
            .collect(),
        None if others_waiting => Vec::new(),
        None => candidates.iter().collect(),
    };
    match fit.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// Read a session's status from its transcript (incrementally).
fn follow(d: &Daemon, s: &Session) -> Result<bool> {
    let Some(src) = d.db().transcript_source(&s.id)? else {
        return Ok(false);
    };
    let Some(next) = next_status(s.status, transcript::working(Path::new(&src))) else {
        return Ok(false);
    };
    let db = d.db();
    methods::save_cursors(&db);
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

/// The status a transcript implies, if it differs: `working` is whether
/// a turn is open. None (no turn line read: there was none yet, or it began
/// before what this run has read) only settles a session still starting;
/// it never calls a working session idle. A permission prompt can't be
/// seen this way, so "needs you" is left alone (only a hook sets it).
fn next_status(cur: Status, working: Option<bool>) -> Option<Status> {
    let want = match (working, cur) {
        (Some(true), _) => Status::Working,
        (Some(false), _) => Status::Idle,
        // Up, and nothing says it's in a turn.
        (None, Status::Starting) => Status::Idle,
        (None, _) => return None,
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
        // Unknown never ends a turn that is running.
        assert_eq!(next_status(Working, None), None);
        // Not a state the transcript can speak to.
        assert_eq!(next_status(WaitingInput, Some(true)), None);
        assert_eq!(next_status(Stopped, Some(true)), None);
    }

    fn thread(id: &str, first: &str) -> codex::Thread {
        codex::Thread {
            id: id.into(),
            rollout_path: format!("/r/{id}.jsonl"),
            created_at_ms: 1,
            first_user_message: Some(first.into()),
        }
    }

    #[test]
    fn a_thread_is_bound_only_when_it_is_clearly_the_sessions() {
        let pick = |c: &[codex::Thread], prompt: Option<&str>, others: bool| {
            pick_thread(c, prompt, others).map(|t| t.id.clone())
        };
        let one = [thread("a", "fix the login test")];
        // Launched with that prompt: it's the one, even with a sibling waiting.
        assert_eq!(
            pick(&one, Some("fix the login test"), true).as_deref(),
            Some("a")
        );
        // A long prompt Codex kept only the start of.
        assert_eq!(
            pick(
                &one,
                Some("fix the login test, and keep the retry count"),
                false
            )
            .as_deref(),
            Some("a")
        );
        // Someone else's thread in the folder (another prompt): not ours.
        assert_eq!(pick(&one, Some("speed up the image pipeline"), false), None);
        // No first message yet: it may still become ours; wait.
        assert_eq!(pick(&[thread("a", "")], Some("fix it"), false), None);
        // No prompt to go by: the only thread, unless a sibling also waits.
        assert_eq!(pick(&one, None, false).as_deref(), Some("a"));
        assert_eq!(pick(&one, None, true), None);
        // Two that fit equally: neither.
        let two = [thread("a", "fix it"), thread("b", "fix it")];
        assert_eq!(pick(&two, Some("fix it"), false), None);
        assert_eq!(pick(&two, None, false), None);
        assert_eq!(pick(&[], None, false), None);
        // An older Codex (no first messages in its index): the only
        // thread in the folder, when nobody else waits there.
        let old = [codex::Thread {
            first_user_message: None,
            ..thread("a", "")
        }];
        assert_eq!(pick(&old, Some("fix it"), false).as_deref(), Some("a"));
        assert_eq!(pick(&old, Some("fix it"), true), None);
    }
}
