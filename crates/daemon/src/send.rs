//! Passing a message to an existing session: the overlord's
//! `send_to_session` tool (`session.send`). Any agent the overlord runs on
//! can use it, and any agent's session can receive it: the text is typed
//! into the session's agent pane as one bracketed paste, then Enter, the
//! same way `overlord.ask` reaches the overlord.
//!
//! Only a session whose turn is over ("your turn") is typed into. One that
//! is working, or waiting on the user (where the text would answer its
//! permission prompt), is refused, or with `when_idle` the message waits
//! for its turn to end: one pending message per session (a newer one
//! replaces it), kept in kv `send_pending` and delivered by a slow tick
//! (`start`). A session that stops, is archived or goes away drops its
//! pending message. Every send, queue, delivery and drop is in the
//! session's log.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use rlord_core::db::Db;
use rlord_core::tmux;
use rlord_core::types::{now_ms, Session, Status};

use crate::Daemon;

/// kv key: pending messages, `{session id: {"text", "at"}}`.
pub const PENDING_KEY: &str = "send_pending";

/// How often pending messages are looked at.
const TICK: Duration = Duration::from_secs(2);

/// After typing into a session, how long before a pending message may
/// follow: its status may not have left "your turn" yet (a hook, or
/// `discover`'s next look), and two messages must not land in one turn.
const SETTLE: Duration = Duration::from_secs(10);

/// A session's turn must have been over this long before a pending message
/// is delivered (not the instant between a reply and its next step).
const IDLE_FOR_MS: i64 = 2_000;

/// Longest message accepted (characters).
const MAX_CHARS: usize = 20_000;

/// One paste at a time (they share tmux's `rlord_paste` buffer), and when
/// each session was last typed into.
static TYPING: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// Type `text` into `pane` (a pane id, `%n`: the session's pinned agent
/// pane, never whatever pane is active) and submit it: out of copy mode
/// first (its keys would take the Enter), one bracketed paste, a moment
/// for it to land, then Enter. `session`: whose pane, remembered for
/// `SETTLE`.
pub fn type_into(session: &str, pane: &str, text: &str) -> Result<()> {
    let mut typed = TYPING.lock().unwrap_or_else(|e| e.into_inner());
    let _ = tmux::cancel_mode(pane);
    tmux::paste(pane, text)?;
    std::thread::sleep(Duration::from_millis(150));
    tmux::send_keys(pane, &["Enter"])?;
    typed
        .get_or_insert_with(HashMap::new)
        .insert(session.to_string(), Instant::now());
    Ok(())
}

fn typed_recently(session: &str) -> bool {
    TYPING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(session))
        .is_some_and(|t| t.elapsed() < SETTLE)
}

/// Why a session can't be sent anything at all (whatever its turn), if
/// it can't.
fn refusal(s: &Session) -> Option<String> {
    if crate::overlord::is_overlord_role(&s.role) {
        return Some("that is you (the overlord): answer the user directly".into());
    }
    if s.archived_at.is_some() {
        return Some(format!("{} is archived", s.name));
    }
    match s.status {
        Status::Stopped => Some(format!(
            "{} is stopped: restart it first (session_action restart), then send",
            s.name
        )),
        Status::Starting => Some(format!(
            "{} is still starting: try again once it shows your turn",
            s.name
        )),
        Status::Error => Some(format!("{} is in an error state", s.name)),
        Status::Idle | Status::Working | Status::WaitingInput => None,
    }
}

/// The session's agent pane, when its tmux session is up.
fn agent_pane(s: &Session) -> Result<String> {
    let t = s
        .tmux_session
        .as_deref()
        .filter(|t| tmux::has_session(t))
        .with_context(|| format!("{} has no live pane", s.name))?;
    tmux::pane_id(t)
}

