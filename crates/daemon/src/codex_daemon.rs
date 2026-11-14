//! Codex sessions on Codex's shared background server ("daemon mode",
//! `[session] codex_daemon`; a session's launch spec `codex_host`).
//!
//! The thread is hosted by Codex's per-user daemon and the pane runs a TUI
//! that is a client of it: the turn goes on when the pane ends, and a
//! restart attaches again (`codex resume <id>`). What changes for rlord:
//!
//! - **Hooks run in the daemon**, without the session's `RLORD_SESSION_ID`
//!   or `TMUX_PANE`. They are matched by thread id (`route`): the bound
//!   thread of a live daemon-mode session, or a first `SessionStart` from
//!   the folder of exactly one daemon-mode session still waiting for its
//!   thread. The hook client asks the gate file (`codex_gate`, kept by
//!   `sync_gate`) before it sends anything, so other Codex threads on the
//!   machine cost one small read.
//! - **A pane that ends isn't a stopped agent** while its turn runs on the
//!   daemon (`turn_running`, asked of the daemon, else the transcript).
//! - **Stopping interrupts the turn first** (`interrupt`): Escape into the
//!   pane when it is up and shows no dialog, else `turn/interrupt` through
//!   the daemon's control socket. Then the pane goes.
//! - Before such a pane starts, rlord starts the daemon itself when none
//!   runs (`ensure_running`), with its own session variables left out: a
//!   daemon the TUI started would inherit the pane's environment and hand
//!   it to every hook it runs.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use rlord_core::agents::codex_gate::{self, Gate};
use rlord_core::agents::codex_server::{self, Interrupted};
use rlord_core::agents::{install, transcript, Agent};
use rlord_core::db::Db;
use rlord_core::tmux;
use rlord_core::types::{Session, Status};

/// The gate as last written (None: not yet this run).
static LAST_GATE: Mutex<Option<Gate>> = Mutex::new(None);

/// The gate for these sessions: each live daemon-mode Codex session's bound
/// thread, and the folder of each one still waiting for a thread. A
/// stopped one isn't listed: its hooks must not bring it back.
pub(crate) fn gate_of(sessions: &[Session]) -> Gate {
    let mut g = Gate::default();
    for s in sessions
        .iter()
        .filter(|s| s.codex_daemon && s.agent == Agent::Codex.name() && s.status != Status::Stopped)
    {
        match &s.agent_session {
            Some(t) => {
                g.threads.insert(t.clone());
            }
            None => {
                g.folders.insert(s.project_path.clone());
                if let Ok(real) = std::fs::canonicalize(&s.project_path) {
                    g.folders.insert(real.to_string_lossy().into_owned());
                }
            }
        }
    }
    g
}

/// Rewrite the gate file when what it should say changed. Cheap: one query,
/// and a write only on a change. Errors are logged, never returned (a
/// stale gate costs a missed hook, which `discover` makes up for).
pub fn sync_gate(db: &Db) {
    // (Unit tests never write the user's data folder.)
    if cfg!(test) {
        return;
    }
    let Ok(sessions) = db.list_sessions(false) else {
        return;
    };
    let g = gate_of(&sessions);
    let Ok(mut last) = LAST_GATE.lock() else {
        return;
    };
    if last.as_ref() == Some(&g) {
        return;
    }
    match codex_gate::write_to(&codex_gate::path(), &g) {
        Ok(()) => *last = Some(g),
        Err(e) => tracing::warn!(error = %e, "writing the Codex daemon gate"),
    }
}

