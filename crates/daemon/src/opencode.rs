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
//!   each session's turn state from OpenCode's own session row, checked
//!   against the turns the server says are running (a row left "busy" by
//!   a server that died mid-turn is over), and what it waits on from the
//!   server.
//! - **Without the server**: the session rows alone, every tick, and only
//!   for what they can still say. A session that was starting is up. A
//!   turn can't be running once the server is gone (turns run in it), so
//!   a session still marked working then becomes your turn; while the
//!   server may only be out of reach for a moment, nothing is concluded.
//!   "Needs you" can't be seen that way.
//! - **Stopped** is the pane: the TUI exited (two looks in a row, as
//!   `discover` does). The turn itself lives in the server and may go on
//!   after the TUI is closed; rlord shows the session, which is stopped.
//!
//! Nothing runs while there are no live OpenCode sessions: no connection,
//! no reads. Nothing is written unless something changed. "Updated" times
//! move only on a prompt the user sent and on a turn's end.
//!
//! This task also confirms what a launch asked for beyond the command
//! line (`agents::Deferred`: the model). The launch creates the session
//! with its model before opening the TUI; if that didn't take, the model
//! is set here once the session exists, and when it can't be in time
//! (`DEFERRED_WAIT_MS`) the session's log says it wasn't applied. The first
//! prompt always rides on the command line: nothing here can lose it.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::json;

use rlord_core::agents::opencode::{self, Turn};
use rlord_core::agents::opencode_service::{map_event, Events, Service, Signal, EVENT_TYPES};
use rlord_core::agents::Agent;
use rlord_core::db::Db;
use rlord_core::types::{now_ms, Session, Status};

use crate::Daemon;

const TICK: Duration = Duration::from_secs(3);

/// The longest wait between attempts to reach the server.
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// How long a launch's model may take to be confirmed or set.
const DEFERRED_WAIT_MS: i64 = 20_000;

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
    /// asks permission under its own id). Only definite answers.
    parents: HashMap<String, Option<String>>,
    /// Attempts to reach the server that found nothing there (no
    /// `service.json`, or nothing accepting connections at its address),
    /// in a row. An error answer means it is there: that resets it.
    failed: u8,
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
            failed: 0,
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
        f.failed = 0;
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
                f.failed = 0;
                changed |= settle_all(d, With::Server(&svc), &up, &mut f.blocked)?;
                f.link = Some((svc, events));
            }
            Err(Unreached { gone, error }) => {
                tracing::debug!(error = %format!("{error:#}"), gone, "OpenCode's server not followed");
                f.retry_at = Instant::now() + f.backoff;
                f.backoff = (f.backoff * 2).min(MAX_BACKOFF);
                f.failed = if gone { f.failed.saturating_add(1) } else { 0 };
            }
        }
    }
    match f.link.take() {
        Some((svc, mut events)) => {
            changed |= apply_deferred(d, Some(&svc), &up)?;
            // A session still starting: its creation may have been missed
            // (the event is sent once). Its row says when it is there.
            let starting: Vec<&Session> = up
                .iter()
                .copied()
                .filter(|s| s.status == Status::Starting)
                .collect();
            if !starting.is_empty() {
                let rows = With::Rows { server_gone: false };
                changed |= settle_all(d, rows, &starting, &mut f.blocked)?;
            }
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
            changed |= apply_deferred(d, None, &up)?;
            // Gone, not merely out of reach for a moment (a stream that
            // just broke, an error answer): nothing was there twice running.
            let rows = With::Rows {
                server_gone: f.failed >= 2,
            };
            changed |= settle_all(d, rows, &up, &mut f.blocked)?;
            if changed {
                d.changed();
            }
            std::thread::sleep(TICK);
        }
    }
    Ok(())
}

/// Why the server couldn't be followed. `gone`: nothing is there (no
/// `service.json`, or nothing accepts a connection at its address); else
/// it is there and answered with an error (a 401, a 404).
struct Unreached {
    gone: bool,
    error: anyhow::Error,
}

/// The server as `service.json` names it now, and its event stream.
fn connect() -> Result<(Service, Events), Unreached> {
    let Some(svc) = Service::read(&opencode::service_file()) else {
        return Err(Unreached {
            gone: true,
            error: anyhow::anyhow!("no OpenCode server is running"),
        });
    };
    match svc.events(EVENT_TYPES, TICK) {
        Ok(events) => Ok((svc, events)),
        Err(error) => Err(Unreached {
            gone: !svc.answers(),
            error,
        }),
    }
}

