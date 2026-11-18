//! Stored statuses re-checked against live evidence. Plain code, no AI.
//!
//! A session's status is stored once and moved only by the events that
//! change it (hooks, OpenCode's server, `discover`). When a clearing event
//! never comes, the stored status stays wrong until the next one: a
//! session read "needs you" for days after a notification that should
//! never have set it, a turn the user interrupted reads "working" (Claude
//! runs no `Stop` hook on Escape), and an agent that crashed without its
//! `SessionEnd` leaves a dead pane behind a live status.
//!
//! Once at daemon start and then once a minute, this checks the sessions
//! that follow hooks (Claude; Codex once its hooks are trusted) against
//! what is there to see:
//!
//! - **The pane.** Its tmux session gone, or its program exited (dead
//!   pane), on two looks in a row: stopped.
//! - **The transcript's tail.** `waiting_input` or `working` that hasn't
//!   moved for `QUIET_MS`, while the transcript says the last turn ended
//!   (a final reply) or was interrupted and nothing was written since:
//!   idle. A turn still open (a tool call waiting on a prompt, or running)
//!   is left alone, however long it waits.
//!
//! Left alone, too: a session a sub-agent may be blocking (one is named as
//! waiting, or a background sub-agent is still open: its prompt can come
//! after the main turn ended); sessions `discover` or `opencode` follow
//! (they read the same evidence on their own tick); a session on Codex's
//! background server for the pane check (its turn outlives the pane).
//!
//! Bounded: one tmux look per live session a pass, and a transcript tail
//! read (at most `transcript::tail`'s window) only for a session whose
//! status has sat still for `QUIET_MS`. Nothing is read under the DB lock;
//! a fix is written only if the status is still the one judged.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use rlord_core::agents::transcript::{self, Tail, TurnTail};
use rlord_core::agents::Agent;
use rlord_core::db::Db;
use rlord_core::tmux;
use rlord_core::types::{now_ms, Session, Status};

use crate::Daemon;

/// Time between passes.
const PASS: Duration = Duration::from_secs(60);

/// How long a status and its transcript must have sat still before the
/// transcript can overrule it: a hook in flight (a `Stop` just after the
/// final reply is written) settles well inside it.
pub(crate) const QUIET_MS: i64 = 120_000;

/// Start the checker: a pass now (after `Daemon::startup`'s reconcile),
/// then one every `PASS`.
pub fn start(d: &Arc<Daemon>) {
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        // How many passes in a row each session's pane has looked gone.
        let mut gone: HashMap<String, u8> = HashMap::new();
        loop {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pass(&d, &mut gone, &tmux::agent_panes_dead, now_ms())
            }));
            match r {
                Ok(Err(e)) => tracing::debug!(error = %e, "re-checking stored statuses"),
                Err(_) => tracing::warn!("re-checking stored statuses panicked"),
                Ok(Ok(_)) => {}
            }
            std::thread::sleep(PASS);
        }
    });
}

/// The sessions checked here: live, on an agent whose hooks report its
/// status, and not followed by another watcher.
fn checked(s: &Session) -> bool {
    match Agent::parse(&s.agent) {
        Some(Agent::Claude) => true,
        Some(Agent::Codex) => !crate::discover::is_unhooked(s),
        // (OpenCode's server events and resync follow it: `opencode.rs`.)
        Some(Agent::OpenCode) | None => false,
    }
}

/// What a pass saw of one session, for `verdict`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Evidence {
    pub status: Status,
    /// How long the status has been what it is (ms).
    pub status_age: i64,
    /// When it became what it is (ms).
    pub state_since: Option<i64>,
    /// Passes in a row its pane has looked gone (None: not checked).
    pub pane_gone: Option<u8>,
    /// The transcript's tail (None: not read, or nothing to read).
    pub tail: Option<Tail>,
    /// A sub-agent may be the one blocked on the user.
    pub subagent_may_block: bool,
    pub now: i64,
}

/// What the evidence says the status is, when it differs: the status and
/// why (logged).
pub(crate) fn verdict(e: &Evidence) -> Option<(Status, &'static str)> {
    if e.status == Status::Stopped {
        return None;
    }
    if e.pane_gone.is_some_and(|n| n >= 2) {
        return Some((Status::Stopped, "pane"));
    }
    if !matches!(e.status, Status::WaitingInput | Status::Working) || e.status_age < QUIET_MS {
        return None;
    }
    let tail = e.tail?;
    // Something was written lately: the agent is alive and the hooks will
    // say what it is doing.
    if tail.last_at.is_none_or(|at| e.now - at < QUIET_MS) {
        return None;
    }
    let why = match tail.turn? {
        TurnTail::Open => return None,
        TurnTail::Ended => "turn_ended",
        TurnTail::Interrupted => "interrupted",
    };
    // Working since a prompt: only a turn's end written after it says
    // that turn is over (one from before is the last turn's).
    if e.status == Status::Working
        && !matches!((tail.turn_at, e.state_since), (Some(end), Some(since)) if end > since)
    {
        return None;
    }
    if e.status == Status::WaitingInput && e.subagent_may_block {
        return None;
    }
    Some((Status::Idle, why))
}