/// Whose a hook event is.
#[derive(Debug)]
pub(crate) enum Route {
    /// The session named by `RLORD_SESSION_ID`, from its pane (checked
    /// against the pane as for any session).
    Pane(Session),
    /// A daemon-mode session, matched by thread (no pane to check).
    Thread(Session),
    Ignore(&'static str),
}

/// Whose a Codex hook event is. `named`: the session `RLORD_SESSION_ID`
/// named, if any (a daemon started from a pane may hand that pane's on to
/// every hook it runs, so for daemon-mode sessions it isn't trusted).
pub(crate) fn route(
    db: &Db,
    event: &str,
    payload: &Value,
    named: Option<Session>,
) -> Result<Route> {
    let codex = Agent::Codex.name();
    let sid = payload["session_id"].as_str().filter(|s| !s.is_empty());
    // The thread of a live daemon-mode session: that session, whoever
    // says they sent it.
    if let Some(sid) = sid {
        if let Some(s) = db
            .session_by_agent_session(sid)?
            .filter(|s| s.codex_daemon && s.agent == codex && s.archived_at.is_none())
        {
            if s.status == Status::Stopped {
                return Ok(Route::Ignore("a stopped session's thread"));
            }
            return Ok(Route::Thread(s));
        }
    }
    match named {
        Some(s) if !s.codex_daemon => return Ok(Route::Pane(s)),
        _ => {}
    }
    // A first SessionStart in the folder of the one daemon-mode session
    // waiting for its thread there.
    if event != "SessionStart" {
        return Ok(Route::Ignore("not a thread rlord follows"));
    }
    let Some(cwd) = payload["cwd"].as_str() else {
        return Ok(Route::Ignore("not a thread rlord follows"));
    };
    let real = std::fs::canonicalize(cwd)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| cwd.to_string());
    let waiting: Vec<Session> = db
        .list_sessions(false)?
        .into_iter()
        .filter(|s| {
            s.codex_daemon
                && s.agent == codex
                && s.status != Status::Stopped
                && s.agent_session.is_none()
                && (s.project_path == cwd || s.project_path == real)
        })
        .collect();
    Ok(match <[Session; 1]>::try_from(waiting) {
        Ok([s]) => Route::Thread(s),
        Err(v) if v.is_empty() => Route::Ignore("not a thread rlord follows"),
        Err(_) => Route::Ignore("more than one session waits in that folder"),
    })
}

/// When `ensure_running` last failed to start the daemon.
static FAILED_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// How long after a failed start `ensure_running` doesn't try again.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Whether a start that last failed at `failed` may be tried now.
fn may_try(failed: Option<Instant>, now: Instant) -> bool {
    failed.is_none_or(|t| now.saturating_duration_since(t) >= RETRY_AFTER)
}

/// Make sure Codex's daemon runs before a daemon-mode pane starts (see the
/// module doc). Bounded. After a failed start it isn't tried again for a
/// minute (a Codex that can't start one isn't asked at every launch); a
/// success, or a Codex not resolved yet, leaves the next launch free to
/// ask. Never fails the launch: the TUI then starts one itself, or runs
/// the turn in the pane, and its hooks still reach rlord by thread.
pub fn ensure_running() {
    if !FAILED_AT.lock().is_ok_and(|t| may_try(*t, Instant::now())) {
        return;
    }
    let Some(program) = install::resolved(Agent::Codex.name()) else {
        return;
    };
    match codex_server::ensure(&program) {
        Ok(d) => {
            tracing::debug!(version = ?d.app_server_version, "Codex's background server is up")
        }
        Err(e) => {
            tracing::warn!(error = %e, "Codex's background server isn't running");
            if let Ok(mut t) = FAILED_AT.lock() {
                *t = Some(Instant::now());
            }
        }
    }
}

/// How long a send waits on the daemon (each step) to learn whether a
/// daemon-mode thread's turn runs. Short: the session's lock is held.
const SEND_CHECK_LIMIT: Duration = Duration::from_millis(500);

/// Why a message must not be typed into a daemon-mode session now: a turn
/// runs on its thread (maybe started from another Codex client, which
/// rlord's status doesn't see), waiting on input included. None: go ahead
/// (also when the daemon can't be asked: rlord's own checks decide).
pub(crate) fn busy_for_send(s: &Session) -> Option<String> {
    if !s.codex_daemon {
        return None;
    }
    let thread = s.agent_session.as_deref()?;
    busy_why(codex_server::thread_state_within(
        &codex_server::default_socket(),
        thread,
        SEND_CHECK_LIMIT,
    ))
}

