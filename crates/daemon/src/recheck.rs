//! Dead agent panes re-checked: a stored status re-verified against the
//! pane. Plain code, no AI. (The rules: "Stale statuses re-checked" in
//! `docs/architecture.md`.)
//!
//! A stored status moves only when an event moves it. An agent that
//! crashed without its `SessionEnd` leaves a live status behind on a dead
//! pane. Once at daemon start and then once a minute, every session that
//! follows hooks (Claude; Codex once its hooks are trusted) is looked at:
//! its tmux session there under its stored name, and its agent pane's
//! program exited, on two answered looks of the same pane in a row:
//! stopped. Nothing else is concluded: a tmux session missing (killed, or
//! renamed outside rlord) is the startup reconcile's, and when tmux can't
//! answer (no server running included) there is no verdict that pass.
//!
//! Bounded: one `tmux list-panes -a` a pass, and one more for each stop.
//! No tmux call is made under the DB lock; a session is stopped only if
//! it is still as it was read, under its session lock.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use rlord_core::agents::Agent;
use rlord_core::db::Db;
use rlord_core::tmux::{self, AgentPane};
use rlord_core::types::Session;

use crate::Daemon;

/// Time between passes.
const PASS: Duration = Duration::from_secs(60);

/// Every tmux session's agent pane, by name (`tmux::agent_panes`; tests
/// pass their own).
pub(crate) type Panes<'a> = dyn Fn() -> Result<HashMap<String, AgentPane>> + 'a;

/// Start the checker: a pass now (after `Daemon::startup`'s reconcile),
/// then one every `PASS`.
pub fn start(d: &Arc<Daemon>) {
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        let mut gone = Gone::new();
        loop {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pass(&d, &mut gone, &tmux::agent_panes)
            }));
            match r {
                Ok(Err(e)) => tracing::debug!(error = %e, "re-checking panes"),
                Err(_) => tracing::warn!("re-checking panes panicked"),
                Ok(Ok(_)) => {}
            }
            std::thread::sleep(PASS);
        }
    });
}

/// The sessions checked here: live, on an agent whose hooks report its
/// status, and not followed by another watcher (`discover` and
/// `opencode.rs` watch their own panes; Codex's background server runs
/// the turn without the pane).
fn checked(s: &Session) -> bool {
    !s.codex_daemon
        && match Agent::parse(&s.agent) {
            Some(Agent::Claude) => true,
            Some(Agent::Codex) => !crate::discover::is_unhooked(s),
            Some(Agent::OpenCode) | None => false,
        }
}

/// Dead looks in a row at one session's agent pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Looks {
    count: u8,
    /// The pane seen dead.
    pane: String,
    /// The session's `last_state_change_at` at the first dead look.
    since: Option<i64>,
}

/// Per session id.
pub(crate) type Gone = HashMap<String, Looks>;

/// Session `s`'s dead agent pane in a snapshot: its pane id when its tmux
/// session is there under the stored name and the pane's program exited.
/// None otherwise: alive, or missing (not this checker's to judge).
fn dead_pane(s: &Session, panes: &HashMap<String, AgentPane>) -> Option<String> {
    let p = panes.get(s.tmux_session.as_deref()?)?;
    p.dead.then(|| p.pane.clone())
}

/// Count one look at session `s` (`dead`: the dead pane seen, if any).
/// Two dead looks in a row count only for the same pane id with the
/// status time unchanged. (`respawn-pane -k` keeps the pane id: a restart
/// is caught by the pane being live again, by `last_state_change_at` when
/// the status moves, and by the session lock `stop` holds.) Returns the
/// dead looks in a row.
fn count_look(s: &Session, gone: &mut Gone, dead: Option<String>) -> u8 {
    let Some(pane) = dead else {
        gone.remove(&s.id);
        return 0;
    };
    let since = s.last_state_change_at;
    match gone.get_mut(&s.id) {
        Some(l) if l.pane == pane && l.since == since => {
            l.count = l.count.saturating_add(1);
            l.count
        }
        _ => {
            gone.insert(
                s.id.clone(),
                Looks {
                    count: 1,
                    pane,
                    since,
                },
            );
            1
        }
    }
}