/// One pass. `gone` counts each session's gone-looking panes across
/// passes; `panes` is one look at every session's agent pane
/// (`tmux::agent_panes_dead`; tests pass their own). When tmux can't say,
/// no pane verdict is made this pass (nothing is stopped on a guess); the
/// transcript checks go on. One session's failure is logged and the pass
/// goes on. Returns how many statuses were corrected.
pub(crate) fn pass(
    d: &Daemon,
    gone: &mut HashMap<String, u8>,
    panes: &dyn Fn() -> Result<HashMap<String, bool>>,
    now: i64,
) -> Result<usize> {
    let sessions: Vec<Session> = d
        .db()
        .running_sessions()?
        .into_iter()
        .filter(checked)
        .collect();
    gone.retain(|id, _| sessions.iter().any(|s| &s.id == id));
    let panes = match panes() {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::debug!(error = %e, "tmux couldn't say: no pane verdicts this pass");
            None
        }
    };
    let mut fixed = 0;
    for s in &sessions {
        match check(d, s, panes.as_ref(), gone, now) {
            Ok(true) => fixed += 1,
            Ok(false) => {}
            Err(e) => tracing::debug!(session = %s.id, error = %e, "re-checking a status"),
        }
    }
    if fixed > 0 {
        d.changed();
    }
    Ok(fixed)
}

/// One session's check (`pass`). Returns whether its status was corrected.
fn check(
    d: &Daemon,
    s: &Session,
    panes: Option<&HashMap<String, bool>>,
    gone: &mut HashMap<String, u8>,
    now: i64,
) -> Result<bool> {
    // (Codex's background server runs the turn without the pane:
    // `discover` watches those panes.)
    let pane = match (panes, s.tmux_session.as_deref()) {
        (Some(p), Some(t)) if !s.codex_daemon => {
            // Not listed: its tmux session is gone.
            let dead = p.get(t).copied().unwrap_or(true);
            Some(crate::discover::count_look(&s.id, gone, dead))
        }
        _ => None,
    };
    let status_age = now - s.last_state_change_at.unwrap_or(now);
    let quiet =
        matches!(s.status, Status::WaitingInput | Status::Working) && status_age >= QUIET_MS;
    let tail = if quiet {
        let path = d.db().transcript_source(&s.id)?;
        path.and_then(|p| transcript::tail(std::path::Path::new(&p)))
    } else {
        None
    };
    let subagent_may_block = quiet && subagent_may_block(d, &s.id)?;
    let e = Evidence {
        status: s.status,
        status_age,
        state_since: s.last_state_change_at,
        pane_gone: pane,
        tail,
        subagent_may_block,
        now,
    };
    let Some((to, why)) = verdict(&e) else {
        return Ok(false);
    };
    let db = d.db();
    // (The one stop path for a gone pane: `discover`'s.)
    if to == Status::Stopped {
        return crate::discover::stop_from_pane(&db, s);
    }
    let ended_at = (why == "turn_ended")
        .then(|| tail.and_then(|t| t.turn_at))
        .flatten();
    apply(&db, s, to, why, now, ended_at)
}

/// Whether a sub-agent of session `id` may be the one blocked on the user:
/// one is named as waiting (`methods::needs_you_key`), or a background
/// sub-agent is still open (its prompt, an `agent_needs_input`
/// notification, can come after the main turn). Open as `background.rs`
/// believes it: unfinished, with a write within its `STALE_MS` (Claude
/// doesn't always record a sub-agent's end).
fn subagent_may_block(d: &Daemon, id: &str) -> Result<bool> {
    let named: Vec<String> = d
        .db()
        .kv_get(&crate::methods::needs_you_key(id))?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default();
    Ok(named.iter().any(|p| !p.is_empty()) || crate::background::open_agents(d, id) > 0)
}