fn busy_why(state: Option<codex_server::ThreadState>) -> Option<String> {
    state
        .filter(|st| st.running())
        .map(|_| "a turn is running on its thread (maybe from another Codex client)".to_string())
}

/// Whether a daemon-mode session's turn is running, without its pane:
/// asked of the daemon (`thread/read`), else read from its transcript.
/// None: it can't be told.
pub(crate) fn turn_running(s: &Session, transcript_path: Option<&str>) -> Option<bool> {
    let thread = s.agent_session.as_deref()?;
    if let Some(st) = codex_server::thread_state(&codex_server::default_socket(), thread) {
        return Some(st.running());
    }
    transcript::working(std::path::Path::new(transcript_path?))
}

/// The session's pane is there and its program hasn't exited.
pub(crate) fn pane_up(s: &Session) -> bool {
    s.tmux_session
        .as_deref()
        .is_some_and(|t| tmux::has_session(t) && tmux::format(t, "#{pane_dead}") != "1")
}

/// What stopping did to a running turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stopped {
    /// No turn was running (or nothing could tell).
    Idle,
    /// It was interrupted and ended.
    Interrupted,
    /// It was asked to stop, and was still running when rlord stopped
    /// waiting: it may go on on Codex's server.
    StillRunning,
}

impl Stopped {
    pub(crate) fn to_json(self) -> Value {
        match self {
            Stopped::Idle => json!(null),
            Stopped::Interrupted => json!("interrupted"),
            Stopped::StillRunning => json!("still_running"),
        }
    }
}