/// One pass. When tmux can't say, no verdict this pass, and the counts
/// start again (two looks in a row means two answered ones). One
/// session's failure is logged and the pass goes on. Returns how many
/// sessions were stopped.
pub(crate) fn pass(d: &Daemon, gone: &mut Gone, panes: &Panes<'_>) -> Result<usize> {
    let sessions: Vec<Session> = d
        .db()
        .running_sessions()?
        .into_iter()
        .filter(checked)
        .collect();
    gone.retain(|id, _| sessions.iter().any(|s| &s.id == id));
    // (Nothing to look at: no tmux call.)
    if sessions.is_empty() {
        return Ok(0);
    }
    let snapshot = match panes() {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, "tmux couldn't say: no verdict this pass");
            gone.clear();
            return Ok(0);
        }
    };
    let mut stopped = 0;
    for s in &sessions {
        let dead = dead_pane(s, &snapshot);
        let seen = dead.clone();
        if count_look(s, gone, dead) < 2 {
            continue;
        }
        let Some(seen) = seen else {
            continue;
        };
        match stop(d, s, &seen, panes) {
            Ok(true) => stopped += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::debug!(session = %s.id, error = %e, "stopping a dead pane's session")
            }
        }
    }
    if stopped > 0 {
        d.changed();
    }
    Ok(stopped)
}

/// Stop session `s` (its agent pane `seen` found dead twice), through
/// `discover`'s stop. Under the session's lock (a restart through rlord
/// holds it), tmux is asked again: the same pane still dead, else
/// nothing. Then, under the DB lock with no tmux call, only if the
/// session is still as it was read: its status, when that last changed,
/// and its message times. Returns whether it was stopped.
fn stop(d: &Daemon, s: &Session, seen: &str, panes: &Panes<'_>) -> Result<bool> {
    let _session = d.sessions.lock(&s.id);
    if dead_pane(s, &panes()?).as_deref() != Some(seen) {
        return Ok(false);
    }
    let db = d.db();
    if !unchanged(&db, s) {
        return Ok(false);
    }
    crate::discover::stop_from_pane(&db, s)
}

