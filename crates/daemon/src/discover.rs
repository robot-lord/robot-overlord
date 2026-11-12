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

/// Live sessions of agents that assign their own id, not confirmed by a
/// hook; and confirmed ones that are still "starting" (a resumed Codex runs
/// no hook until its next prompt: see `follow`).
fn unhooked(d: &Daemon) -> Result<Vec<Session>> {
    Ok(d.db()
        .running_sessions()?
        .into_iter()
        .filter(|s| Agent::parse(&s.agent).is_some_and(|a| !a.mints_id()))
        .filter(|s| {
            s.agent_session.is_none()
                || s.agent_session_trust != "hook"
                || s.status == Status::Starting
        })
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
            changed |= bind(d, s, others)? || first_prompt_due(d, s)?;
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

/// How long after a launch without a prompt the agent's screen is taken
/// to be up.
const UP_AFTER_MS: i64 = 4_000;

/// A session launched without a prompt has nothing to report until its
/// first one: Codex creates the conversation (and runs SessionStart) only
/// then. So once its pane has been up a moment it is the user's turn, not
/// "starting" for ever. (The overlord starts this way: `ask` waits for it
/// to leave "starting" before typing into it.) Returns whether it changed.
fn first_prompt_due(d: &Daemon, s: &Session) -> Result<bool> {
    if s.status != Status::Starting {
        return Ok(false);
    }
    let (launched_bare, since) = {
        let db = d.db();
        let launched_bare = db
            .kv_get(&methods::bind_prompt_key(&s.id))?
            .is_some_and(|p| p.trim().is_empty());
        let since = db
            .kv_get(&methods::bind_since_key(&s.id))?
            .and_then(|v| v.parse::<i64>().ok());
        (launched_bare, since)
    };
    let now = now_ms();
    if !launched_bare || !since.is_some_and(|t| up_long_enough(t, now)) {
        return Ok(false);
    }
    // Up means its composer is on screen, not a startup dialog in front of
    // it (where typed text would land on the wrong thing). Read without
    // the DB lock.
    if !composer_up(s) {
        return Ok(false);
    }
    let db = d.db();
    // Moved on meanwhile (a hook, a restart): not ours to say.
    let cur = db.resolve_ref(&s.id)?;
    if cur.status != Status::Starting || cur.agent_session.is_some() {
        return Ok(false);
    }
    db.set_status(&s.id, Status::Idle, now)?;
    db.insert_event(&s.id, now, "awaiting_first_prompt", r#"{"from":"pane"}"#)?;
    Ok(true)
}

fn up_long_enough(launched_ms: i64, now_ms: i64) -> bool {
    now_ms - launched_ms >= UP_AFTER_MS
}

/// The session's agent pane shows Codex's composer (`codex::composer_shown`).
fn composer_up(s: &Session) -> bool {
    s.tmux_session
        .as_deref()
        .and_then(|t| tmux::capture_pane(t).ok())
        .is_some_and(|screen| codex::composer_shown(&screen))
}

/// Find the conversation a session launched, in its agent's index (see
/// `pick_thread`). `others`: another session waits for one in this folder.
fn bind(d: &Daemon, s: &Session, others: bool) -> Result<bool> {
    let (since, prompt, tracked) = bind_inputs(&d.db(), s)?;
    // (Codex's index is read without the DB lock.)
    let candidates = untracked_threads(s, since, &tracked);
    let Some(t) = pick_thread(&candidates, prompt.as_deref(), others) else {
        return Ok(false);
    };
    record_bind(&d.db(), s, t)
}

/// `bind`, once, under the caller's DB lock: for a stopped session about to
/// be started again (the overlord), whose thread may have turned up after
/// rlord last looked. Nobody else waits in the folder it is asked for.
pub(crate) fn bind_now(db: &Db, s: &Session) -> Result<bool> {
    let (since, prompt, tracked) = bind_inputs(db, s)?;
    let candidates = untracked_threads(s, since, &tracked);
    match pick_thread(&candidates, prompt.as_deref(), false) {
        Some(t) => record_bind(db, s, t),
        None => Ok(false),
    }
}

/// What the search for a session's thread goes by: started since when,
/// with which first prompt (if any), and the threads already taken.
fn bind_inputs(
    db: &Db,
    s: &Session,
) -> Result<(i64, Option<String>, std::collections::HashSet<String>)> {
    let since = db
        .kv_get(&methods::bind_since_key(&s.id))?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(s.created_at);
    let prompt = db
        .kv_get(&methods::bind_prompt_key(&s.id))?
        .filter(|p| !p.trim().is_empty());
    Ok((since, prompt, db.agent_session_ids()?))
}

fn untracked_threads(
    s: &Session,
    since: i64,
    tracked: &std::collections::HashSet<String>,
) -> Vec<codex::Thread> {
    codex::threads_started_in(&codex::home(), &s.project_path, since)
        .into_iter()
        .filter(|t| !tracked.contains(&t.id))
        .collect()
}

fn record_bind(db: &Db, s: &Session, t: &codex::Thread) -> Result<bool> {
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
///
/// A session the hooks have confirmed is theirs, except while it is still
/// "starting": Codex runs SessionStart for a resumed conversation only
/// with its next prompt, so a resume would stay "starting" until then (and
/// the overlord, which is typed into only once it has left "starting",
/// could not be asked anything). Once its pane has been up a moment, its
/// transcript says whether a turn is open.
fn follow(d: &Daemon, s: &Session) -> Result<bool> {
    let hooked = s.agent_session_trust == "hook";
    if hooked
        && (s.status != Status::Starting
            || !s
                .last_state_change_at
                .is_some_and(|t| up_long_enough(t, now_ms())))
    {
        return Ok(false);
    }
    let Some(src) = d.db().transcript_source(&s.id)? else {
        return Ok(false);
    };
    let path = Path::new(&src);
    // Still starting (just launched or resumed): only what the agent has
    // written since counts. A conversation killed mid-turn ends on a turn
    // that never closes; resumed, it waits for the user.
    let working = match (s.status, s.last_state_change_at) {
        (Status::Starting, Some(t)) => transcript::working_since(path, t),
        _ => transcript::working(path),
    };
    // Settling a start with nothing in the transcript: only once the
    // composer is up (not an update or trust prompt in front of it).
    if s.status == Status::Starting && working.is_none() && !composer_up(s) {
        return Ok(false);
    }
    let Some(next) = next_status(s.status, working) else {
        return Ok(false);
    };
    let db = d.db();
    methods::save_cursors(&db);
    // The hooks started reporting meanwhile: theirs stands.
    let cur = db.resolve_ref(&s.id)?;
    if (cur.agent_session_trust == "hook" && !hooked) || cur.status != s.status {
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
    fn a_pane_is_up_after_a_moment() {
        assert!(!up_long_enough(10_000, 10_000 + UP_AFTER_MS - 1));
        assert!(up_long_enough(10_000, 10_000 + UP_AFTER_MS));
    }

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
