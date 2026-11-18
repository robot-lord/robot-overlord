//! Dead agent panes re-checked: a stored status re-verified against the
//! pane. Plain code, no AI. (The rules: "Stale statuses re-checked" in
//! `docs/architecture.md`.)
//!
//! A stored status moves only when an event moves it. An agent that
//! crashed without its `SessionEnd`, or a tmux session that went away,
//! leaves a live status behind. Once at daemon start and then once a
//! minute, every session that follows hooks (Claude; Codex once its hooks
//! are trusted) is looked at: its tmux session gone, or its agent pane's
//! program exited, on two answered looks of the same pane in a row:
//! stopped. Nothing else is concluded: when tmux can't answer (no server
//! running included), there is no verdict that pass.
//!
//! Bounded: one `tmux list-panes -a` a pass. Nothing is read under the DB
//! lock; a session is stopped only if it is still as it was read.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use rlord_core::agents::Agent;
use rlord_core::db::Db;
use rlord_core::tmux;
use rlord_core::types::Session;

use crate::Daemon;

/// Time between passes.
const PASS: Duration = Duration::from_secs(60);

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

/// One pass. `panes` is one look at every session's agent pane
/// (`tmux::agent_panes`; tests pass their own). When tmux can't say, no
/// verdict this pass, and the counts start again (two looks in a row means
/// two answered ones). One session's failure is logged and the pass goes
/// on. Returns how many sessions were stopped.
pub(crate) fn pass(
    d: &Daemon,
    gone: &mut Gone,
    panes: &dyn Fn() -> Result<HashMap<String, bool>>,
) -> Result<usize> {
    let sessions: Vec<Session> = d
        .db()
        .running_sessions()?
        .into_iter()
        .filter(checked)
        .collect();
    gone.retain(|id, _| sessions.iter().any(|s| &s.id == id));
    let dead = match panes() {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, "tmux couldn't say: no verdict this pass");
            gone.clear();
            return Ok(0);
        }
    };
    let mut stopped = 0;
    for s in &sessions {
        let Some(t) = s.tmux_session.as_deref() else {
            continue;
        };
        // Not listed: its tmux session is gone.
        let looks = count_look(s, gone, dead.get(t).copied().unwrap_or(true));
        if looks < 2 {
            continue;
        }
        match stop(&d.db(), s) {
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

/// Stop session `s` (its pane is gone), through `discover`'s stop, only if
/// it is still as it was read: its status, when that last changed, and its
/// message times (a hook that kept the status but moved a message is news
/// too). Returns whether it was stopped.
fn stop(db: &Db, s: &Session) -> Result<bool> {
    let same = db.resolve_ref(&s.id).is_ok_and(|cur| {
        cur.status == s.status
            && cur.last_state_change_at == s.last_state_change_at
            && cur.last_user_msg_at == s.last_user_msg_at
            && cur.last_agent_msg_at == s.last_agent_msg_at
            && cur.last_rlord_msg_at == s.last_rlord_msg_at
    });
    if !same {
        return Ok(false);
    }
    crate::discover::stop_from_pane(db, s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlord_core::config::Config;
    use rlord_core::types::Status;
    use serde_json::json;

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

    #[test]
    fn a_dead_pane_stops_its_session_on_two_answered_looks() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        {
            let db = d.db();
            add(&db, "alive", "claude", Status::Working, 5);
            add(&db, "dead", "claude", Status::WaitingInput, 5);
            db.kv_set(&crate::methods::needs_you_key("dead"), r#"[""]"#)
                .unwrap();
            add(&db, "vanished", "claude", Status::Idle, 5);
            // Other watchers' sessions: never here.
            add(&db, "oc", "opencode", Status::Idle, 5);
            add(&db, "cx", "codex", Status::Idle, 5);
        }
        let panes = || -> Result<HashMap<String, bool>> {
            Ok(
                [("alive", false), ("dead", true), ("oc", true), ("cx", true)]
                    .iter()
                    .map(|(k, v)| (k.to_string(), *v))
                    .collect(),
            )
        };
        let broken = || -> Result<HashMap<String, bool>> { anyhow::bail!("no server running") };
        let status = |id: &str| d.db().resolve_ref(id).unwrap().status;
        let mut gone = Gone::new();
        assert_eq!(
            pass(&d, &mut gone, &panes).unwrap(),
            0,
            "one look isn't proof"
        );
        // A failed look in between: no verdict, and the count starts again.
        assert_eq!(pass(&d, &mut gone, &broken).unwrap(), 0);
        assert!(gone.is_empty());
        assert_eq!(pass(&d, &mut gone, &panes).unwrap(), 0);
        assert_eq!(pass(&d, &mut gone, &panes).unwrap(), 2);
        assert_eq!(status("dead"), Status::Stopped);
        assert_eq!(status("vanished"), Status::Stopped);
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

    /// The look count starts again when the status changed since the first
    /// dead look (a restart or respawn in between).
    #[test]
    fn a_restart_between_looks_starts_the_count_again() {
        let db = Db::open_in_memory().unwrap();
        add(&db, "s", "claude", Status::Idle, 5);
        let mut gone = Gone::new();
        let s1 = db.resolve_ref("s").unwrap();
        assert_eq!(count_look(&s1, &mut gone, true), 1);
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

    /// A message moved since the session was read (the status kept):
    /// nothing is stopped.
    #[test]
    fn a_stop_checks_what_was_read() {
        let db = Db::open_in_memory().unwrap();
        add(&db, "s", "claude", Status::Working, 5);
        let seen = db.resolve_ref("s").unwrap();
        db.touch_last_user_msg("s", 7).unwrap();
        assert!(!stop(&db, &seen).unwrap());
        assert_eq!(db.resolve_ref("s").unwrap().status, Status::Working);
        let fresh = db.resolve_ref("s").unwrap();
        assert!(stop(&db, &fresh).unwrap());
        assert_eq!(db.resolve_ref("s").unwrap().status, Status::Stopped);
    }
}