/// Sessions whose TUI exited: stopped (two looks in a row, as `discover`
/// does).
fn panes(d: &Daemon, live: &[Session], gone: &mut HashMap<String, u8>) -> Result<bool> {
    let mut changed = false;
    for s in live {
        if crate::discover::pane_looks(s, gone) >= 2 {
            changed |= crate::discover::stop_from_pane(&d.db(), s)?;
        }
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
    // A prompt: the user's, or one rlord typed (`own.rs`; OpenCode's
    // event has no text here, so by time).
    let (kind, source) = if step.user_msg {
        crate::own::prompt(db, &s.id, None, now)?
    } else {
        (kind, None)
    };
    if step.agent_msg {
        db.touch_last_agent_msg(&s.id, now)?;
    }
    let detail = crate::own::with_source(json!({ "from": from }), source);
    db.insert_event(&s.id, now, kind, &detail.to_string())?;
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
    let store = opencode::db_path();
    let mut found = None;
    let mut failed = None;
    climb(
        &mut f.parents,
        id,
        via_parents,
        |id| match d.db().session_by_agent_session(id) {
            Ok(s) => {
                found = s;
                found.is_some()
            }
            Err(e) => {
                failed = Some(e);
                true // (stop: nothing can be told)
            }
        },
        |id| opencode::parent(&store, id),
    );
    match failed {
        Some(e) => Err(e),
        None => Ok(found),
    }
}

/// Walk from OpenCode session `id` up its parents until `ours` says one is
/// rlord's (true: found, or stop). `parent` answers `Some(Some(p))`,
/// `Some(None)` (it has none) or `None` (couldn't be told). Only definite
/// answers are remembered in `cache`: a parent that couldn't be read is
/// asked for again next time, never taken as "no parent".
fn climb(
    cache: &mut HashMap<String, Option<String>>,
    id: &str,
    via_parents: bool,
    mut ours: impl FnMut(&str) -> bool,
    parent: impl Fn(&str) -> Option<Option<String>>,
) -> bool {
    let mut id = id.to_string();
    for _ in 0..6 {
        if ours(&id) {
            return true;
        }
        if !via_parents {
            return false;
        }
        if cache.len() > 2000 {
            cache.clear();
        }
        let up = match cache.get(&id) {
            Some(known) => known.clone(),
            None => match parent(&id) {
                Some(answer) => {
                    cache.insert(id.clone(), answer.clone());
                    answer
                }
                None => return false,
            },
        };
        match up {
            Some(p) => id = p,
            None => return false,
        }
    }
    false
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
        changed |= apply_deferred(d, Some(svc), &[&s])?;
    }
    Ok(changed)
}

/// What a resync has to go on.
#[derive(Clone, Copy)]
enum With<'a> {
    /// The server, just (re)connected: what each session waits on is asked.
    Server(&'a Service),
    /// OpenCode's session rows alone. `server_gone`: the server didn't
    /// answer repeatedly (not merely a stream that just broke).
    Rows { server_gone: bool },
}

/// What is known about a session at a resync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// The server answered: whether the session waits on the user.
    Pending(bool),
    /// Rows only, the server possibly still there.
    Rows,
    /// Rows only, and the server is gone.
    ServerGone,
    /// The server answered: nothing waits on the user, and no turn of the
    /// session is running in it (`Service::active`), whatever the row says
    /// (a turn that died with an earlier server).
    NotRunning,
}

/// The status a session should be in, from its turn state and what is
/// known; None when that is where it is, or when nothing can be concluded.
///
/// With the server's answer, the rows and the answer decide. Without it,
/// only what the rows can still say: a turn that ended is your turn; a
/// turn the rows call running is believed only for a session that was
/// starting, and is over when the server is gone (turns run in it, so a
/// server that died mid-turn leaves the row "busy" for good).
fn settle(cur: Status, turn: Turn, seen: Seen) -> Option<Status> {
    let want = match (seen, turn.working()) {
        (Seen::Pending(true), _) => Status::WaitingInput,
        (Seen::Pending(false), true) => Status::Working,
        (_, false) => Status::Idle,
        (Seen::ServerGone | Seen::NotRunning, true) => Status::Idle,
        (Seen::Rows, true) if cur == Status::Starting => Status::Working,
        (Seen::Rows, true) => return None,
    };
    (want != cur).then_some(want)
}

/// How long a session may be "starting" with no row in OpenCode before it
/// is taken as up anyway (its screen is: the pane is alive). Longer than
/// the launch's create step may take, with its message on screen.
const STARTING_MS: i64 =
    ((opencode::CREATE_LIMIT_S + opencode::CREATE_PAUSE_S) * 1000) as i64 + 5_000;