/// `session.send {ref, text, when_idle}`.
pub fn send(d: &Daemon, params: &Value) -> Result<Value> {
    let r = params
        .get("ref")
        .and_then(Value::as_str)
        .context("missing 'ref'")?;
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .context("nothing to send")?;
    if text.chars().count() > MAX_CHARS {
        bail!("message too long ({MAX_CHARS} characters at most)");
    }
    let when_idle = params.get("when_idle").and_then(Value::as_bool) == Some(true);
    let s = d.db().resolve_ref(r)?;
    if let Some(why) = refusal(&s) {
        bail!("not sent: {why}");
    }
    let chars = text.chars().count();
    if s.status != Status::Idle {
        let label = s.status.label();
        if !when_idle {
            bail!(
                "not sent: {} is {label} (mid-turn). Pass when_idle: true to deliver it when its \
                 turn is over, or tell the user",
                s.name
            );
        }
        let replaced = queue(&d.db(), &s.id, text)?;
        d.changed();
        return Ok(json!({"queued": true, "session": s.name, "status": label,
                         "replaced_pending": replaced}));
    }
    // (tmux without the DB lock.)
    let pane = agent_pane(&s)?;
    type_into(&s.id, &pane, text)?;
    d.db().insert_event(
        &s.id,
        now_ms(),
        "message_sent",
        &json!({"from": "overlord", "chars": chars}).to_string(),
    )?;
    d.changed();
    Ok(json!({"sent": true, "session": s.name}))
}

fn pending(db: &Db) -> Result<Map<String, Value>> {
    Ok(db
        .kv_get(PENDING_KEY)?
        .and_then(|v| serde_json::from_str::<Value>(&v).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default())
}

fn save_pending(db: &Db, m: &Map<String, Value>) -> Result<()> {
    if m.is_empty() {
        db.kv_delete(PENDING_KEY)
    } else {
        db.kv_set(PENDING_KEY, &Value::Object(m.clone()).to_string())
    }
}

/// Hold `text` for session `id` until its turn is over. Returns whether it
/// replaced one already waiting.
fn queue(db: &Db, id: &str, text: &str) -> Result<bool> {
    let mut m = pending(db)?;
    let now = now_ms();
    let replaced = m
        .insert(id.to_string(), json!({"text": text, "at": now}))
        .is_some();
    save_pending(db, &m)?;
    db.insert_event(
        id,
        now,
        "message_queued",
        &json!({"from": "overlord", "chars": text.chars().count(), "replaced": replaced})
            .to_string(),
    )?;
    Ok(replaced)
}

/// What to do with one pending message now.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Wait,
    Deliver,
    Drop(String),
}

fn next(s: Option<&Session>, now: i64) -> Next {
    let Some(s) = s else {
        return Next::Drop("session gone".into());
    };
    if let Some(why) = refusal(s) {
        // (Still starting: a restart; it comes back to your turn.)
        if s.status != Status::Starting {
            return Next::Drop(why);
        }
        return Next::Wait;
    }
    let settled = s
        .last_state_change_at
        .is_none_or(|t| now - t >= IDLE_FOR_MS);
    if s.status == Status::Idle && settled {
        Next::Deliver
    } else {
        Next::Wait
    }
}

/// Deliver pending messages to sessions whose turn is over.
pub fn start(d: &Arc<Daemon>) {
    let d = Arc::clone(d);
    std::thread::spawn(move || loop {
        std::thread::sleep(TICK);
        if let Err(e) = tick(&d) {
            tracing::debug!(error = %e, "delivering pending session messages");
        }
    });
}

