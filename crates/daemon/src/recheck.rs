//! Dead agent panes re-checked: a stored status re-verified against the
//! pane. Plain code, no AI. (The rules: "Stale statuses re-checked" in
//! `docs/architecture.md`.)
//!
//! A stored status moves only when an event moves it. An agent that
//! crashed without its `SessionEnd`, or a tmux session that went away,
//! leaves a live status behind. Once at daemon start and then once a
//! minute, every session that follows hooks (Claude; Codex once its hooks
//! are trusted) is looked at: its agent pane's program exited, or its tmux
//! session gone (found by its rlord mark, not its name: a rename outside
//! rlord isn't an end), on two answered looks of the same pane in a row:
//! stopped. Nothing else is concluded: when tmux can't answer (no server
//! running included), there is no verdict that pass.
//!
//! Bounded: one `tmux list-panes -a` a pass, and one more for each stop.
//! Nothing is read under the DB lock; a session is stopped only if it is
//! still as it was read, under its session lock.

use std::collections::{HashMap, HashSet};
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

/// What the checker asks tmux (tests answer it themselves).
pub(crate) trait Tmux {
    /// Every tmux session's agent pane, by name (`tmux::agent_panes`).
    fn panes(&self) -> Result<HashMap<String, AgentPane>>;
    /// Mark tmux session `name` with rlord session `id`.
    fn mark(&self, name: &str, id: &str);
}

struct Real;

impl Tmux for Real {
    fn panes(&self) -> Result<HashMap<String, AgentPane>> {
        tmux::agent_panes()
    }
    fn mark(&self, name: &str, id: &str) {
        crate::methods::mark_tmux(name, id);
    }
}

/// Start the checker: a pass now (after `Daemon::startup`'s reconcile),
/// then one every `PASS`.
pub fn start(d: &Arc<Daemon>) {
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        let mut memo = Memo::default();
        loop {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pass(&d, &mut memo, &Real)
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

/// What passes remember.
#[derive(Default)]
pub(crate) struct Memo {
    /// Per session: dead looks in a row, of which pane (None: its tmux
    /// session missing), and its `last_state_change_at` at the first.
    gone: HashMap<String, (u8, Option<String>, Option<i64>)>,
    /// Sessions seen marked with their rlord id: only these can be told
    /// gone when no tmux session carries the mark (else a rename outside
    /// rlord looks like an end).
    marked: HashSet<String>,
}

/// Session `s`'s agent pane in a snapshot: under its tmux name when that
/// session isn't marked as another rlord session's, else wherever its
/// mark is (renamed outside rlord). None: not there.
fn find<'a>(s: &Session, panes: &'a HashMap<String, AgentPane>) -> Option<&'a AgentPane> {
    s.tmux_session
        .as_deref()
        .and_then(|t| panes.get(t))
        .filter(|p| p.mark.as_deref().is_none_or(|m| m == s.id))
        .or_else(|| {
            panes
                .values()
                .find(|p| p.mark.as_deref() == Some(s.id.as_str()))
        })
}

/// What one look says of session `s`: `Some(pane)` dead (`pane` None: its
/// tmux session is gone, which only a marked session can be told), or
/// None (alive, or nothing can be concluded).
fn dead_look(
    s: &Session,
    panes: &HashMap<String, AgentPane>,
    marked: bool,
) -> Option<Option<String>> {
    match find(s, panes) {
        Some(p) => p.dead.then(|| Some(p.pane.clone())),
        None => marked.then_some(None),
    }
}

/// Count one look at session `s`. Two dead looks in a row count only if
/// they are of the same pane (or both found its tmux session gone) and
/// the status didn't change between them (a restart or a respawn: not the
/// pane seen dead). Returns the dead looks in a row.
fn count_look(s: &Session, memo: &mut Memo, look: Option<Option<String>>) -> u8 {
    let Some(pane) = look else {
        memo.gone.remove(&s.id);
        return 0;
    };
    let at = s.last_state_change_at;
    let e = memo
        .gone
        .entry(s.id.clone())
        .or_insert((0, pane.clone(), at));
    if e.0 > 0 && e.1 == pane && e.2 == at {
        e.0 = e.0.saturating_add(1);
    } else {
        *e = (1, pane, at);
    }
    e.0
}

