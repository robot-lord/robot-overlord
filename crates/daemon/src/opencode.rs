//! OpenCode sessions' status, from OpenCode itself. Plain code, no AI.
//!
//! OpenCode has no command hooks. Its sessions run in one background
//! server per user, and the TUI in rlord's pane is only a client of it; so
//! this is the third status source, beside the hooks (`methods::hook_emit`)
//! and `discover`:
//!
//! - **The server's event stream** (one connection for every session, only
//!   the event types that matter): a turn starting and ending, a
//!   permission request or question and its answer. Events are matched to
//!   rlord's sessions by the conversation id rlord minted at launch.
//! - **Resync** after every (re)connect, since the stream has no replay:
//!   each session's turn state from OpenCode's own session row, and what it
//!   waits on from the server.
//! - **Without the server** (not running, or unreachable): working / your
//!   turn from the session rows alone, every tick. "Needs you" can't be
//!   seen that way.
//! - **Stopped** is the pane: the TUI exited (two looks in a row, as
//!   `discover` does). The turn itself lives in the server and may go on
//!   after the TUI is closed; rlord shows the session, which is stopped.
//!
//! Nothing runs while there are no live OpenCode sessions: no connection,
//! no reads. Nothing is written unless something changed. "Updated" times
//! move only on a prompt the user sent and on a turn's end.
//!
//! This task also applies what a launch couldn't put on the command line
//! (`agents::Deferred`: the model, then the first prompt), once OpenCode
//! has the session.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use rlord_core::agents::opencode::{self, Turn};
use rlord_core::agents::opencode_service::{map_event, Events, Service, Signal, EVENT_TYPES};
use rlord_core::agents::{Agent, Deferred};
use rlord_core::db::Db;
use rlord_core::tmux;
use rlord_core::types::{now_ms, Session, Status};

use crate::Daemon;

const TICK: Duration = Duration::from_secs(3);

/// The longest wait between attempts to reach the server.
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Where a launch's `Deferred` waits until OpenCode has the session.
pub(crate) fn deferred_key(id: &str) -> String {
    format!("deferred:{id}")
}

/// What each session waits on the user for (`perm:<id>`, `form:<id>`), by
/// rlord session id. Kept in memory: rebuilt from the server at resync.
type Blocked = HashMap<String, BTreeSet<String>>;

struct Follower {
    link: Option<(Service, Events)>,
    retry_at: Instant,
    backoff: Duration,
    blocked: Blocked,
    /// How many ticks in a row each session's pane has looked gone.
    gone: HashMap<String, u8>,
    /// OpenCode sessions' parents, as looked up (a sub-agent's session
    /// asks permission under its own id).
    parents: HashMap<String, Option<String>>,
}

/// Start the follower.
pub fn start(d: &Arc<Daemon>) {
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        let mut f = Follower {
            link: None,
            retry_at: Instant::now(),
            backoff: Duration::from_secs(1),
            blocked: HashMap::new(),
            gone: HashMap::new(),
            parents: HashMap::new(),
        };
        loop {
            let began = Instant::now();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pass(&d, &mut f)));
            match r {
                Ok(Err(e)) => tracing::debug!(error = %format!("{e:#}"), "following OpenCode"),
                Err(_) => {
                    tracing::warn!("following OpenCode panicked");
                    f.link = None;
                }
                Ok(Ok(())) => {}
            }
            // (A pass that failed early must not spin.)
            if let Some(rest) = Duration::from_millis(500).checked_sub(began.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    });
}

/// Live OpenCode sessions (each has the conversation id rlord minted).
fn live(d: &Daemon) -> Result<Vec<Session>> {
    Ok(d.db()
        .running_sessions()?
        .into_iter()
        .filter(|s| s.agent == Agent::OpenCode.name() && s.agent_session.is_some())
        .collect())
}