/// What `id` and the sub-agent sessions under it wait on the user for
/// (None: the server couldn't say for `id` itself). Only sub-agent
/// sessions with a turn running are asked (one that is done waits on
/// nobody), and one that can't be asked counts as waiting on nothing.
fn pending_under(svc: &Service, store: &std::path::Path, id: &str) -> Option<Vec<String>> {
    let mut all = svc.pending(id)?;
    for (child, turn) in opencode::descendants(store, id) {
        if turn.working() {
            all.extend(svc.pending(&child).unwrap_or_default());
        }
    }
    Some(all)
}

/// Bring every session in line with OpenCode's own record: after a
/// (re)connect (what each session, and the sub-agent sessions under it,
/// waits on is asked too), and every tick while there is no server. The
/// reads happen without the DB lock.
fn settle_all(d: &Daemon, with: With, up: &[&Session], blocked: &mut Blocked) -> Result<bool> {
    let store = opencode::db_path();
    // The turns running in the server (asked before the rows are read: a
    // turn starting in between is on the event stream, already open).
    let active = match with {
        With::Server(svc) => svc.active(),
        With::Rows { .. } => None,
    };
    // When the current server started (it writes service.json then). The
    // veto below applies only to turns that began before it: a turn that
    // died with an earlier server. A turn begun under this server is
    // trusted to its row, since `/active` may not list every project's
    // sessions or a session waiting on a permission.
    let server_since = std::fs::metadata(opencode::service_file())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    let mut changed = false;
    for s in up {
        let Some(id) = s.agent_session.as_deref() else {
            continue;
        };
        let turn = opencode::turn(&store, id);
        let pending = match with {
            With::Server(svc) if turn.is_some() => pending_under(svc, &store, id),
            _ => None,
        };
        if let Some(p) = &pending {
            if p.is_empty() {
                blocked.remove(&s.id);
            } else {
                blocked.insert(s.id.clone(), p.iter().cloned().collect());
            }
        }
        let began_before_server = matches!(
            (turn.and_then(|t| t.busy), server_since),
            (Some(b), Some(since)) if b < since
        );
        let not_running = began_before_server && active.as_ref().is_some_and(|a| !a.contains(id));
        let seen = match (&pending, with) {
            (Some(p), _) if p.is_empty() && not_running => Seen::NotRunning,
            (Some(p), _) => Seen::Pending(!p.is_empty()),
            (None, With::Rows { server_gone: true }) => Seen::ServerGone,
            (None, _) => Seen::Rows,
        };
        let now = now_ms();
        let want = match turn {
            Some(turn) => settle(s.status, turn, seen),
            // No row: OpenCode hasn't created the session (the launch's
            // create step didn't take; the TUI creates it with the first
            // message). After a while it is simply up.
            None => {
                let since = s.last_state_change_at.unwrap_or(s.created_at);
                (s.status == Status::Starting && now - since > STARTING_MS).then_some(Status::Idle)
            }
        };
        let Some(want) = want else {
            continue;
        };
        let db = d.db();
        // Something else moved it meanwhile: that stands.
        if db.resolve_ref(&s.id)?.status != s.status {
            continue;
        }
        db.set_status(&s.id, want, now)?;
        let ended = turn.is_some_and(|t| !t.working());
        // (The times OpenCode recorded, not when rlord noticed.)
        let mut source = None;
        let (kind, from) = match want {
            // What it waited on was answered while rlord wasn't listening:
            // not a message (as from the event stream).
            Status::Working if s.status == Status::WaitingInput => ("answered", "opencode session"),
            Status::Working => {
                let busy = turn.and_then(|t| t.busy).unwrap_or(now);
                // (The user's, or one rlord typed: `own.rs`, by time.)
                let (kind, by) = crate::own::prompt(&db, &s.id, None, busy)?;
                source = by;
                (kind, "opencode session")
            }
            Status::WaitingInput => ("permission_request", "opencode session"),
            // Just (re)started, and no turn running: it is up.
            _ if s.status == Status::Starting => ("session_start", "opencode session"),
            _ if ended => {
                let idle = turn.and_then(|t| t.idle).unwrap_or(now);
                db.touch_last_agent_msg(&s.id, idle)?;
                ("agent_stopped", "opencode session")
            }
            // Its turn never ended: the server it ran in is gone, or the
            // server running now has no such turn. Not a reply, so
            // "updated" doesn't move.
            _ if seen == Seen::NotRunning => ("agent_stopped", "opencode turn not running"),
            _ => ("agent_stopped", "opencode server gone"),
        };
        let detail = crate::own::with_source(json!({ "from": from }), source);
        db.insert_event(&s.id, now, kind, &detail.to_string())?;
        changed = true;
    }
    Ok(changed)
}