/// One pass. When tmux can't say, no verdict this pass, and the counts
/// start again (two looks in a row means two answered ones). One
/// session's failure is logged and the pass goes on. Returns how many
/// sessions were stopped.
pub(crate) fn pass(d: &Daemon, memo: &mut Memo, tmux: &dyn Tmux) -> Result<usize> {
    let sessions: Vec<Session> = d
        .db()
        .running_sessions()?
        .into_iter()
        .filter(checked)
        .collect();
    memo.gone
        .retain(|id, _| sessions.iter().any(|s| &s.id == id));
    memo.marked
        .retain(|id| sessions.iter().any(|s| &s.id == id));
    let panes = match tmux.panes() {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, "tmux couldn't say: no verdict this pass");
            memo.gone.clear();
            return Ok(0);
        }
    };
    let mut stopped = 0;
    for s in &sessions {
        // Seen marked: from now on it is found by its mark. Not marked
        // yet (made before marks): marked now.
        match find(s, &panes) {
            Some(p) if p.mark.as_deref() == Some(s.id.as_str()) => {
                memo.marked.insert(s.id.clone());
            }
            Some(_) => {
                if let Some(t) = s.tmux_session.as_deref() {
                    tmux.mark(t, &s.id);
                }
            }
            None => {}
        }
        let look = dead_look(s, &panes, memo.marked.contains(&s.id));
        let pane = look.clone();
        if count_look(s, memo, look) < 2 {
            continue;
        }
        let seen = pane.expect("counted dead");
        match stop(d, s, &seen, memo.marked.contains(&s.id), tmux) {
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

/// Stop session `s` (its pane `seen` found dead twice; None: its tmux
/// session gone), through `discover`'s stop. Under the session's lock (a
/// restart through rlord holds it), tmux is asked again: the same pane
/// still dead (or the session still gone), else nothing. Then under the
/// DB lock, only if the session is still as it was read: its status, when
/// that last changed, and its message times. Returns whether it was
/// stopped.
fn stop(
    d: &Daemon,
    s: &Session,
    seen: &Option<String>,
    marked: bool,
    tmux: &dyn Tmux,
) -> Result<bool> {
    let _session = d.sessions.lock(&s.id);
    let now = tmux.panes()?;
    if dead_look(s, &now, marked).as_ref() != Some(seen) {
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

    /// A tmux that answers from a table: tmux session name to (pane,
    /// dead, mark). Marks set through `mark` land in it.
    #[derive(Default)]
    struct Fake {
        panes: RefCell<HashMap<String, AgentPane>>,
        down: RefCell<bool>,
    }

    impl Fake {
        fn set(&self, name: &str, pane: &str, dead: bool) {
            let mut p = self.panes.borrow_mut();
            let mark = p.get(name).and_then(|x| x.mark.clone());
            p.insert(
                name.into(),
                AgentPane {
                    pane: pane.into(),
                    dead,
                    mark,
                },
            );
        }
        fn rename(&self, from: &str, to: &str) {
            let mut p = self.panes.borrow_mut();
            let x = p.remove(from).unwrap();
            p.insert(to.into(), x);
        }
    }

    impl Tmux for Fake {
        fn panes(&self) -> Result<HashMap<String, AgentPane>> {
            if *self.down.borrow() {
                anyhow::bail!("no server running");
            }
            Ok(self.panes.borrow().clone())
        }
        fn mark(&self, name: &str, id: &str) {
            if let Some(p) = self.panes.borrow_mut().get_mut(name) {
                p.mark.get_or_insert_with(|| id.to_string());
            }
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
            // Other watchers' sessions: never here.
            add(&db, "oc", "opencode", Status::Idle, 5);
            add(&db, "cx", "codex", Status::Idle, 5);
        }
        let t = Fake::default();
        t.set("alive", "%1", false);
        t.set("dead", "%2", true);
        t.set("oc", "%3", true);
        t.set("cx", "%4", true);
        let status = |id: &str| d.db().resolve_ref(id).unwrap().status;
        let mut memo = Memo::default();
        assert_eq!(pass(&d, &mut memo, &t).unwrap(), 0, "one look isn't proof");
        // A failed look in between: no verdict, and the count starts again.
        *t.down.borrow_mut() = true;
        assert_eq!(pass(&d, &mut memo, &t).unwrap(), 0);
        assert!(memo.gone.is_empty());
        *t.down.borrow_mut() = false;
        assert_eq!(pass(&d, &mut memo, &t).unwrap(), 0);
        assert_eq!(pass(&d, &mut memo, &t).unwrap(), 1);
        assert_eq!(status("dead"), Status::Stopped);
        assert_eq!(status("alive"), Status::Working);
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

    /// Renamed outside rlord: found by its mark, alive. A missing session
    /// is gone only once it was seen marked; never marked: no verdict.
    #[test]
    fn a_rename_is_not_an_end() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        add(&d.db(), "s", "claude", Status::Working, 5);
        add(&d.db(), "u", "claude", Status::Working, 5);
        let t = Fake::default();
        t.set("s", "%1", false);
        let mut memo = Memo::default();
        let status = |id: &str| d.db().resolve_ref(id).unwrap().status;
        // First look marks it ("u" isn't there at all: never seen marked).
        pass(&d, &mut memo, &t).unwrap();
        assert_eq!(t.panes.borrow()["s"].mark.as_deref(), Some("s"));
        pass(&d, &mut memo, &t).unwrap();
        // Renamed by the user: found by its mark, nothing stops.
        t.rename("s", "my-work");
        for _ in 0..3 {
            assert_eq!(pass(&d, &mut memo, &t).unwrap(), 0);
        }
        assert_eq!(status("s"), Status::Working);
        assert_eq!(status("u"), Status::Working, "never marked: no verdict");
        // Gone for real (killed): stopped on two looks.
        t.panes.borrow_mut().remove("my-work");
        pass(&d, &mut memo, &t).unwrap();
        assert_eq!(status("s"), Status::Working);
        pass(&d, &mut memo, &t).unwrap();
        assert_eq!(status("s"), Status::Stopped);
        assert_eq!(status("u"), Status::Working);
    }

    /// A restart while the session reads "starting" (so its status time
    /// may not move): the pane changed, so the count starts again; and the
    /// stop asks tmux again, under the session's lock.
    #[test]
    fn a_new_pane_starts_the_count_again() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        add(&d.db(), "s", "claude", Status::Starting, 5);
        let t = Fake::default();
        t.set("s", "%1", true);
        let mut memo = Memo::default();
        pass(&d, &mut memo, &t).unwrap();
        // Respawned into a new pane, which also died.
        t.set("s", "%7", true);
        assert_eq!(
            pass(&d, &mut memo, &t).unwrap(),
            0,
            "not the pane seen dead"
        );
        assert_eq!(memo.gone["s"].0, 1);
        // The stop itself: the pane changed since the looks: nothing.
        let s = d.db().resolve_ref("s").unwrap();
        t.set("s", "%9", true);
        assert!(!stop(&d, &s, &Some("%7".into()), false, &t).unwrap());
        assert!(stop(&d, &s, &Some("%9".into()), false, &t).unwrap());
    }

    /// A message moved since the session was read (the status kept):
    /// nothing is stopped.
    #[test]
    fn a_stop_checks_what_was_read() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        add(&d.db(), "s", "claude", Status::Working, 5);
        let t = Fake::default();
        t.set("s", "%1", true);
        let seen = d.db().resolve_ref("s").unwrap();
        d.db().touch_last_user_msg("s", 7).unwrap();
        assert!(!stop(&d, &seen, &Some("%1".into()), false, &t).unwrap());
        assert_eq!(d.db().resolve_ref("s").unwrap().status, Status::Working);
        let fresh = d.db().resolve_ref("s").unwrap();
        assert!(stop(&d, &fresh, &Some("%1".into()), false, &t).unwrap());
        assert_eq!(d.db().resolve_ref("s").unwrap().status, Status::Stopped);
    }
}