fn tick(d: &Daemon) -> Result<()> {
    let due: Vec<(Session, String)> = {
        let db = d.db();
        let mut m = pending(&db)?;
        if m.is_empty() {
            return Ok(());
        }
        let now = now_ms();
        let mut due = Vec::new();
        let mut dropped = false;
        for (id, v) in m.clone() {
            let s = db.resolve_ref(&id).ok();
            match next(s.as_ref(), now) {
                Next::Wait => {}
                Next::Deliver => {
                    let text = v["text"].as_str().unwrap_or("").to_string();
                    due.push((s.expect("checked"), text));
                }
                Next::Drop(why) => {
                    m.remove(&id);
                    dropped = true;
                    if s.is_some() {
                        db.insert_event(
                            &id,
                            now,
                            "message_dropped",
                            &json!({"reason": why}).to_string(),
                        )?;
                    }
                }
            }
        }
        if dropped {
            save_pending(&db, &m)?;
        }
        due
    };
    for (s, text) in due {
        if typed_recently(&s.id) {
            continue;
        }
        // Without the DB lock: tmux.
        let r = agent_pane(&s).and_then(|pane| type_into(&s.id, &pane, &text));
        let db = d.db();
        let mut m = pending(&db)?;
        // Replaced meanwhile by a newer message: that one waits its turn.
        if m.get(&s.id).and_then(|v| v["text"].as_str()) == Some(text.as_str()) {
            m.remove(&s.id);
            save_pending(&db, &m)?;
        }
        let (kind, detail) = match &r {
            Ok(()) => (
                "message_delivered",
                json!({"from": "overlord", "chars": text.chars().count()}),
            ),
            Err(e) => ("message_dropped", json!({"reason": format!("{e:#}")})),
        };
        db.insert_event(&s.id, now_ms(), kind, &detail.to_string())?;
        drop(db);
        d.changed();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(db: &Db) -> String {
        db.insert_session(&rlord_core::db::NewSession {
            id: "s1",
            name: "fix-login",
            agent: "codex",
            agent_session: None,
            project_path: "/tmp",
            tmux_session: Some("rlord_s1"),
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        "s1".into()
    }

    fn session(status: Status, role: &str) -> Session {
        let db = Db::open_in_memory().unwrap();
        let mut s = db.resolve_ref(&insert(&db)).unwrap();
        s.status = status;
        s.role = role.into();
        s
    }

    #[test]
    fn who_can_be_sent_to() {
        assert!(refusal(&session(Status::Idle, "")).is_none());
        assert!(refusal(&session(Status::Working, "")).is_none());
        for st in [Status::Stopped, Status::Starting, Status::Error] {
            assert!(refusal(&session(st, "")).is_some(), "{st:?}");
        }
        let mut archived = session(Status::Idle, "");
        archived.archived_at = Some(1);
        assert!(refusal(&archived).is_some());
        assert!(refusal(&session(Status::Idle, "overlord")).is_some());
    }

    #[test]
    fn a_pending_message_waits_for_a_settled_turn() {
        let now = 100_000;
        let mut s = session(Status::Working, "");
        assert_eq!(next(Some(&s), now), Next::Wait);
        s.status = Status::WaitingInput;
        assert_eq!(next(Some(&s), now), Next::Wait);
        // Its turn just ended: a moment first.
        s.status = Status::Idle;
        s.last_state_change_at = Some(now - 500);
        assert_eq!(next(Some(&s), now), Next::Wait);
        s.last_state_change_at = Some(now - IDLE_FOR_MS);
        assert_eq!(next(Some(&s), now), Next::Deliver);
        // A restart: wait for it.
        s.status = Status::Starting;
        assert_eq!(next(Some(&s), now), Next::Wait);
        // Stopped, archived or gone: dropped.
        s.status = Status::Stopped;
        assert!(matches!(next(Some(&s), now), Next::Drop(_)));
        assert!(matches!(next(None, now), Next::Drop(_)));
    }

    #[test]
    fn one_pending_message_per_session() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        assert!(!queue(&db, &id, "first").unwrap());
        assert!(queue(&db, &id, "second").unwrap());
        let m = pending(&db).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[&id]["text"], "second");
        let log = db.list_events(&id, 10).unwrap();
        assert_eq!(log.iter().filter(|e| e.kind == "message_queued").count(), 2);
        save_pending(&db, &Map::new()).unwrap();
        assert!(db.kv_get(PENDING_KEY).unwrap().is_none());
    }
}