/// One tick: about `TICK` long, most of it waiting (for events, or asleep).
fn pass(d: &Daemon, f: &mut Follower) -> Result<()> {
    let live = live(d)?;
    f.gone.retain(|id, _| live.iter().any(|s| &s.id == id));
    f.blocked.retain(|id, _| live.iter().any(|s| &s.id == id));
    if live.is_empty() {
        f.link = None;
        f.parents.clear();
        std::thread::sleep(TICK);
        return Ok(());
    }
    let mut changed = panes(d, &live, &mut f.gone)?;
    let up: Vec<&Session> = live
        .iter()
        .filter(|s| f.gone.get(&s.id).is_none_or(|n| *n == 0))
        .collect();
    if f.link.is_none() && Instant::now() >= f.retry_at {
        match connect() {
            Ok((svc, events)) => {
                tracing::debug!(server = ?svc, "following OpenCode's events");
                f.backoff = Duration::from_secs(1);
                changed |= settle_all(d, Some(&svc), &up, &mut f.blocked)?;
                f.link = Some((svc, events));
            }
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "OpenCode's server not reachable");
                f.retry_at = Instant::now() + f.backoff;
                f.backoff = (f.backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
    match f.link.take() {
        Some((svc, mut events)) => {
            changed |= apply_deferred(d, &svc, &up)?;
            if changed {
                d.changed();
            }
            let until = Instant::now() + TICK;
            while Instant::now() < until {
                match events.poll() {
                    Ok(evs) => {
                        let mut changed = false;
                        for (id, signal) in evs.iter().filter_map(map_event) {
                            changed |= on_signal(d, f, &svc, &id, &signal)?;
                        }
                        if changed {
                            d.changed();
                        }
                    }
                    Err(e) => {
                        // Over: reconnect (soon) and resync.
                        tracing::debug!(error = %format!("{e:#}"), "OpenCode's event stream");
                        f.retry_at = Instant::now() + Duration::from_secs(1);
                        return Ok(());
                    }
                }
            }
            f.link = Some((svc, events));
        }
        None => {
            changed |= settle_all(d, None, &up, &mut f.blocked)?;
            if changed {
                d.changed();
            }
            std::thread::sleep(TICK);
        }
    }
    Ok(())
}

/// The server as `service.json` names it now, and its event stream.
fn connect() -> Result<(Service, Events)> {
    let svc = Service::read(&opencode::service_file()).context("no OpenCode server is running")?;
    let events = svc.events(EVENT_TYPES, TICK)?;
    Ok((svc, events))
}

/// Sessions whose TUI exited: stopped. One failed look isn't proof (tmux
/// busy, a restart in between): two in a row.
fn panes(d: &Daemon, live: &[Session], gone: &mut HashMap<String, u8>) -> Result<bool> {
    let mut changed = false;
    for s in live {
        let dead = s
            .tmux_session
            .as_deref()
            .is_some_and(|t| !tmux::has_session(t) || tmux::format(t, "#{pane_dead}") == "1");
        let looks = gone.entry(s.id.clone()).or_default();
        *looks = if dead { looks.saturating_add(1) } else { 0 };
        if *looks < 2 {
            continue;
        }
        let db = d.db();
        // (Restarted meanwhile: not this pane's end.)
        if db.resolve_ref(&s.id)?.status != s.status {
            continue;
        }
        let now = now_ms();
        db.set_status(&s.id, Status::Stopped, now)?;
        db.insert_event(&s.id, now, "session_end", r#"{"from":"pane"}"#)?;
        changed = true;
    }
    Ok(changed)
}

/// What a signal changes for a session.
#[derive(Debug, Default, PartialEq, Eq)]
struct Step {
    status: Option<Status>,
    /// The event to log (None: nothing happened worth a line).
    kind: Option<&'static str>,
    /// A prompt the user sent / a finished turn: the only things that move
    /// a session's "updated" times.
    user_msg: bool,
    agent_msg: bool,
}

/// What `signal` means for a session in `cur`, waiting on `blocked`.
fn step(cur: Status, blocked: &mut BTreeSet<String>, signal: &Signal) -> Step {
    use Status::*;
    let to = |want: Status| (want != cur).then_some(want);
    match signal {
        Signal::Created => Step {
            status: (cur == Starting).then_some(Idle),
            kind: (cur == Starting).then_some("session_start"),
            ..Default::default()
        },
        Signal::Prompt => Step {
            // (Sent while it waits on a permission: still waiting.)
            status: matches!(cur, Starting | Idle).then_some(Working),
            kind: Some("prompt_submitted"),
            user_msg: true,
            agent_msg: false,
        },
        Signal::Busy => {
            blocked.clear();
            Step {
                status: to(Working),
                kind: to(Working).map(|_| "turn_started"),
                ..Default::default()
            }
        }
        Signal::Ended => {
            blocked.clear();
            Step {
                status: to(Idle),
                kind: to(Idle).map(|_| "agent_stopped"),
                user_msg: false,
                agent_msg: matches!(cur, Working | WaitingInput),
            }
        }
        Signal::Asked(what) => {
            let new = blocked.insert(what.clone());
            Step {
                status: to(WaitingInput),
                kind: (new || cur != WaitingInput).then_some(if what.starts_with("form:") {
                    "question"
                } else {
                    "permission_request"
                }),
                ..Default::default()
            }
        }
        Signal::Answered(what) => {
            blocked.remove(what);
            // Working again once nothing is waited on.
            let moved = cur == WaitingInput && blocked.is_empty();
            Step {
                status: moved.then_some(Working),
                kind: moved.then_some("answered"),
                ..Default::default()
            }
        }
    }
}

/// Write a step. Returns whether anything changed.
fn write(db: &Db, s: &Session, step: &Step, from: &str) -> Result<bool> {
    let Some(kind) = step.kind else {
        return Ok(false);
    };
    let now = now_ms();
    if let Some(status) = step.status {
        db.set_status(&s.id, status, now)?;
    }
    if step.user_msg {
        db.touch_last_user_msg(&s.id, now)?;
    }
    if step.agent_msg {
        db.touch_last_agent_msg(&s.id, now)?;
    }
    db.insert_event(&s.id, now, kind, &json!({ "from": from }).to_string())?;
    Ok(true)
}

/// Apply one signal about a session rlord runs (`s`, as it is now).
fn apply(db: &Db, blocked: &mut Blocked, s: &Session, signal: &Signal) -> Result<bool> {
    if s.agent != Agent::OpenCode.name() || s.status == Status::Stopped || s.archived_at.is_some() {
        return Ok(false);
    }
    let waits = blocked.entry(s.id.clone()).or_default();
    let step = step(s.status, waits, signal);
    if waits.is_empty() {
        blocked.remove(&s.id);
    }
    write(db, s, &step, "opencode events")
}

/// The rlord session an OpenCode session id belongs to. A permission
/// request or question may come from a sub-agent's child session: those
/// are followed up to the session rlord launched.
fn owner(d: &Daemon, f: &mut Follower, id: &str, via_parents: bool) -> Result<Option<Session>> {
    let mut id = id.to_string();
    for _ in 0..6 {
        if let Some(s) = d.db().session_by_agent_session(&id)? {
            return Ok(Some(s));
        }
        if !via_parents {
            return Ok(None);
        }
        if f.parents.len() > 2000 {
            f.parents.clear();
        }
        let parent = f
            .parents
            .entry(id.clone())
            .or_insert_with(|| opencode::parent(&opencode::db_path(), &id))
            .clone();
        match parent {
            Some(p) => id = p,
            None => return Ok(None),
        }
    }
    Ok(None)
}

fn on_signal(
    d: &Daemon,
    f: &mut Follower,
    svc: &Service,
    id: &str,
    signal: &Signal,
) -> Result<bool> {
    let prompt = matches!(signal, Signal::Asked(_) | Signal::Answered(_));
    let Some(s) = owner(d, f, id, prompt)? else {
        return Ok(false); // not one of rlord's
    };
    let mut changed = apply(&d.db(), &mut f.blocked, &s, signal)?;
    if *signal == Signal::Created {
        changed |= apply_deferred(d, svc, &[&s])?;
    }
    Ok(changed)
}

/// The status a session should be in, from its turn state and (when the
/// server could be asked) whether it waits on the user; None when that is
/// where it is. Without the server, a session that needs you and is still
/// mid-turn is left alone: what it waits on can't be seen.
fn settle(cur: Status, turn: Turn, pending: Option<bool>) -> Option<Status> {
    let want = match (pending, turn.working()) {
        (Some(true), _) => Status::WaitingInput,
        (None, true) if cur == Status::WaitingInput => return None,
        (_, true) => Status::Working,
        (_, false) => Status::Idle,
    };
    (want != cur).then_some(want)
}

/// Bring every session in line with OpenCode's own record: after a
/// (re)connect (`svc`: what each waits on is asked too), and every tick
/// while there is no server. The reads happen without the DB lock.
fn settle_all(
    d: &Daemon,
    svc: Option<&Service>,
    up: &[&Session],
    blocked: &mut Blocked,
) -> Result<bool> {
    let store = opencode::db_path();
    let mut changed = false;
    for s in up {
        let Some(id) = s.agent_session.as_deref() else {
            continue;
        };
        // (No row: OpenCode hasn't created the session yet.)
        let Some(turn) = opencode::turn(&store, id) else {
            continue;
        };
        let pending = svc.and_then(|svc| svc.pending(id));
        if let Some(p) = &pending {
            if p.is_empty() {
                blocked.remove(&s.id);
            } else {
                blocked.insert(s.id.clone(), p.iter().cloned().collect());
            }
        }
        let Some(want) = settle(s.status, turn, pending.as_ref().map(|p| !p.is_empty())) else {
            continue;
        };
        let db = d.db();
        // Something else moved it meanwhile: that stands.
        if db.resolve_ref(&s.id)?.status != s.status {
            continue;
        }
        let now = now_ms();
        db.set_status(&s.id, want, now)?;
        // (The times OpenCode recorded, not when rlord noticed.)
        let kind = match want {
            Status::Working => {
                db.touch_last_user_msg(&s.id, turn.busy.unwrap_or(now))?;
                "prompt_submitted"
            }
            Status::WaitingInput => "permission_request",
            _ if s.status == Status::Starting && turn.busy.is_none() => "session_start",
            _ => {
                db.touch_last_agent_msg(&s.id, turn.idle.unwrap_or(now))?;
                "agent_stopped"
            }
        };
        db.insert_event(&s.id, now, kind, r#"{"from":"opencode session"}"#)?;
        changed = true;
    }
    Ok(changed)
}

/// Apply what the launch held back (the model, then the first prompt) to
/// each session OpenCode now has. Once: the record is removed whether or
/// not the server took it (a model it doesn't know is logged, not retried).
fn apply_deferred(d: &Daemon, svc: &Service, up: &[&Session]) -> Result<bool> {
    let waiting: Vec<(&Session, String)> = {
        let db = d.db();
        up.iter()
            .filter_map(|s| Some((*s, db.kv_get(&deferred_key(&s.id)).ok()??)))
            .collect()
    };
    let mut changed = false;
    for (s, raw) in waiting {
        let Some(id) = s.agent_session.as_deref() else {
            continue;
        };
        if svc.has_session(id) != Some(true) {
            continue; // not created yet (or the server can't say): later
        }
        let outcome = match serde_json::from_str::<Deferred>(&raw) {
            Ok(def) => {
                let model = match opencode::model_ref(&def.model) {
                    Some(m) => svc.set_model(id, &m).map_err(|e| format!("{e:#}")),
                    None => Err("not a provider/model name".to_string()),
                };
                let prompt = def
                    .prompt
                    .as_deref()
                    .map(|p| svc.prompt(id, p).map_err(|e| format!("{e:#}")));
                json!({"model": def.model,
                       "model_error": model.err(),
                       "prompt_sent": prompt.as_ref().map(|p| p.is_ok()),
                       "prompt_error": prompt.and_then(|p| p.err())})
            }
            Err(e) => json!({"error": e.to_string()}),
        };
        let db = d.db();
        db.kv_delete(&deferred_key(&s.id))?;
        db.insert_event(&s.id, now_ms(), "launch_applied", &outcome.to_string())?;
        if outcome["model_error"].is_string() || outcome["prompt_error"].is_string() {
            tracing::warn!(session = %s.id, outcome = %outcome, "applying an OpenCode launch");
        }
        changed = true;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Status::*;

    fn waits(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_turn_starts_and_ends() {
        let mut b = BTreeSet::new();
        // The TUI is up.
        let s = step(Starting, &mut b, &Signal::Created);
        assert_eq!((s.status, s.kind), (Some(Idle), Some("session_start")));
        assert_eq!(step(Idle, &mut b, &Signal::Created), Step::default());
        // A prompt is a message from the user, and starts the turn.
        let s = step(Idle, &mut b, &Signal::Prompt);
        assert_eq!(
            (s.status, s.user_msg, s.agent_msg),
            (Some(Working), true, false)
        );
        // The turn starting right after changes nothing more.
        assert_eq!(step(Working, &mut b, &Signal::Busy), Step::default());
        // A prompt sent mid-turn is still a message; the status stays.
        let s = step(Working, &mut b, &Signal::Prompt);
        assert_eq!(
            (s.status, s.kind, s.user_msg),
            (None, Some("prompt_submitted"), true)
        );
        // Its end is the agent's message.
        let s = step(Working, &mut b, &Signal::Ended);
        assert_eq!(
            (s.status, s.kind, s.agent_msg),
            (Some(Idle), Some("agent_stopped"), true)
        );
        // Said twice (execution.succeeded, then session.idle): once.
        assert_eq!(step(Idle, &mut b, &Signal::Ended), Step::default());
        // A turn nobody prompted here (started from another client).
        let s = step(Idle, &mut b, &Signal::Busy);
        assert_eq!(
            (s.status, s.kind, s.user_msg),
            (Some(Working), Some("turn_started"), false)
        );
    }

    #[test]
    fn needs_you_until_everything_asked_is_answered() {
        let mut b = BTreeSet::new();
        let asked = |x: &str| Signal::Asked(x.into());
        let answered = |x: &str| Signal::Answered(x.into());
        let s = step(Working, &mut b, &asked("perm:1"));
        assert_eq!(
            (s.status, s.kind),
            (Some(WaitingInput), Some("permission_request"))
        );
        assert!(
            !s.user_msg && !s.agent_msg,
            "a prompt for permission isn't a message"
        );
        // A question on top (a sub-agent's, say).
        let s = step(WaitingInput, &mut b, &asked("form:9"));
        assert_eq!((s.status, s.kind), (None, Some("question")));
        // The same one again: nothing.
        assert_eq!(
            step(WaitingInput, &mut b, &asked("form:9")),
            Step::default()
        );
        assert_eq!(b, waits(&["form:9", "perm:1"]));
        // One answered: still waiting on the other.
        assert_eq!(
            step(WaitingInput, &mut b, &answered("perm:1")),
            Step::default()
        );
        let s = step(WaitingInput, &mut b, &answered("form:9"));
        assert_eq!((s.status, s.kind), (Some(Working), Some("answered")));
        assert!(b.is_empty());
        // An answer to something never seen asked (asked before rlord
        // connected) changes nothing by itself.
        assert_eq!(step(Working, &mut b, &answered("perm:0")), Step::default());
        // The turn ending, or going on, clears what was waited on.
        step(Working, &mut b, &asked("perm:2"));
        let s = step(WaitingInput, &mut b, &Signal::Ended);
        assert_eq!((s.status, s.agent_msg), (Some(Idle), true));
        assert!(b.is_empty());
        step(Working, &mut b, &asked("perm:3"));
        assert_eq!(
            step(WaitingInput, &mut b, &Signal::Busy).status,
            Some(Working)
        );
        assert!(b.is_empty());
    }

    #[test]
    fn a_resync_follows_opencodes_own_record() {
        let turn = |busy: Option<i64>, idle: Option<i64>| Turn { busy, idle };
        let (running, over, none) = (
            turn(Some(20), Some(10)),
            turn(Some(20), Some(30)),
            turn(None, None),
        );
        // The session exists, nothing asked yet: your turn.
        assert_eq!(settle(Starting, none, Some(false)), Some(Idle));
        assert_eq!(settle(Starting, turn(Some(5), None), None), Some(Working));
        assert_eq!(settle(Idle, running, Some(false)), Some(Working));
        assert_eq!(settle(Working, over, Some(false)), Some(Idle));
        assert_eq!(settle(Working, running, Some(false)), None);
        assert_eq!(settle(Idle, over, None), None);
        // Something is waiting on the user, as the server says.
        assert_eq!(settle(Working, running, Some(true)), Some(WaitingInput));
        assert_eq!(settle(WaitingInput, running, Some(true)), None);
        // It was answered while rlord wasn't listening.
        assert_eq!(settle(WaitingInput, running, Some(false)), Some(Working));
        // Without the server: a waiting session mid-turn is left alone,
        // but a turn that ended can't be waiting.
        assert_eq!(settle(WaitingInput, running, None), None);
        assert_eq!(settle(WaitingInput, over, None), Some(Idle));
    }

    fn db_with(id: &str, agent: &str, conversation: &str) -> Db {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id,
            name: "o",
            agent,
            agent_session: Some(conversation),
            project_path: "/tmp",
            tmux_session: Some("rlord_absent"),
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        db.set_status(id, Starting, 1).unwrap();
        db
    }

    #[test]
    fn signals_write_status_events_and_message_times() {
        let db = db_with("01oc", "opencode", "ses_1a");
        let mut blocked = Blocked::new();
        let get = || db.resolve_ref("01oc").unwrap();
        let events = || db.list_events("01oc", 100).unwrap().len();
        let mut on = |signal: Signal| apply(&db, &mut blocked, &get(), &signal).unwrap();
        assert!(on(Signal::Created));
        assert_eq!(get().status, Idle);
        assert_eq!(
            (get().last_user_msg_at, get().last_agent_msg_at),
            (None, None)
        );
        assert!(on(Signal::Prompt));
        assert!(get().last_user_msg_at.is_some());
        let n = events();
        // Nothing new: nothing written.
        assert!(!on(Signal::Busy));
        assert_eq!(events(), n);
        assert!(on(Signal::Asked("perm:1".into())));
        assert_eq!(get().status, WaitingInput);
        assert_eq!(get().last_agent_msg_at, None);
        assert!(on(Signal::Answered("perm:1".into())));
        assert_eq!(get().status, Working);
        assert!(on(Signal::Ended));
        assert_eq!(get().status, Idle);
        assert!(get().last_agent_msg_at.is_some());
        assert!(!on(Signal::Ended));
        // A stopped session (its TUI closed) isn't revived by the server
        // finishing its turn.
        db.set_status("01oc", Stopped, 9).unwrap();
        assert!(!on(Signal::Busy));
        assert_eq!(get().status, Stopped);
        // Another agent's session that happens to hold such an id: never.
        let other = db_with("01cl", "claude", "ses_2b");
        let s = other.resolve_ref("01cl").unwrap();
        assert!(!apply(&other, &mut Blocked::new(), &s, &Signal::Busy).unwrap());
    }
}