/// Interrupt a daemon-mode session's running turn before its pane goes
/// (stop, archive). Escape into the pane when it is up with no dialog on
/// screen (Codex's composer: Escape there interrupts the turn, from any
/// client attached to the thread); otherwise `turn/interrupt` through the
/// daemon. Then wait a few seconds for the turn to end. Called with no DB
/// lock held.
pub(crate) fn interrupt(s: &Session) -> Stopped {
    let Some(thread) = s.agent_session.as_deref() else {
        return Stopped::Idle;
    };
    let socket = codex_server::default_socket();
    // The daemon says whether a turn runs; when it can't be asked, the
    // status rlord has does.
    let running = match codex_server::thread_state(&socket, thread) {
        Some(st) => st.running(),
        None => matches!(s.status, Status::Working | Status::WaitingInput),
    };
    if !running {
        return Stopped::Idle;
    }
    let pane = s
        .tmux_session
        .as_deref()
        .filter(|_| pane_up(s))
        .and_then(|t| Some((tmux::pane_id(t).ok()?, tmux::capture_pane(t).ok()?)));
    let escaped = match pane {
        Some((pane, screen)) if !rlord_core::agents::codex::dialog_shown(&screen) => {
            tmux::send_keys(&pane, &["Escape"]).is_ok()
        }
        _ => false,
    };
    if !escaped {
        match codex_server::interrupt(&socket, thread) {
            Ok(Interrupted::NotRunning) => return Stopped::Idle,
            Ok(Interrupted::Asked) => {}
            Err(e) => {
                tracing::warn!(session = %s.id, error = %e, "couldn't interrupt a Codex turn");
                return Stopped::StillRunning;
            }
        }
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(4) {
        std::thread::sleep(Duration::from_millis(250));
        match codex_server::thread_state(&socket, thread) {
            Some(st) if !st.running() => return Stopped::Interrupted,
            // (The daemon can't be asked: Escape was pressed; take it.)
            None if escaped => return Stopped::Interrupted,
            _ => {}
        }
    }
    Stopped::StillRunning
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlord_core::db::NewSession;

    #[test]
    fn retries_only_a_minute_after_a_failure() {
        let now = Instant::now() + Duration::from_secs(600);
        assert!(may_try(None, now));
        assert!(!may_try(Some(now - Duration::from_secs(10)), now));
        assert!(may_try(Some(now - RETRY_AFTER), now));
    }

    #[test]
    fn busy_while_any_turn_runs() {
        use codex_server::ThreadState as T;
        assert!(busy_why(Some(T::Active { waiting: false })).is_some());
        assert!(busy_why(Some(T::Active { waiting: true })).is_some());
        assert!(busy_why(Some(T::Idle)).is_none());
        assert!(busy_why(Some(T::NotLoaded)).is_none());
        assert!(busy_why(None).is_none());
    }

    fn session(db: &Db, id: &str, folder: &str, daemon: bool, thread: Option<&str>) {
        let spec = if daemon {
            json!({"agent": "codex", "codex_host": "daemon"})
        } else {
            json!({"agent": "codex"})
        };
        db.insert_session(&NewSession {
            id,
            name: id,
            agent: "codex",
            agent_session: thread,
            project_path: folder,
            tmux_session: Some(id),
            launch_spec: &spec.to_string(),
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        db.set_status(id, Status::Working, 1).unwrap();
    }

    fn routed(r: Route) -> String {
        match r {
            Route::Pane(s) => format!("pane:{}", s.id),
            Route::Thread(s) => format!("thread:{}", s.id),
            Route::Ignore(why) => format!("ignore:{why}"),
        }
    }

    #[test]
    fn hooks_from_the_daemon_go_by_thread() {
        let db = Db::open_in_memory().unwrap();
        session(&db, "d1", "/src/app", true, Some("t-1"));
        session(&db, "d2", "/src/web", true, None);
        session(&db, "p1", "/src/app", false, Some("t-9"));
        let r = |event: &str, payload: Value, named: Option<&str>| {
            let named = named.map(|n| db.resolve_ref(n).unwrap());
            routed(route(&db, event, &payload, named).unwrap())
        };
        // Its bound thread: that session, with or without a name.
        assert_eq!(r("Stop", json!({"session_id": "t-1"}), None), "thread:d1");
        // A daemon started from another session's pane names that one:
        // the thread decides.
        assert_eq!(
            r("Stop", json!({"session_id": "t-1"}), Some("p1")),
            "thread:d1"
        );
        // An in-pane session's own hooks: as before.
        assert_eq!(
            r("Stop", json!({"session_id": "t-9"}), Some("p1")),
            "pane:p1"
        );
        // Someone else's thread.
        assert!(r("Stop", json!({"session_id": "t-x"}), None).starts_with("ignore"));
        // A first SessionStart in the folder of the one waiting session.
        assert_eq!(
            r(
                "SessionStart",
                json!({"session_id": "t-2", "cwd": "/src/web"}),
                None
            ),
            "thread:d2"
        );
        // Not from a waiting folder, or not a SessionStart.
        assert!(r(
            "SessionStart",
            json!({"session_id": "t-3", "cwd": "/src/app"}),
            None
        )
        .starts_with("ignore"));
        assert!(r(
            "UserPromptSubmit",
            json!({"session_id": "t-2", "cwd": "/src/web"}),
            None
        )
        .starts_with("ignore"));
        // Two waiting in one folder: neither.
        session(&db, "d3", "/src/web", true, None);
        assert_eq!(
            r(
                "SessionStart",
                json!({"session_id": "t-2", "cwd": "/src/web"}),
                None
            ),
            "ignore:more than one session waits in that folder"
        );
        // A stopped session's thread never brings it back.
        db.set_status("d1", Status::Stopped, 2).unwrap();
        assert_eq!(
            r("Stop", json!({"session_id": "t-1"}), None),
            "ignore:a stopped session's thread"
        );
    }

    #[test]
    fn the_gate_lists_live_daemon_threads_and_waiting_folders() {
        let db = Db::open_in_memory().unwrap();
        session(&db, "d1", "/src/app", true, Some("t-1"));
        session(&db, "d2", "/src/web", true, None);
        session(&db, "d3", "/src/old", true, Some("t-3"));
        db.set_status("d3", Status::Stopped, 2).unwrap();
        session(&db, "p1", "/src/app", false, Some("t-9"));
        let g = gate_of(&db.list_sessions(false).unwrap());
        assert_eq!(g.threads.into_iter().collect::<Vec<_>>(), ["t-1"]);
        assert!(g.folders.contains("/src/web"));
        assert!(!g.folders.contains("/src/app"));
    }
}