/// Confirm each launch's model (see `Deferred`), once:
///
/// - the session has it (the launch's create step took): done, silently;
/// - OpenCode has the session on another model: set it now, and say in
///   the session's log that a first prompt may have run before that;
/// - neither within `DEFERRED_WAIT_MS` of the launch (no server, or no
///   such session): the log says the model wasn't applied.
///
/// `linked`: the server the event stream is on; without one, the server
/// `service.json` names is tried (it may be up before the stream is).
fn apply_deferred(d: &Daemon, linked: Option<&Service>, up: &[&Session]) -> Result<bool> {
    let waiting: Vec<(&Session, String)> = {
        let db = d.db();
        up.iter()
            .filter_map(|s| Some((*s, db.kv_get(&deferred_key(&s.id)).ok()??)))
            .collect()
    };
    if waiting.is_empty() {
        return Ok(false);
    }
    let store = opencode::db_path();
    let read = match linked {
        Some(_) => None,
        None => Service::read(&opencode::service_file()),
    };
    let svc = linked.or(read.as_ref());
    let mut changed = false;
    for (s, raw) in waiting {
        let Some(id) = s.agent_session.as_deref() else {
            continue;
        };
        let held: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        let late = now_ms() - held["at"].as_i64().unwrap_or(0) > DEFERRED_WAIT_MS;
        let model = held["model"].as_str().unwrap_or("").to_string();
        let has = opencode::session_model(&store, id);
        let outcome = match deferred_step(&model, has.as_deref(), late) {
            Deferred::Wait => continue,
            Deferred::Done => None,
            Deferred::Note(why) => Some(not_applied(&model, &why)),
            Deferred::Set(wanted) => match svc.map(|svc| (svc.has_session(id), svc)) {
                Some((Some(true), svc)) => Some(match svc.set_model(id, &wanted) {
                    Ok(()) => json!({"model": model, "note": "the model was set after the \
                        session started: a first prompt may have run on OpenCode's default"}),
                    Err(e) => not_applied(&model, &format!("{e:#}")),
                }),
                _ if late => Some(not_applied(
                    &model,
                    match svc {
                        Some(_) => "OpenCode hasn't created the session in time",
                        None => "OpenCode's server couldn't be reached in time",
                    },
                )),
                _ => continue,
            },
        };
        let db = d.db();
        db.kv_delete(&deferred_key(&s.id))?;
        if let Some(outcome) = outcome {
            db.insert_event(&s.id, now_ms(), "launch_applied", &outcome.to_string())?;
            tracing::warn!(session = %s.id, outcome = %outcome, "an OpenCode launch's model");
            changed = true;
        }
    }
    Ok(changed)
}

/// What to do about a launch's model.
#[derive(Debug, PartialEq)]
enum Deferred {
    /// The session has it.
    Done,
    /// Set it through the server (if the session is there).
    Set(serde_json::Value),
    /// Nothing can be done: say why in the session's log.
    Note(String),
    /// The session isn't there yet: look again.
    Wait,
}

/// `model`: what the launch asked for; `has`: the model on the session's
/// row (None: no such session yet, or no model on it).
fn deferred_step(model: &str, has: Option<&str>, late: bool) -> Deferred {
    let Some(wanted) = opencode::model_ref(model) else {
        return Deferred::Note("not a provider/model name".to_string());
    };
    // (Compared without the variant: the row doesn't keep it.)
    let name = opencode::model_name(&wanted);
    match has {
        Some(has) if Some(has) == name.as_deref() => Deferred::Done,
        None if !late => Deferred::Wait,
        _ => Deferred::Set(wanted),
    }
}