/// Whether session `s` is still as it was read: its status, when that
/// last changed, and its message times (a hook that kept the status but
/// moved a message is news too).
fn unchanged(db: &Db, s: &Session) -> bool {
    db.resolve_ref(&s.id).is_ok_and(|cur| {
        cur.status == s.status
            && cur.last_state_change_at == s.last_state_change_at
            && cur.last_user_msg_at == s.last_user_msg_at
            && cur.last_agent_msg_at == s.last_agent_msg_at
            && cur.last_rlord_msg_at == s.last_rlord_msg_at
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlord_core::config::Config;
    use rlord_core::types::Status;
    use serde_json::json;
    use std::cell::RefCell;

    fn add(db: &Db, id: &str, agent: &str, status: Status, at: i64) {
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
    }

    /// tmux as a table: session name to its agent pane; `down`: no server.
    #[derive(Default)]
    struct Fake {
        panes: RefCell<HashMap<String, AgentPane>>,
        down: RefCell<bool>,
    }

    impl Fake {
        fn set(&self, name: &str, pane: &str, dead: bool) {
            self.panes.borrow_mut().insert(
                name.into(),
                AgentPane {
                    pane: pane.into(),
                    dead,
                },
            );
        }
        fn look(&self) -> Result<HashMap<String, AgentPane>> {
            if *self.down.borrow() {
                anyhow::bail!("no server running");
            }
            Ok(self.panes.borrow().clone())
        }
    }

    #[test]
    fn a_dead_pane_stops_its_session_on_two_answered_looks() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        {
            let db = d.db();
            add(&db, "alive", "claude", Status::Working, 5);
            add(&db, "dead", "claude", Status::WaitingInput, 5);
            db.kv_set(&crate::methods::needs_you_key("dead"), r#"[""]"#)
                .unwrap();
            // Its tmux session missing: never here (the reconcile's).
            add(&db, "missing", "claude", Status::Working, 5);
            // Other watchers' sessions: never here.
            add(&db, "oc", "opencode", Status::Idle, 5);
            add(&db, "cx", "codex", Status::Idle, 5);
        }
        let t = Fake::default();
        t.set("alive", "%1", false);
        t.set("dead", "%2", true);
        t.set("oc", "%3", true);
        t.set("cx", "%4", true);
        let panes = || t.look();
        let status = |id: &str| d.db().resolve_ref(id).unwrap().status;
        let mut gone = Gone::new();
        assert_eq!(
            pass(&d, &mut gone, &panes).unwrap(),
            0,
            "one look isn't proof"
        );
        // A failed look in between: no verdict, and the count starts again.
        *t.down.borrow_mut() = true;
        assert_eq!(pass(&d, &mut gone, &panes).unwrap(), 0);
        assert!(gone.is_empty());
        *t.down.borrow_mut() = false;
        assert_eq!(pass(&d, &mut gone, &panes).unwrap(), 0);
        assert_eq!(pass(&d, &mut gone, &panes).unwrap(), 1);
        for _ in 0..3 {
            pass(&d, &mut gone, &panes).unwrap();
        }
        assert_eq!(status("dead"), Status::Stopped);
        assert_eq!(status("alive"), Status::Working);
        assert_eq!(status("missing"), Status::Working, "missing: no verdict");
        assert_eq!(status("oc"), Status::Idle);
        assert_eq!(status("cx"), Status::Idle);
        // Stopped through `discover`'s stop: logged as the pane's end, and
        // nobody waits in it any more.
        let db = d.db();
        let last = db.list_events("dead", 1).unwrap();
        assert_eq!(
            (last[0].kind.as_str(), &last[0].payload["from"]),
            ("session_end", &json!("pane"))
        );
        assert_eq!(
            db.kv_get(&crate::methods::needs_you_key("dead")).unwrap(),
            None
        );
    }

    /// No session to look at: tmux isn't asked.
    #[test]
    fn no_sessions_no_tmux_call() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let asked = RefCell::new(false);
        let panes = || -> Result<HashMap<String, AgentPane>> {
            *asked.borrow_mut() = true;
            Ok(HashMap::new())
        };
        assert_eq!(pass(&d, &mut Gone::new(), &panes).unwrap(), 0);
        assert!(!*asked.borrow());
    }

    /// Renamed outside rlord: its stored name isn't there, so nothing is
    /// concluded, however many looks.
    #[test]
    fn a_rename_is_never_an_end() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        add(&d.db(), "s", "claude", Status::Working, 5);
        let t = Fake::default();
        t.set("my-work", "%1", true);
        let panes = || t.look();
        let mut gone = Gone::new();
        for _ in 0..4 {
            assert_eq!(pass(&d, &mut gone, &panes).unwrap(), 0);
        }
        assert_eq!(d.db().resolve_ref("s").unwrap().status, Status::Working);
    }

    /// A different pane id between the looks starts the count again; the
    /// stop asks tmux again under the session's lock: a pane that changed,
    /// or is live again (a respawn keeps the id), stops nothing.
    #[test]
    fn the_same_pane_must_be_dead_twice() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        add(&d.db(), "s", "claude", Status::Starting, 5);
        let t = Fake::default();
        t.set("s", "%1", true);
        let panes = || t.look();
        let mut gone = Gone::new();
        pass(&d, &mut gone, &panes).unwrap();
        t.set("s", "%7", true);
        assert_eq!(
            pass(&d, &mut gone, &panes).unwrap(),
            0,
            "not the pane seen dead"
        );
        assert_eq!(gone["s"].count, 1);
        let s = d.db().resolve_ref("s").unwrap();
        t.set("s", "%7", false);
        assert!(!stop(&d, &s, "%7", &panes).unwrap());
        t.set("s", "%9", true);
        assert!(!stop(&d, &s, "%7", &panes).unwrap());
        assert!(stop(&d, &s, "%9", &panes).unwrap());
    }

    /// The count starts again when the status changed since the first dead
    /// look.
    #[test]
    fn a_status_change_between_looks_starts_the_count_again() {
        let db = Db::open_in_memory().unwrap();
        add(&db, "s", "claude", Status::Idle, 5);
        let mut gone = Gone::new();
        let s1 = db.resolve_ref("s").unwrap();
        assert_eq!(count_look(&s1, &mut gone, Some("%1".into())), 1);
        db.set_status("s", Status::Starting, 9).unwrap();
        db.set_status("s", Status::Idle, 10).unwrap();
        let s2 = db.resolve_ref("s").unwrap();
        assert_eq!(count_look(&s2, &mut gone, Some("%1".into())), 1);
        assert_eq!(count_look(&s2, &mut gone, Some("%1".into())), 2);
        assert_eq!(count_look(&s2, &mut gone, None), 0);
    }

    /// A message moved since the session was read (the status kept):
    /// nothing is stopped.
    #[test]
    fn a_stop_checks_what_was_read() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        add(&d.db(), "s", "claude", Status::Working, 5);
        let t = Fake::default();
        t.set("s", "%1", true);
        let panes = || t.look();
        let seen = d.db().resolve_ref("s").unwrap();
        d.db().touch_last_user_msg("s", 7).unwrap();
        assert!(!stop(&d, &seen, "%1", &panes).unwrap());
        assert_eq!(d.db().resolve_ref("s").unwrap().status, Status::Working);
        let fresh = d.db().resolve_ref("s").unwrap();
        assert!(stop(&d, &fresh, "%1", &panes).unwrap());
        assert_eq!(d.db().resolve_ref("s").unwrap().status, Status::Stopped);
    }
}