/// Write the corrected status, only if the session's status is still the
/// one judged (an event may have moved it meanwhile). Returns whether it
/// was written.
/// `ended_at`: when the agent's final reply was written, for a turn found
/// ended (its last message, as a `Stop` would have recorded).
fn apply(
    db: &Db,
    s: &Session,
    to: Status,
    why: &str,
    now: i64,
    ended_at: Option<i64>,
) -> Result<bool> {
    match db.resolve_ref(&s.id) {
        Ok(cur) if cur.status == s.status && cur.last_state_change_at == s.last_state_change_at => {
        }
        _ => return Ok(false),
    }
    db.set_status(&s.id, to, now)?;
    // (Never moved back: a later message already recorded stands.)
    if let Some(at) = ended_at.filter(|at| s.last_agent_msg_at.is_none_or(|m| *at > m)) {
        db.touch_last_agent_msg(&s.id, at)?;
    }
    // Nobody is blocked once it no longer waits.
    db.kv_delete(&crate::methods::needs_you_key(&s.id))?;
    let since = s.last_state_change_at.map(|t| now - t);
    db.insert_event(
        &s.id,
        now,
        "status_rechecked",
        &json!({"from": s.status.as_str(), "to": to.as_str(), "why": why, "after_ms": since})
            .to_string(),
    )?;
    tracing::info!(session = %s.id, from = s.status.as_str(), to = to.as_str(), why,
        "stored status corrected");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlord_core::config::Config;
    use serde_json::Value;

    const MIN: i64 = 60_000;

    fn ev(status: Status, status_age: i64, tail: Option<Tail>) -> Evidence {
        Evidence {
            status,
            status_age,
            state_since: Some(100 * MIN - status_age),
            pane_gone: Some(0),
            tail,
            subagent_may_block: false,
            now: 100 * MIN,
        }
    }

    fn tail(turn: TurnTail, ago: i64) -> Option<Tail> {
        Some(Tail {
            turn: Some(turn),
            turn_at: Some(100 * MIN - ago),
            last_at: Some(100 * MIN - ago),
        })
    }

    #[test]
    fn the_evidence_decides_only_when_it_is_unambiguous() {
        use Status::*;
        use TurnTail::*;
        let long = 158 * 60 * MIN;
        // The audit's case: "needs you" from an idle reminder after the
        // turn's final reply, days later.
        assert_eq!(
            verdict(&ev(WaitingInput, long, tail(Ended, long))),
            Some((Idle, "turn_ended"))
        );
        // Escape at the prompt, or mid-turn: the turn is over.
        assert_eq!(
            verdict(&ev(WaitingInput, 5 * MIN, tail(Interrupted, 5 * MIN))),
            Some((Idle, "interrupted"))
        );
        assert_eq!(
            verdict(&ev(Working, 5 * MIN, tail(Interrupted, 3 * MIN))),
            Some((Idle, "interrupted"))
        );
        assert_eq!(
            verdict(&ev(Working, 5 * MIN, tail(Ended, 3 * MIN))),
            Some((Idle, "turn_ended"))
        );
        // Working since a prompt that came after the last turn's end: that
        // end is the last turn's, not this one's (the prompt's own lines
        // not written yet, or out of the tail).
        assert_eq!(verdict(&ev(Working, 5 * MIN, tail(Ended, 6 * MIN))), None);
        assert_eq!(
            verdict(&ev(Working, 5 * MIN, tail(Interrupted, 5 * MIN))),
            None
        );
        // A real prompt waits with the turn open, however long (one real
        // question waited 7 h): never touched.
        assert_eq!(
            verdict(&ev(WaitingInput, 7 * 60 * MIN, tail(Open, 7 * 60 * MIN))),
            None
        );
        // A long tool call: open too.
        assert_eq!(verdict(&ev(Working, 30 * MIN, tail(Open, 30 * MIN))), None);
        // Too soon: a Stop may be on its way.
        assert_eq!(verdict(&ev(Working, MIN, tail(Ended, MIN))), None);
        // The status sat still but the transcript moved lately.
        assert_eq!(verdict(&ev(Working, 5 * MIN, tail(Ended, MIN))), None);
        // Nothing to read, or nothing about a turn in it.
        assert_eq!(verdict(&ev(Working, 5 * MIN, None)), None);
        let no_turn = Some(Tail {
            turn: None,
            turn_at: None,
            last_at: Some(0),
        });
        assert_eq!(verdict(&ev(Working, 5 * MIN, no_turn)), None);
        // A sub-agent may be the one asking: needs-you stays.
        let mut sub = ev(WaitingInput, long, tail(Ended, long));
        sub.subagent_may_block = true;
        assert_eq!(verdict(&sub), None);
        // (A working session's main turn ended is still idle.)
        let mut sub = ev(Working, long, tail(Ended, long - MIN));
        sub.subagent_may_block = true;
        assert_eq!(verdict(&sub), Some((Idle, "turn_ended")));
        // Other statuses are the hooks' alone.
        for st in [Idle, Starting, Error] {
            assert_eq!(verdict(&ev(st, long, tail(Ended, long))), None, "{st:?}");
        }
    }

    #[test]
    fn a_pane_gone_twice_stops_the_session() {
        let mut e = ev(Status::Working, 0, None);
        e.pane_gone = Some(1);
        assert_eq!(verdict(&e), None, "one look isn't proof");
        e.pane_gone = Some(2);
        assert_eq!(verdict(&e), Some((Status::Stopped, "pane")));
        e.status = Status::Idle;
        assert_eq!(verdict(&e), Some((Status::Stopped, "pane")));
        // Not looked at (on Codex's background server).
        e.pane_gone = None;
        assert_eq!(verdict(&e), None);
    }

    // ---- a pass over a database, with fixture transcripts ----

    fn line(v: Value) -> String {
        format!("{v}\n")
    }

    fn ended_at(ts: &str) -> String {
        line(json!({"type": "user", "timestamp": "2026-11-10T00:00:00Z",
            "message": {"role": "user", "content": "fix it"}}))
            + &line(json!({"type": "assistant", "timestamp": ts,
                "message": {"stop_reason": "end_turn", "content": [{"type": "text", "text": "done"}]}}))
    }

    fn open_at(ts: &str) -> String {
        line(json!({"type": "assistant", "timestamp": ts,
            "message": {"stop_reason": "tool_use", "content": [{"type": "tool_use", "name": "AskUserQuestion"}]}}))
    }

    fn add(
        db: &Db,
        dir: &std::path::Path,
        id: &str,
        agent: &str,
        status: Status,
        at: i64,
        body: &str,
    ) {
        db.insert_session(&rlord_core::db::NewSession {
            id,
            name: id,
            agent,
            agent_session: Some(id),
            project_path: "/tmp",
            tmux_session: Some(id),
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        db.set_status(id, Status::Starting, 1).unwrap();
        db.set_status(id, status, at).unwrap();
        let name = if agent == "codex" {
            format!("rollout-2026-11-10T00-00-00-{id}.jsonl")
        } else {
            format!("{id}.jsonl")
        };
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        db.upsert_transcript_source(id, &p.to_string_lossy())
            .unwrap();
    }

    #[test]
    fn a_pass_corrects_only_what_the_evidence_proves() {
        let dir = tempfile::tempdir().unwrap();
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let t0 = transcript::parse_iso_ms("2026-11-10T00:00:10Z").unwrap();
        let now = t0 + 158 * 3_600_000;
        {
            let db = d.db();
            // Needs you, set after the turn's final reply: stale.
            add(
                &db,
                dir.path(),
                "stale",
                "claude",
                Status::WaitingInput,
                t0 + 60_000,
                &ended_at("2026-11-10T00:00:10Z"),
            );
            db.kv_set(&crate::methods::needs_you_key("stale"), r#"[""]"#)
                .unwrap();
            // A question still open: real.
            add(
                &db,
                dir.path(),
                "asking",
                "claude",
                Status::WaitingInput,
                t0,
                &open_at("2026-11-10T00:00:10Z"),
            );
            // Stale, but a background sub-agent is believed open (it
            // wrote lately: `background`): it may be the one asking. One
            // not believed open any more doesn't hold the fix back.
            for id in ["bgsub", "bgold"] {
                add(
                    &db,
                    dir.path(),
                    id,
                    "claude",
                    Status::WaitingInput,
                    t0,
                    &ended_at("2026-11-10T00:00:10Z"),
                );
            }
            // Interrupted, reads working.
            let interrupted = open_at("2026-11-10T00:00:05Z")
                + &line(json!({"type": "user", "timestamp": "2026-11-10T00:00:10Z",
                    "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}}));
            add(
                &db,
                dir.path(),
                "esc",
                "claude",
                Status::Working,
                t0 - 5_000,
                &interrupted,
            );
            // OpenCode: its own watcher's.
            add(
                &db,
                dir.path(),
                "oc",
                "opencode",
                Status::WaitingInput,
                t0,
                "",
            );
            // Codex, hooks not trusted: `discover`'s.
            add(
                &db,
                dir.path(),
                "cx",
                "codex",
                Status::Working,
                t0,
                &line(
                    json!({"timestamp": "2026-11-10T00:00:10Z", "type": "event_msg", "payload": {"type": "task_complete"}}),
                ),
            );
            // Codex, hooks trusted, aborted turn: corrected here.
            add(
                &db,
                dir.path(),
                "cxh",
                "codex",
                Status::Working,
                t0 - 5_000,
                &line(
                    json!({"timestamp": "2026-11-10T00:00:10Z", "type": "event_msg", "payload": {"type": "turn_aborted"}}),
                ),
            );
            db.set_agent_session("cxh", "cxh", "hook").unwrap();
            // Idle with a dead pane; idle with its tmux session gone.
            add(&db, dir.path(), "dead", "claude", Status::Idle, t0, "");
            add(&db, dir.path(), "vanished", "claude", Status::Idle, t0, "");
        }
        crate::background::set_open_agents(&d, "bgsub", 1);
        // tmux couldn't say: no pane verdicts, the transcript checks go on.
        let broken = || -> Result<HashMap<String, bool>> { anyhow::bail!("lost server") };
        let mut gone = HashMap::new();
        assert_eq!(pass(&d, &mut gone, &broken, now).unwrap(), 4);
        let status = |id: &str| d.db().resolve_ref(id).unwrap().status;
        assert_eq!(status("stale"), Status::Idle);
        assert_eq!(status("esc"), Status::Idle);
        assert_eq!(status("cxh"), Status::Idle);
        assert_eq!(status("asking"), Status::WaitingInput);
        assert_eq!(status("bgsub"), Status::WaitingInput);
        assert_eq!(status("bgold"), Status::Idle);
        // A turn found ended dates the session's last message, as a Stop
        // would have; an interrupt doesn't.
        let at = |id: &str| d.db().resolve_ref(id).unwrap().last_agent_msg_at;
        assert_eq!(at("stale"), Some(t0));
        assert_eq!(at("esc"), None);
        assert_eq!(status("oc"), Status::WaitingInput);
        assert_eq!(status("cx"), Status::Working);
        // No look at the panes so far.
        assert_eq!(status("dead"), Status::Idle);
        assert!(gone.is_empty());
        // The blocked set went with the wait, and the fix is logged.
        let db = d.db();
        assert_eq!(
            db.kv_get(&crate::methods::needs_you_key("stale")).unwrap(),
            None
        );
        let log = db.list_events("stale", 5).unwrap();
        let last = log.last().unwrap();
        assert_eq!(last.kind, "status_rechecked");
        assert_eq!(last.payload["from"], "waiting_input");
        assert_eq!(last.payload["why"], "turn_ended");
        drop(db);
        // Every agent pane alive but one; "vanished" isn't listed.
        let panes = || -> Result<HashMap<String, bool>> {
            Ok([
                "stale", "asking", "bgsub", "bgold", "esc", "oc", "cx", "cxh",
            ]
            .iter()
            .map(|t| (t.to_string(), false))
            .chain([("dead".to_string(), true)])
            .collect())
        };
        assert_eq!(pass(&d, &mut gone, &panes, now + 30_000).unwrap(), 0);
        assert_eq!(status("dead"), Status::Idle, "one look isn't proof");
        // The second look stops both, by `discover`'s stop; nothing else
        // changes again.
        assert_eq!(pass(&d, &mut gone, &panes, now + 60_000).unwrap(), 2);
        assert_eq!(status("dead"), Status::Stopped);
        assert_eq!(status("vanished"), Status::Stopped);
        let last = d.db().list_events("dead", 1).unwrap();
        assert_eq!(
            (last[0].kind.as_str(), &last[0].payload["from"]),
            ("session_end", &json!("pane"))
        );
        assert_eq!(pass(&d, &mut gone, &panes, now + 120_000).unwrap(), 0);
    }

    /// A turn's end found later never moves the last message back.
    #[test]
    fn the_last_message_only_moves_forward() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        add(&db, dir.path(), "s", "claude", Status::Working, 5, "");
        db.touch_last_agent_msg("s", 50).unwrap();
        let seen = db.resolve_ref("s").unwrap();
        assert!(apply(&db, &seen, Status::Idle, "turn_ended", 60, Some(40)).unwrap());
        assert_eq!(db.resolve_ref("s").unwrap().last_agent_msg_at, Some(50));
    }

    #[test]
    fn a_status_that_moved_meanwhile_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        add(&db, dir.path(), "s", "claude", Status::Working, 5, "");
        let seen = db.resolve_ref("s").unwrap();
        // A prompt arrives between the look and the write.
        db.set_status("s", Status::Idle, 6).unwrap();
        db.set_status("s", Status::Working, 7).unwrap();
        assert!(!apply(&db, &seen, Status::Idle, "turn_ended", 8, None).unwrap());
        assert_eq!(db.resolve_ref("s").unwrap().status, Status::Working);
    }
}