/// The log entry for a model that wasn't applied.
fn not_applied(model: &str, why: &str) -> serde_json::Value {
    json!({"model": model, "model_error": why,
           "note": format!("the model {model} wasn't applied ({why}): this session runs on \
                            OpenCode's default model")})
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
        let (yes, no) = (Seen::Pending(true), Seen::Pending(false));
        // The session exists, nothing asked yet: your turn.
        assert_eq!(settle(Starting, none, no), Some(Idle));
        assert_eq!(settle(Idle, running, no), Some(Working));
        assert_eq!(settle(Working, over, no), Some(Idle));
        assert_eq!(settle(Working, running, no), None);
        // Something is waiting on the user, as the server says.
        assert_eq!(settle(Working, running, yes), Some(WaitingInput));
        assert_eq!(settle(WaitingInput, running, yes), None);
        // It was answered while rlord wasn't listening.
        assert_eq!(settle(WaitingInput, running, no), Some(Working));
    }

    #[test]
    fn rows_alone_say_less_and_a_dead_server_ends_its_turns() {
        let (running, over) = (
            Turn {
                busy: Some(20),
                idle: Some(10),
            },
            Turn {
                busy: Some(20),
                idle: Some(30),
            },
        );
        // A turn that ended is your turn, whoever says so.
        assert_eq!(settle(Working, over, Seen::Rows), Some(Idle));
        assert_eq!(settle(WaitingInput, over, Seen::Rows), Some(Idle));
        assert_eq!(settle(Idle, over, Seen::Rows), None);
        // A session that was starting is believed mid-turn (launched with
        // a prompt); otherwise a "busy" row concludes nothing while the
        // server may only be out of reach.
        assert_eq!(settle(Starting, running, Seen::Rows), Some(Working));
        assert_eq!(settle(Idle, running, Seen::Rows), None);
        assert_eq!(settle(Working, running, Seen::Rows), None);
        assert_eq!(settle(WaitingInput, running, Seen::Rows), None);
        // The server is gone: no turn is running, whatever its row says.
        assert_eq!(settle(Working, running, Seen::ServerGone), Some(Idle));
        assert_eq!(settle(WaitingInput, running, Seen::ServerGone), Some(Idle));
        assert_eq!(settle(Starting, running, Seen::ServerGone), Some(Idle));
        assert_eq!(settle(Idle, running, Seen::ServerGone), None);
        // The server answered and runs no turn of it: the row is stale
        // (its turn died with an earlier server).
        assert_eq!(settle(Working, running, Seen::NotRunning), Some(Idle));
        assert_eq!(settle(Starting, running, Seen::NotRunning), Some(Idle));
        assert_eq!(settle(Idle, running, Seen::NotRunning), None);
        assert_eq!(settle(Working, over, Seen::NotRunning), Some(Idle));
    }

    #[test]
    fn only_a_definite_parent_is_remembered() {
        use std::cell::{Cell, RefCell};
        let ours = |id: &str| id == "ses_root";
        let mut cache = HashMap::new();
        // The database can't be read just now: not found, nothing cached.
        let asked = Cell::new(0);
        let unreadable = |_: &str| {
            asked.set(asked.get() + 1);
            None
        };
        assert!(!climb(&mut cache, "ses_child", true, ours, unreadable));
        assert!(cache.is_empty());
        // Readable again: found through its parent, and remembered.
        let tree = |id: &str| {
            asked.set(asked.get() + 1);
            match id {
                "ses_grandchild" => Some(Some("ses_child".to_string())),
                "ses_child" => Some(Some("ses_root".to_string())),
                "ses_other" => Some(None),
                _ => None,
            }
        };
        assert!(climb(&mut cache, "ses_grandchild", true, ours, tree));
        assert_eq!(cache.len(), 2);
        let before = asked.get();
        assert!(climb(&mut cache, "ses_grandchild", true, ours, tree));
        assert_eq!(asked.get(), before, "answered from memory");
        // Someone else's session: a definite "no parent" is remembered too.
        assert!(!climb(&mut cache, "ses_other", true, ours, tree));
        assert_eq!(cache.get("ses_other"), Some(&None));
        // Events that aren't prompts never climb.
        let seen = RefCell::new(Vec::new());
        let note = |id: &str| {
            seen.borrow_mut().push(id.to_string());
            false
        };
        assert!(!climb(&mut cache, "ses_child", false, note, tree));
        assert_eq!(*seen.borrow(), ["ses_child"]);
    }

    #[test]
    fn a_launchs_model_is_confirmed_set_or_reported() {
        let want = "opencode/x-free";
        // The launch created the session with it: nothing to do or say.
        assert_eq!(
            deferred_step(want, Some("opencode/x-free"), false),
            Deferred::Done
        );
        assert_eq!(
            deferred_step("opencode/x-free#high", Some(want), true),
            Deferred::Done
        );
        // The session is there on another model: set it.
        let set = Deferred::Set(json!({"providerID": "opencode", "id": "x-free"}));
        assert_eq!(deferred_step(want, Some("opencode/default"), false), set);
        // Not there yet: wait; past the wait, one last try (the server may
        // have it though its row can't be read), else it is reported.
        assert_eq!(deferred_step(want, None, false), Deferred::Wait);
        assert_eq!(deferred_step(want, None, true), set);
        assert!(matches!(
            deferred_step("opus", None, false),
            Deferred::Note(_)
        ));
        let v = not_applied(want, "OpenCode's server couldn't be reached in time");
        let note = v["note"].as_str().unwrap();
        assert!(
            note.contains(want) && note.contains("default model"),
            "{note}"
        );
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
