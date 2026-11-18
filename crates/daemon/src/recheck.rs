//! Stored statuses re-checked against live evidence. Plain code, no AI.
//! The rules, and why "needs you" is cleared only when the user moved on:
//! "Stale statuses re-checked" in `docs/architecture.md`.
//!
//! Bounded: one `tmux list-panes -a` a pass, and a transcript tail read
//! (at most `transcript::tail`'s window) only for a session whose status
//! has sat still for `QUIET_MS`. Nothing is read under the DB lock; a fix
//! is written only if the status is still the one judged.

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
        let mut memo = Memo::default();
        loop {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pass(
                    &d,
                    &mut memo,
                    &tmux::agent_panes,
                    &tmux::pid_alive,
                    now_ms(),
                )
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

/// Per session: answered looks in a row its pane looked gone, and its
/// `last_state_change_at` when the first of them was made.
pub(crate) type Gone = HashMap<String, (u8, Option<i64>)>;

/// Count one look at session `s`'s pane. The count starts again when the
/// look isn't dead, and when the status changed since the first dead look
/// (a restart or a respawn in between: that pane isn't the one seen dead).
fn count_look(s: &Session, gone: &mut Gone, dead: bool) -> u8 {
    let e = gone
        .entry(s.id.clone())
        .or_insert((0, s.last_state_change_at));
    if !dead {
        *e = (0, s.last_state_change_at);
    } else if e.0 == 0 || e.1 != s.last_state_change_at {
        *e = (1, s.last_state_change_at);
    } else {
        e.0 = e.0.saturating_add(1);
    }
    e.0
}

/// What passes remember.
#[derive(Default)]
pub(crate) struct Memo {
    /// How many answered looks in a row each session's pane looked gone,
    /// and its status change time at the first of them.
    gone: Gone,
    /// The tmux server's pid, as it last said: when no server answers, it
    /// is gone only if that process is.
    server_pid: Option<i32>,
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
    /// Someone other than the session's own conversation is named as
    /// waiting (a sub-agent, another thread): `methods::needs_you_key`.
    pub others_waiting: bool,
    /// The transcript's tail (None: not read, or nothing to read).
    pub tail: Option<Tail>,
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
    // Written after the status began.
    let after = |t: Option<i64>| matches!((t, e.state_since), (Some(t), Some(s)) if t > s);
    // Nothing written lately (else the agent is alive and its hooks will
    // say what it is doing).
    let quiet = tail.last_at.is_some_and(|at| e.now - at >= QUIET_MS);
    let ended = |turn: Option<TurnTail>| match turn {
        Some(TurnTail::Ended) => Some("turn_ended"),
        Some(TurnTail::Interrupted) => Some("interrupted"),
        _ => None,
    };
    if e.status == Status::WaitingInput {
        // Never from the transcript alone: a prompt can wait silently for
        // hours (on the main agent or a sub-agent). Only once the user
        // clearly moved on, and what followed is over and quiet: Escape at
        // the prompt (an interrupt written after the wait began), or a
        // prompt of theirs after it whose turn ended. Anything still going:
        // the next pass decides.
        // (A sub-agent or another thread waiting: theirs to clear.)
        if e.others_waiting {
            return None;
        }
        let escaped = tail.turn == Some(TurnTail::Interrupted) && after(tail.turn_at);
        let moved = after(tail.prompt_at) && ended(tail.turn).is_some() && after(tail.turn_at);
        return (quiet && (escaped || moved)).then_some((Status::Idle, "moved_on"));
    }
    // Working: its turn's end (written after the status change: else it
    // is the previous turn's), and quiet since.
    let why = ended(tail.turn)?;
    (quiet && after(tail.turn_at)).then_some((Status::Idle, why))
}

/// One pass. `panes` is one look at every session's agent pane
/// (`tmux::agent_panes`), `alive` asks whether a process is there (tests
/// pass their own). With no server answering, the sessions are gone only
/// if the server process seen before is gone too (a deleted socket file
/// with the server alive proves nothing). Any other tmux failure: no pane
/// verdict this pass. One session's failure is logged and the pass goes
/// on. Returns how many statuses were corrected.
pub(crate) fn pass(
    d: &Daemon,
    memo: &mut Memo,
    panes: &dyn Fn() -> Result<Option<tmux::AgentPanes>>,
    alive: &dyn Fn(i32) -> bool,
    now: i64,
) -> Result<usize> {
    let sessions: Vec<Session> = d
        .db()
        .running_sessions()?
        .into_iter()
        .filter(checked)
        .collect();
    memo.gone
        .retain(|id, _| sessions.iter().any(|s| &s.id == id));
    let dead = match panes() {
        Ok(Some(p)) => {
            memo.server_pid = p.server_pid.or(memo.server_pid);
            Some(p.dead)
        }
        // No server answers, and the one seen before has exited: every
        // session is gone.
        Ok(None) if memo.server_pid.is_some_and(|pid| !alive(pid)) => Some(HashMap::new()),
        r => {
            if let Err(e) = r {
                tracing::debug!(error = %e, "tmux couldn't say: no pane verdicts this pass");
            }
            // (Two looks in a row means two that were answered.)
            memo.gone.clear();
            None
        }
    };
    let mut fixed = 0;
    for s in &sessions {
        match check(d, s, dead.as_ref(), &mut memo.gone, now) {
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
    gone: &mut Gone,
    now: i64,
) -> Result<bool> {
    // (Codex's background server runs the turn without the pane:
    // `discover` watches those panes.)
    let pane = match (panes, s.tmux_session.as_deref()) {
        (Some(p), Some(t)) if !s.codex_daemon => {
            // Not listed: its tmux session is gone.
            let dead = p.get(t).copied().unwrap_or(true);
            Some(count_look(s, gone, dead))
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
    let others_waiting = s.status == Status::WaitingInput && {
        let named: Vec<String> = d
            .db()
            .kv_get(&crate::methods::needs_you_key(&s.id))?
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        named.iter().any(|p| !p.is_empty())
    };
    let e = Evidence {
        status: s.status,
        status_age,
        state_since: s.last_state_change_at,
        pane_gone: pane,
        others_waiting,
        tail,
        now,
    };
    let Some((to, why)) = verdict(&e) else {
        return Ok(false);
    };
    let db = d.db();
    // (The one stop path for a gone pane: `discover`'s.)
    if to == Status::Stopped {
        // (The same compare-and-set as any fix, under this lock.)
        if !unchanged(&db, s)? {
            return Ok(false);
        }
        return crate::discover::stop_from_pane(&db, s);
    }
    // A turn found ended: its final reply is the last message.
    let ended_at = tail
        .filter(|t| to == Status::Idle && t.turn == Some(TurnTail::Ended))
        .and_then(|t| t.turn_at);
    let done = apply(&db, s, to, why, now, ended_at)?;
    // The user moved on: their prompt is their latest message (forward
    // only). Only a prompt shown to be theirs: one they typed (not a
    // Codex turn start), with no prompt of rlord's since the wait began
    // (`own.rs`: rlord types into sessions too). Else left as it is.
    let since = s.last_state_change_at;
    let rlord_typed = s
        .last_rlord_msg_at
        .is_some_and(|r| since.is_some_and(|st| r > st));
    let prompted = tail
        .and_then(|t| t.typed_prompt_at)
        .filter(|at| why == "moved_on" && !rlord_typed && since.is_some_and(|st| *at > st));
    if let Some(at) = prompted.filter(|at| done && s.last_user_msg_at.is_none_or(|m| *at > m)) {
        db.touch_last_user_msg(&s.id, at)?;
    }
    Ok(done)
}

/// Whether session `s` is still as it was read: its status, when that
/// last changed, and its message times. (A hook that kept the status but
/// moved a message, e.g. a prompt while working, is news too: the
/// evidence is stale.) Asked under the same DB lock as the write.
fn unchanged(db: &Db, s: &Session) -> Result<bool> {
    Ok(db.resolve_ref(&s.id).is_ok_and(|cur| {
        cur.status == s.status
            && cur.last_state_change_at == s.last_state_change_at
            && cur.last_user_msg_at == s.last_user_msg_at
            && cur.last_agent_msg_at == s.last_agent_msg_at
            && cur.last_rlord_msg_at == s.last_rlord_msg_at
    }))
}

/// Write the corrected status, only if the session is still as judged:
/// its status and its message times (an event may have moved them
/// meanwhile). Returns whether it
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
    if !unchanged(db, s)? {
        return Ok(false);
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
            others_waiting: false,
            tail,
            now: 100 * MIN,
        }
    }

    /// A tail whose last turn line (and last line) is `ago` old, and whose
    /// latest prompt is `prompt_ago` old.
    fn tail_p(turn: TurnTail, ago: i64, prompt_ago: Option<i64>) -> Option<Tail> {
        Some(Tail {
            turn: Some(turn),
            turn_at: Some(100 * MIN - ago),
            prompt_at: prompt_ago.map(|p| 100 * MIN - p),
            typed_prompt_at: prompt_ago.map(|p| 100 * MIN - p),
            last_at: Some(100 * MIN - ago),
        })
    }

    fn tail(turn: TurnTail, ago: i64) -> Option<Tail> {
        tail_p(turn, ago, None)
    }

    #[test]
    fn the_evidence_decides_only_when_it_is_unambiguous() {
        use Status::*;
        use TurnTail::*;
        let long = 158 * 60 * MIN;
        // Needs you is never cleared from the transcript alone: the
        // audit's case (needs you set after the turn's final reply, days
        // ago) is left for the user; so is a prompt waiting silently on a
        // sub-agent, and a real question waiting for hours.
        assert_eq!(verdict(&ev(WaitingInput, long, tail(Ended, long))), None);
        // An interrupt from before the wait: not this one.
        assert_eq!(
            verdict(&ev(WaitingInput, 5 * MIN, tail(Interrupted, 6 * MIN))),
            None
        );
        // Escape at the prompt: an interrupt written after the wait began,
        // quiet since: the user moved on.
        assert_eq!(
            verdict(&ev(WaitingInput, 5 * MIN, tail(Interrupted, 3 * MIN))),
            Some((Idle, "moved_on"))
        );
        assert_eq!(
            verdict(&ev(WaitingInput, 7 * 60 * MIN, tail(Open, 7 * 60 * MIN))),
            None
        );
        // A prompt from before the wait: not moving on.
        assert_eq!(
            verdict(&ev(
                WaitingInput,
                long,
                tail_p(Ended, long, Some(long + MIN))
            )),
            None
        );
        // The user moved on: a prompt of theirs after the wait began, and
        // its turn over and quiet: idle. Still going, or not quiet yet:
        // nothing (the next pass decides).
        assert_eq!(
            verdict(&ev(
                WaitingInput,
                long,
                tail_p(Ended, 3 * MIN, Some(4 * MIN))
            )),
            Some((Idle, "moved_on"))
        );
        assert_eq!(
            verdict(&ev(
                WaitingInput,
                long,
                tail_p(Open, 3 * MIN, Some(4 * MIN))
            )),
            None
        );
        assert_eq!(
            verdict(&ev(
                WaitingInput,
                long,
                tail_p(Ended, MIN / 2, Some(4 * MIN))
            )),
            None
        );
        // A sub-agent or another thread named as waiting: theirs, even
        // when the user moved on in the main conversation.
        let mut other = ev(WaitingInput, long, tail_p(Ended, 3 * MIN, Some(4 * MIN)));
        other.others_waiting = true;
        assert_eq!(verdict(&other), None);
        let mut esc = ev(WaitingInput, 5 * MIN, tail(Interrupted, 3 * MIN));
        esc.others_waiting = true;
        assert_eq!(verdict(&esc), None);
        // Working: its turn's end, written after the status change, and
        // quiet since.
        assert_eq!(
            verdict(&ev(Working, 5 * MIN, tail(Interrupted, 3 * MIN))),
            Some((Idle, "interrupted"))
        );
        assert_eq!(
            verdict(&ev(Working, 5 * MIN, tail(Ended, 3 * MIN))),
            Some((Idle, "turn_ended"))
        );
        // An end from before the status change is the previous turn's.
        assert_eq!(verdict(&ev(Working, 5 * MIN, tail(Ended, 6 * MIN))), None);
        assert_eq!(
            verdict(&ev(Working, 5 * MIN, tail(Interrupted, 5 * MIN))),
            None
        );
        // A long tool call: open.
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
            prompt_at: None,
            typed_prompt_at: None,
            last_at: Some(0),
        });
        assert_eq!(verdict(&ev(Working, 5 * MIN, no_turn)), None);
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
            // Needs you, set after the turn's final reply, days ago: left
            // for the user (never cleared from the transcript alone).
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
            // The same, but a sub-agent is named as waiting: left alone.
            let moved_sub = open_at("2026-11-10T00:00:05Z")
                + &line(json!({"type": "user", "timestamp": "2026-11-10T00:01:00Z",
                    "message": {"role": "user", "content": "never mind"}}))
                + &line(
                    json!({"type": "assistant", "timestamp": "2026-11-10T00:02:00Z",
                    "message": {"stop_reason": "end_turn", "content": []}}),
                );
            add(
                &db,
                dir.path(),
                "subwait",
                "claude",
                Status::WaitingInput,
                t0,
                &moved_sub,
            );
            db.kv_set(&crate::methods::needs_you_key("subwait"), r#"["agent-7"]"#)
                .unwrap();
            // Needs you, then the user typed a new prompt (its hooks lost)
            // and that turn ended: they moved on.
            let moved = open_at("2026-11-10T00:00:05Z")
                + &line(json!({"type": "user", "timestamp": "2026-11-10T00:01:00Z",
                    "message": {"role": "user", "content": "never mind, do this instead"}}))
                + &line(
                    json!({"type": "assistant", "timestamp": "2026-11-10T00:02:00Z",
                    "message": {"stop_reason": "end_turn", "content": [{"type": "text", "text": "done"}]}}),
                );
            add(
                &db,
                dir.path(),
                "moved",
                "claude",
                Status::WaitingInput,
                t0,
                &moved,
            );
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
        // tmux couldn't say: no pane verdicts, the transcript checks go on.
        let broken = || -> Result<Option<tmux::AgentPanes>> { anyhow::bail!("lost server") };
        let mut memo = Memo::default();
        let alive = |_: i32| true;
        assert_eq!(pass(&d, &mut memo, &broken, &alive, now).unwrap(), 3);
        let status = |id: &str| d.db().resolve_ref(id).unwrap().status;
        assert_eq!(status("stale"), Status::WaitingInput);
        assert_eq!(status("asking"), Status::WaitingInput);
        assert_eq!(status("subwait"), Status::WaitingInput);
        assert_eq!(status("moved"), Status::Idle);
        assert_eq!(status("esc"), Status::Idle);
        assert_eq!(status("cxh"), Status::Idle);
        // A turn found ended dates the session's last message, as a Stop
        // would have; an interrupt (Claude's, or Codex's turn_aborted)
        // doesn't.
        let at = |id: &str| d.db().resolve_ref(id).unwrap().last_agent_msg_at;
        // Moving on dates the user's last message from their prompt.
        assert_eq!(
            d.db().resolve_ref("moved").unwrap().last_user_msg_at,
            transcript::parse_iso_ms("2026-11-10T00:01:00Z")
        );
        assert_eq!(
            at("moved"),
            transcript::parse_iso_ms("2026-11-10T00:02:00Z")
        );
        assert_eq!(at("esc"), None);
        assert_eq!(at("cxh"), None);
        assert_eq!(status("oc"), Status::WaitingInput);
        assert_eq!(status("cx"), Status::Working);
        // No look at the panes so far.
        assert_eq!(status("dead"), Status::Idle);
        assert!(memo.gone.is_empty());
        // The blocked set went with the wait, and the fix is logged.
        let db = d.db();
        assert_eq!(
            db.kv_get(&crate::methods::needs_you_key("stale"))
                .unwrap()
                .as_deref(),
            Some(r#"[""]"#)
        );
        let log = db.list_events("moved", 5).unwrap();
        let last = log.last().unwrap();
        assert_eq!(last.kind, "status_rechecked");
        assert_eq!(last.payload["from"], "waiting_input");
        assert_eq!(last.payload["why"], "moved_on");
        let log = db.list_events("cxh", 5).unwrap();
        assert_eq!(log.last().unwrap().payload["why"], "interrupted");
        drop(db);
        // Every agent pane alive but one; "vanished" isn't listed.
        let panes = || -> Result<Option<tmux::AgentPanes>> {
            Ok(Some(tmux::AgentPanes {
                dead: [
                    "stale", "asking", "subwait", "moved", "esc", "oc", "cx", "cxh",
                ]
                .iter()
                .map(|t| (t.to_string(), false))
                .chain([("dead".to_string(), true)])
                .collect(),
                server_pid: Some(4242),
            }))
        };
        assert_eq!(
            pass(&d, &mut memo, &panes, &alive, now + 30_000).unwrap(),
            0
        );
        assert_eq!(status("dead"), Status::Idle, "one look isn't proof");
        // A failed look in between starts the count again.
        let broken = || -> Result<Option<tmux::AgentPanes>> { anyhow::bail!("lost server") };
        pass(&d, &mut memo, &broken, &alive, now + 40_000).unwrap();
        assert!(memo.gone.is_empty());
        assert_eq!(
            pass(&d, &mut memo, &panes, &alive, now + 50_000).unwrap(),
            0
        );
        // The second look stops both, by `discover`'s stop; nothing else
        // changes again.
        assert_eq!(
            pass(&d, &mut memo, &panes, &alive, now + 60_000).unwrap(),
            2
        );
        assert_eq!(status("dead"), Status::Stopped);
        assert_eq!(status("vanished"), Status::Stopped);
        let last = d.db().list_events("dead", 1).unwrap();
        assert_eq!(
            (last[0].kind.as_str(), &last[0].payload["from"]),
            ("session_end", &json!("pane"))
        );
        assert_eq!(
            pass(&d, &mut memo, &panes, &alive, now + 120_000).unwrap(),
            0
        );
    }

    /// No server answers: the sessions are gone only if the server process
    /// seen before has exited (a deleted socket file with the server
    /// alive proves nothing).
    #[test]
    fn a_dead_server_is_proven_before_anything_stops() {
        let dir = tempfile::tempdir().unwrap();
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        add(&d.db(), dir.path(), "a", "claude", Status::Idle, 1, "");
        let mut memo = Memo::default();
        let up = || -> Result<Option<tmux::AgentPanes>> {
            Ok(Some(tmux::AgentPanes {
                dead: [("a".to_string(), false)].into(),
                server_pid: Some(4242),
            }))
        };
        let no_answer = || -> Result<Option<tmux::AgentPanes>> { Ok(None) };
        let status = || d.db().resolve_ref("a").unwrap().status;
        // Never seen a server: nothing to prove.
        for _ in 0..3 {
            pass(&d, &mut memo, &no_answer, &|_| false, 10).unwrap();
        }
        assert_eq!(status(), Status::Idle);
        pass(&d, &mut memo, &up, &|_| true, 20).unwrap();
        // Its process still there (the socket file gone): no verdict.
        for _ in 0..3 {
            pass(&d, &mut memo, &no_answer, &|pid| pid == 4242, 30).unwrap();
        }
        assert_eq!(status(), Status::Idle);
        assert!(memo.gone.is_empty());
        // Its process gone: every session gone, on two looks.
        pass(&d, &mut memo, &no_answer, &|_| false, 40).unwrap();
        assert_eq!(status(), Status::Idle);
        pass(&d, &mut memo, &no_answer, &|_| false, 50).unwrap();
        assert_eq!(status(), Status::Stopped);
    }

    /// The look count starts again when the status changed since the first
    /// dead look (a restart or respawn in between).
    #[test]
    fn a_restart_between_looks_starts_the_count_again() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        add(&db, dir.path(), "s", "claude", Status::Idle, 5, "");
        let mut gone = Gone::new();
        let s1 = db.resolve_ref("s").unwrap();
        assert_eq!(count_look(&s1, &mut gone, true), 1);
        // Restarted: a new status change.
        db.set_status("s", Status::Starting, 9).unwrap();
        db.set_status("s", Status::Idle, 10).unwrap();
        let s2 = db.resolve_ref("s").unwrap();
        assert_eq!(
            count_look(&s2, &mut gone, true),
            1,
            "not the pane seen dead"
        );
        assert_eq!(count_look(&s2, &mut gone, true), 2);
        assert_eq!(count_look(&s2, &mut gone, false), 0);
    }

    /// Moving on dates the user's last message only from a prompt shown to
    /// be theirs: not when rlord typed into the session since the wait
    /// began.
    #[test]
    fn rlords_prompts_dont_date_the_users_message() {
        let dir = tempfile::tempdir().unwrap();
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let t0 = transcript::parse_iso_ms("2026-11-10T00:00:10Z").unwrap();
        let body = open_at("2026-11-10T00:00:05Z")
            + &line(json!({"type": "user", "timestamp": "2026-11-10T00:01:00Z",
                "message": {"role": "user", "content": "continue"}}))
            + &line(
                json!({"type": "assistant", "timestamp": "2026-11-10T00:02:00Z",
                "message": {"stop_reason": "end_turn", "content": [{"type": "text", "text": "done"}]}}),
            );
        add(
            &d.db(),
            dir.path(),
            "r",
            "claude",
            Status::WaitingInput,
            t0,
            &body,
        );
        // rlord typed "continue" (a resume after a limit) after the wait.
        d.db().touch_last_rlord_msg("r", t0 + 40_000).unwrap();
        let s = d.db().resolve_ref("r").unwrap();
        let now = t0 + 3_600_000;
        assert!(check(&d, &s, None, &mut Gone::new(), now).unwrap());
        let after = d.db().resolve_ref("r").unwrap();
        assert_eq!(after.status, Status::Idle, "the status fix still applies");
        assert_eq!(after.last_user_msg_at, None, "not the user's");
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

    /// A prompt that kept the status (working, working) still makes the
    /// evidence stale.
    #[test]
    fn a_message_meanwhile_is_news_too() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        add(&db, dir.path(), "s", "claude", Status::Working, 5, "");
        let seen = db.resolve_ref("s").unwrap();
        db.touch_last_user_msg("s", 7).unwrap();
        assert!(!apply(&db, &seen, Status::Idle, "turn_ended", 8, None).unwrap());
        assert_eq!(db.resolve_ref("s").unwrap().status, Status::Working);
    }

    /// The pane-stop path has the same compare-and-set: a message moved
    /// since the session was read, and nothing is stopped.
    #[test]
    fn a_pane_stop_checks_what_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        add(&d.db(), dir.path(), "s", "claude", Status::Idle, 5, "");
        let seen = d.db().resolve_ref("s").unwrap();
        let dead: HashMap<String, bool> = [("s".to_string(), true)].into();
        let mut gone = HashMap::new();
        assert!(!check(&d, &seen, Some(&dead), &mut gone, 10).unwrap());
        // A prompt arrives (the status stays) before the second look.
        d.db().touch_last_user_msg("s", 11).unwrap();
        assert!(!check(&d, &seen, Some(&dead), &mut gone, 12).unwrap());
        assert_eq!(d.db().resolve_ref("s").unwrap().status, Status::Idle);
        // Read again: stopped.
        let fresh = d.db().resolve_ref("s").unwrap();
        assert!(check(&d, &fresh, Some(&dead), &mut gone, 13).unwrap());
        assert_eq!(d.db().resolve_ref("s").unwrap().status, Status::Stopped);
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
