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
//! replaces it, and a message sent directly drops it), kept in kv
//! `send_pending` and delivered by a slow tick (`start`). A session that
//! stops, is archived or goes away drops its pending message. Every send,
//! queue, delivery and drop is in the session's log.
//!
//! Right before each paste (`type_into`), under the session's lock
//! (locks.rs: a restart, stop or archive waits for it, and it for them),
//! the session is read again and its pane is looked at. It is not typed
//! into, and the message waits (`when_idle`) or is refused, when:
//!
//! - its screen isn't at its input prompt (`Agent::at_prompt`): a dialog
//!   would take the Enter as its choice, whatever the status says;
//! - its input already holds a draft (`draft`): the paste would be added
//!   to it and sent with it;
//! - it was typed into in the last `SETTLE` (two messages in one turn);
//! - someone used a terminal attached to it in the last `USER_ACTIVE_S`
//!   (the paste would land in the middle of what they are typing). The
//!   terminal the overlord's overlay is open on doesn't count: the user is
//!   typing to the overlord in the popup, not to the session behind it.
//!
//! A queued message held back by these is logged once ("message_held",
//! with why), so a message stuck in the queue shows in the session's log.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use rlord_core::agents::{strip_ansi, Agent};
use rlord_core::db::Db;
use rlord_core::tmux;
use rlord_core::types::{now_ms, Session, Status};

use crate::Daemon;

/// kv key: pending messages, `{session id: {"text", "at"}}`.
pub const PENDING_KEY: &str = "send_pending";

/// How often pending messages are looked at.
const TICK: Duration = Duration::from_secs(2);

/// After typing into a session, how long before anything else may be
/// typed into it: its status may not have left "your turn" yet (a hook,
/// or `discover`'s next look), and two messages must not land in one turn.
const SETTLE: Duration = Duration::from_secs(10);

/// A session's turn must have been over this long before a pending message
/// is delivered (not the instant between a reply and its next step).
const IDLE_FOR_MS: i64 = 2_000;

/// A terminal attached to the session that was used this recently (tmux's
/// client activity, in seconds) means someone may be typing in it.
const USER_ACTIVE_S: i64 = 5;

/// Longest message accepted (characters).
const MAX_CHARS: usize = 20_000;

/// One paste at a time (they share tmux's `rlord_paste` buffer), and when
/// each session was last typed into. Taken last (locks.rs), and nothing
/// else is taken while it is held.
static TYPING: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// What to check right before typing into a session's pane.
pub struct Checks<'a> {
    /// Its agent: how its screen is read (`Agent::at_prompt`).
    pub agent: Agent,
    /// Not within `SETTLE` of the last paste into it.
    pub settle: bool,
    /// Its tmux session: not while someone is using a terminal attached
    /// to it.
    pub attached: Option<&'a str>,
    /// The terminal (tmux client name) the overlord's overlay is open on:
    /// its activity is the user typing in the popup, not in the session.
    pub overlay: Option<&'a str>,
    /// Not while its input holds a draft (`draft`).
    pub draft: bool,
}

/// What `type_into` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Typed {
    Done,
    /// Not typed: now is the wrong moment (why), though it may be fine
    /// later.
    NotNow(String),
}

/// Type `text` into `pane` (a pane id, `%n`: the session's pinned agent
/// pane, never whatever pane is active) and submit it, unless `checks`
/// say not now: out of copy mode first (its keys would take the Enter),
/// one bracketed paste, a moment for it to land, then Enter. `session`:
/// whose pane, remembered for `SETTLE`. The caller holds the session's
/// lock (locks.rs).
pub fn type_into(session: &str, pane: &str, text: &str, checks: &Checks) -> Result<Typed> {
    let mut typed = TYPING.lock().unwrap_or_else(|e| e.into_inner());
    let last = typed.get_or_insert_with(HashMap::new);
    if checks.settle && last.get(session).is_some_and(|t| t.elapsed() < SETTLE) {
        return Ok(Typed::NotNow(
            "it was typed into moments ago (its turn may not have started yet)".into(),
        ));
    }
    if let Some(t) = checks.attached {
        let now_s = now_ms() / 1000;
        if user_active(&tmux::list_clients(), t, checks.overlay, now_s) {
            return Ok(Typed::NotNow(
                "someone is typing in it (a terminal showing it was just used)".into(),
            ));
        }
    }
    // With attributes: a dim placeholder isn't a draft.
    let ansi = tmux::capture_pane_id_ansi(pane)?;
    if !checks.agent.at_prompt(&strip_ansi(&ansi)) {
        return Ok(Typed::NotNow(
            "its screen isn't at its input prompt (a dialog or question is up)".into(),
        ));
    }
    if checks.draft && draft(checks.agent, &ansi).is_some() {
        return Ok(Typed::NotNow(
            "its input holds unsent text (the message would be added to it)".into(),
        ));
    }
    let _ = tmux::cancel_mode(pane);
    tmux::paste(pane, text)?;
    std::thread::sleep(Duration::from_millis(150));
    tmux::send_keys(pane, &["Enter"])?;
    last.insert(session.to_string(), Instant::now());
    Ok(Typed::Done)
}

/// A terminal showing tmux session `t` was used in the last
/// `USER_ACTIVE_S` (`now_s`: epoch seconds, as `client_activity`), other
/// than `overlay` (the one the overlord's popup is open on).
fn user_active(clients: &[tmux::Client], t: &str, overlay: Option<&str>, now_s: i64) -> bool {
    clients.iter().any(|c| {
        c.session == t && Some(c.name.as_str()) != overlay && now_s - c.activity < USER_ACTIVE_S
    })
}

/// Text typed in the agent's input and not sent (`screen` captured with
/// attributes). Claude's input box (`remote::input_text`) and Codex's
/// composer (`codex::draft`) are read; OpenCode's prompt box isn't yet.
fn draft(agent: Agent, screen: &str) -> Option<String> {
    match agent {
        Agent::Claude => crate::remote::input_text(screen),
        Agent::Codex => rlord_core::agents::codex::draft(screen),
        Agent::OpenCode => None,
    }
}

/// Why a session can't be sent anything at all (whatever its turn), if
/// it can't. `limited`: when the usage limit it is stopped on resets, if
/// it is (typed in now, the message gets only another limit reply).
fn refusal(s: &Session, limited: Option<i64>) -> Option<String> {
    if crate::overlord::is_overlord_role(&s.role) {
        return Some("that is you (the overlord): answer the user directly".into());
    }
    if s.archived_at.is_some() {
        return Some(format!("{} is archived", s.name));
    }
    if let Some(t) = limited {
        return Some(if t > now_ms() {
            format!(
                "{} is stopped on a usage limit until {}",
                s.name,
                rlord_core::timefmt::clock(t)
            )
        } else {
            format!(
                "{}'s usage limit reset just now: try again in a minute",
                s.name
            )
        });
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

/// Type a message into session `s` (read under its lock, just now), after
/// the last checks (`type_into`).
pub(crate) fn type_message(d: &Daemon, s: &Session, text: &str) -> Result<Typed> {
    let pane = agent_pane(s)?;
    let overlay = d
        .overlay
        .lock()
        .expect("ui mutex")
        .as_ref()
        .map(|(client, _)| client.clone());
    let checks = Checks {
        agent: Agent::named(&s.agent)?,
        settle: true,
        attached: s.tmux_session.as_deref(),
        overlay: overlay.as_deref(),
        draft: true,
    };
    // A prompt of the user's it will type over is kept first (the resume
    // job's takeover check; rlord's own typing is told apart from the
    // event written after it).
    crate::limits::before_typing(d, s);
    type_into(&s.id, &pane, text, &checks)
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
    let id = d.db().resolve_ref(r)?.id;
    // Its lock, then a fresh look: nothing restarts, stops or archives it
    // from here until the text is in (locks.rs: this before the DB).
    let _session = d.sessions.lock(&id);
    let limited = crate::limits::limited(d).get(&id).copied();
    let s = d.db().resolve_ref(&id)?;
    if let Some(why) = refusal(&s, limited) {
        if limited.is_some() && when_idle && refusal(&s, None).is_none() {
            // Held until the limit resets (then a settled turn).
            return not_now(d, &s, text, true, &why);
        }
        bail!("not sent: {why}");
    }
    if s.status != Status::Idle {
        let why = format!("{} is {} (mid-turn)", s.name, s.status.label());
        return not_now(d, &s, text, when_idle, &why);
    }
    // (tmux without the DB lock.)
    if let Typed::NotNow(why) = type_message(d, &s, text)? {
        return not_now(d, &s, text, when_idle, &format!("{}: {why}", s.name));
    }
    let db = d.db();
    // Sent directly: a message still waiting for this session is older,
    // and must not arrive after this one.
    let replaced = unqueue(&db, &s.id, "replaced by a newer message, sent directly")?;
    db.insert_event(
        &s.id,
        now_ms(),
        "message_sent",
        &json!({"from": "overlord", "chars": text.chars().count(),
                "replaced_pending": replaced})
        .to_string(),
    )?;
    drop(db);
    d.changed();
    Ok(json!({"sent": true, "session": s.name, "replaced_pending": replaced}))
}

/// Not typed now (`why`): held for later with `when_idle`, else refused.
fn not_now(d: &Daemon, s: &Session, text: &str, when_idle: bool, why: &str) -> Result<Value> {
    if !when_idle {
        bail!(
            "not sent: {why}. Pass when_idle: true to deliver it when it can take it, or tell \
             the user"
        );
    }
    let replaced = queue(&d.db(), &s.id, text)?;
    d.changed();
    Ok(
        json!({"queued": true, "session": s.name, "status": s.status.label(),
              "why": why, "replaced_pending": replaced}),
    )
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

/// Drop session `id`'s pending message, if it has one (logged as dropped,
/// `why`, replaced). Returns whether there was one.
fn unqueue(db: &Db, id: &str, why: &str) -> Result<bool> {
    let mut m = pending(db)?;
    if m.remove(id).is_none() {
        return Ok(false);
    }
    save_pending(db, &m)?;
    db.insert_event(
        id,
        now_ms(),
        "message_dropped",
        &json!({"reason": why, "replaced": true}).to_string(),
    )?;
    Ok(true)
}

/// What to do with one pending message now.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Wait,
    Deliver,
    Drop(String),
}

fn next(s: Option<&Session>, limited: Option<i64>, now: i64) -> Next {
    let Some(s) = s else {
        return Next::Drop("session gone".into());
    };
    if let Some(why) = refusal(s, None) {
        // (Still starting: a restart; it comes back to your turn.)
        if s.status != Status::Starting {
            return Next::Drop(why);
        }
        return Next::Wait;
    }
    // On a usage limit: it comes back when the limit resets.
    if limited.is_some_and(|t| crate::limits::held(t, now)) {
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
    let limited = crate::limits::limited(d);
    let due: Vec<String> = {
        let db = d.db();
        let mut m = pending(&db)?;
        if m.is_empty() {
            return Ok(());
        }
        let now = now_ms();
        let mut due = Vec::new();
        let mut dropped = false;
        for id in m.keys().cloned().collect::<Vec<_>>() {
            let s = db.resolve_ref(&id).ok();
            match next(s.as_ref(), limited.get(&id).copied(), now) {
                Next::Wait => {}
                Next::Deliver => due.push(id),
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
    for id in due {
        deliver(d, &id)?;
    }
    Ok(())
}

/// Deliver session `id`'s pending message if it is still due: looked at
/// again under the session's lock, right before typing.
fn deliver(d: &Daemon, id: &str) -> Result<()> {
    let _session = d.sessions.lock(id);
    let limited = crate::limits::limited(d).get(id).copied();
    let (s, text) = {
        let db = d.db();
        let Ok(s) = db.resolve_ref(id) else {
            return Ok(()); // (gone: the next tick drops it)
        };
        let Some(text) = pending(&db)?
            .get(id)
            .and_then(|v| v["text"].as_str())
            .map(str::to_string)
        else {
            return Ok(()); // (sent directly meanwhile)
        };
        (s, text)
    };
    if next(Some(&s), limited, now_ms()) != Next::Deliver {
        return Ok(());
    }
    // Without the DB lock: tmux.
    let r = type_message(d, &s, &text);
    if let Ok(Typed::NotNow(why)) = &r {
        // A later tick; said once, so a stuck queue is visible.
        return held(d, id, &text, why);
    }
    let db = d.db();
    let mut m = pending(&db)?;
    // (Queued and sent under this lock: still this text.)
    if m.get(id).and_then(|v| v["text"].as_str()) == Some(text.as_str()) {
        m.remove(id);
        save_pending(&db, &m)?;
    }
    let (kind, detail) = match &r {
        Ok(_) => (
            "message_delivered",
            json!({"from": "overlord", "chars": text.chars().count()}),
        ),
        Err(e) => ("message_dropped", json!({"reason": format!("{e:#}")})),
    };
    db.insert_event(id, now_ms(), kind, &detail.to_string())?;
    drop(db);
    d.changed();
    Ok(())
}

/// Log, once per queued message, why it wasn't delivered yet (its entry
/// is marked `held`; a newer message starts unmarked).
fn held(d: &Daemon, id: &str, text: &str, why: &str) -> Result<()> {
    let db = d.db();
    let mut m = pending(&db)?;
    let Some(entry) = m.get_mut(id).and_then(Value::as_object_mut) else {
        return Ok(());
    };
    if entry.get("text").and_then(Value::as_str) != Some(text) || entry.contains_key("held") {
        return Ok(());
    }
    entry.insert("held".into(), json!(true));
    save_pending(&db, &m)?;
    db.insert_event(
        id,
        now_ms(),
        "message_held",
        &json!({"reason": why}).to_string(),
    )?;
    drop(db);
    d.changed();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code's idle screen as it really draws it (layout taken from
    /// a live 2.x pane): the input box between two rules, then status
    /// lines, the dim placeholder-free `❯ `, and an earlier reply above.
    fn claude_idle(input: &str) -> String {
        let rule = "─".repeat(90);
        [
            "✻ Sautéed for 19s · done 11:47 AM".to_string(),
            String::new(),
            "❯ 1. run the tests".to_string(),
            "● Done.".to_string(),
            rule.clone(),
            format!("❯ {input}"),
            rule,
            "  Opus 5.5 | Context 6.0% | 5h 40.0% | 7d 5.0%".to_string(),
            "  -- INSERT -- ⏵⏵ auto mode on (shift+tab to cycle)".to_string(),
        ]
        .join("\n")
    }

    #[test]
    fn claude_real_idle_screen_takes_a_send() {
        let idle = claude_idle("");
        assert!(
            Agent::Claude.at_prompt(&strip_ansi(&idle)),
            "idle box read as a dialog"
        );
        assert_eq!(
            draft(Agent::Claude, &idle),
            None,
            "empty box read as a draft"
        );
        let typed = claude_idle("half a thought");
        assert!(Agent::Claude.at_prompt(&strip_ansi(&typed)));
        assert!(
            draft(Agent::Claude, &typed).is_some(),
            "a typed draft must hold the send"
        );
    }

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

    fn refusal_now(s: &Session) -> Option<String> {
        refusal(s, None)
    }

    #[test]
    fn a_limited_session_is_not_sent_to_until_it_resets() {
        let now = 100_000;
        let mut s = session(Status::Idle, "");
        s.last_state_change_at = Some(0);
        let why = refusal(&s, Some(now_ms() + 60_000)).unwrap();
        assert!(why.contains("stopped on a usage limit until"), "{why}");
        // Past its reset, held for the margin: said so, not a past time.
        let why = refusal(&s, Some(now_ms() - 1_000)).unwrap();
        assert!(why.contains("reset just now"), "{why}");
        // A queued message waits for the reset, then goes as usual.
        assert_eq!(next(Some(&s), Some(now + 60_000), now), Next::Wait);
        assert_eq!(next(Some(&s), None, now), Next::Deliver);
        // Just past the reset: still held for the margin, like resume jobs.
        let margin = crate::limits::MARGIN_MS;
        assert_eq!(next(Some(&s), Some(now - 1), now), Next::Wait);
        assert_eq!(next(Some(&s), Some(now - margin + 1), now), Next::Wait);
        assert_eq!(next(Some(&s), Some(now - margin), now), Next::Deliver);
        // Stopped on a limit and archived: dropped, not held.
        s.archived_at = Some(1);
        assert!(matches!(
            next(Some(&s), Some(now + 60_000), now),
            Next::Drop(_)
        ));
    }

    #[test]
    fn who_can_be_sent_to() {
        assert!(refusal_now(&session(Status::Idle, "")).is_none());
        assert!(refusal_now(&session(Status::Working, "")).is_none());
        for st in [Status::Stopped, Status::Starting, Status::Error] {
            assert!(refusal_now(&session(st, "")).is_some(), "{st:?}");
        }
        let mut archived = session(Status::Idle, "");
        archived.archived_at = Some(1);
        assert!(refusal_now(&archived).is_some());
        assert!(refusal_now(&session(Status::Idle, "overlord")).is_some());
    }

    #[test]
    fn a_pending_message_waits_for_a_settled_turn() {
        let now = 100_000;
        let mut s = session(Status::Working, "");
        assert_eq!(next(Some(&s), None, now), Next::Wait);
        s.status = Status::WaitingInput;
        assert_eq!(next(Some(&s), None, now), Next::Wait);
        // Its turn just ended: a moment first.
        s.status = Status::Idle;
        s.last_state_change_at = Some(now - 500);
        assert_eq!(next(Some(&s), None, now), Next::Wait);
        s.last_state_change_at = Some(now - IDLE_FOR_MS);
        assert_eq!(next(Some(&s), None, now), Next::Deliver);
        // A restart: wait for it.
        s.status = Status::Starting;
        assert_eq!(next(Some(&s), None, now), Next::Wait);
        // Stopped, archived or gone: dropped.
        s.status = Status::Stopped;
        assert!(matches!(next(Some(&s), None, now), Next::Drop(_)));
        assert!(matches!(next(None, None, now), Next::Drop(_)));
    }

    /// A second paste right after one: not now (before any tmux call).
    #[test]
    fn one_paste_per_settle() {
        let id = "settle-test";
        TYPING
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(id.into(), Instant::now());
        let checks = Checks {
            agent: Agent::Claude,
            settle: true,
            attached: None,
            overlay: None,
            draft: true,
        };
        let r = type_into(id, "%no-such-pane", "hi", &checks).unwrap();
        assert!(matches!(r, Typed::NotNow(why) if why.contains("moments ago")));
    }

    #[test]
    fn someone_typing_in_an_attached_terminal() {
        let c = |session: &str, activity: i64| tmux::Client {
            name: "/dev/ttys001".into(),
            session: session.into(),
            activity,
        };
        let now = 1_000_000;
        assert!(user_active(
            &[c("rlord_s1", now - 1)],
            "rlord_s1",
            None,
            now
        ));
        // Idle a while, or showing another session: not typing in it.
        assert!(!user_active(
            &[c("rlord_s1", now - USER_ACTIVE_S)],
            "rlord_s1",
            None,
            now
        ));
        assert!(!user_active(&[c("rlord_s2", now)], "rlord_s1", None, now));
        assert!(!user_active(&[], "rlord_s1", None, now));
        // The overlord's overlay is open on that terminal: the user is
        // typing to the overlord in the popup, not to the session.
        let busy = [c("rlord_s1", now - 1)];
        assert!(!user_active(&busy, "rlord_s1", Some("/dev/ttys001"), now));
        // Another terminal on it still counts.
        assert!(user_active(&busy, "rlord_s1", Some("/dev/ttys009"), now));
    }

    /// A queued message held back is logged once, with why; a newer one
    /// is logged again.
    #[test]
    fn a_held_message_is_logged_once() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        queue(&db, &id, "first").unwrap();
        let d = Daemon::new(db, rlord_core::config::Config::default());
        let held_count = || {
            let log = d.db().list_events(&id, 20).unwrap();
            log.iter().filter(|e| e.kind == "message_held").count()
        };
        held(&d, &id, "first", "a dialog is up").unwrap();
        held(&d, &id, "first", "a dialog is up").unwrap();
        assert_eq!(held_count(), 1);
        assert_eq!(pending(&d.db()).unwrap()[&id]["text"], "first");
        // (Not this message's text: nothing.)
        held(&d, &id, "other", "x").unwrap();
        assert_eq!(held_count(), 1);
        queue(&d.db(), &id, "second").unwrap();
        held(&d, &id, "second", "a draft").unwrap();
        assert_eq!(held_count(), 2);
        let log = d.db().list_events(&id, 20).unwrap();
        let e = log.iter().find(|e| e.kind == "message_held").unwrap();
        assert!(e.payload["reason"].as_str().is_some(), "{}", e.payload);
    }

    #[test]
    fn a_direct_send_drops_the_pending_message() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        assert!(!unqueue(&db, &id, "x").unwrap());
        queue(&db, &id, "older").unwrap();
        assert!(unqueue(&db, &id, "replaced by a newer message, sent directly").unwrap());
        assert!(pending(&db).unwrap().is_empty());
        let log = db.list_events(&id, 10).unwrap();
        let dropped = log.iter().find(|e| e.kind == "message_dropped").unwrap();
        assert_eq!(dropped.payload["replaced"], true, "{}", dropped.payload);
    }

    /// A send waits for a restart, stop or archive holding the session,
    /// and then looks again: a session stopped meanwhile is refused.
    #[test]
    fn a_send_waits_for_the_session_and_looks_again() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        db.set_status(&id, Status::Idle, 1).unwrap();
        let d = Daemon::new(db, rlord_core::config::Config::default());
        let guard = d.sessions.lock(&id);
        let d2 = Arc::clone(&d);
        let t =
            std::thread::spawn(move || send(&d2, &json!({"ref": "fix-login", "text": "hello"})));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!t.is_finished(), "the send waits for the lock");
        d.db().set_status(&id, Status::Stopped, 2).unwrap();
        drop(guard);
        let e = t.join().unwrap().unwrap_err().to_string();
        assert!(e.contains("stopped"), "{e}");
    }

    /// The whole path on a scratch tmux server: typed when the pane is at
    /// its prompt; a dialog on screen, or a paste moments ago, holds it
    /// (queued with `when_idle`, else refused); a direct send drops what
    /// was queued. Gated: `RLORD_TMUX_TESTS=1 cargo test`.
    #[test]
    fn sends_on_a_private_tmux_server() {
        if std::env::var("RLORD_TMUX_TESTS").as_deref() != Ok("1") {
            return;
        }
        // Safety: test process only; the daemon never sets this.
        std::env::set_var("RLORD_TMUX_SOCKET", "rlord-test-send");
        let tx = |args: &[&str]| tmux::command().args(args).output().unwrap();
        tx(&["kill-server"]);
        // A pane that shows what it is given (the "agent").
        tmux::new_session("rlord_s1", "/tmp", "cat").unwrap();
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        db.set_status(&id, Status::Idle, 1).unwrap();
        let d = Daemon::new(db, rlord_core::config::Config::default());
        let screen = || tmux::capture_pane("rlord_s1").unwrap();

        // A dialog on screen: held, whatever the status says.
        tx(&[
            "send-keys",
            "-t",
            "rlord_s1",
            "-l",
            "Do you want to proceed?\r❯ 1. Yes\r  2. No\r",
        ]);
        std::thread::sleep(Duration::from_millis(200));
        let e = send(&d, &json!({"ref": &id, "text": "one"})).unwrap_err();
        assert!(e.to_string().contains("input prompt"), "{e}");
        let q = send(&d, &json!({"ref": &id, "text": "one", "when_idle": true})).unwrap();
        assert_eq!(q["queued"], true);
        assert!(!screen().contains("one\n"));

        // A draft in its composer (the session is a Codex one): held.
        let composer = |text: &str| format!("printf '› {text}\\n'; stty -echo; exec cat");
        tmux::respawn_pane("rlord_s1", "/tmp", &composer("half a draft")).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let e = send(&d, &json!({"ref": &id, "text": "one"})).unwrap_err();
        assert!(e.to_string().contains("unsent text"), "{e}");

        // At its prompt again (the placeholder is dim): typed, and the
        // queued one is dropped.
        let placeholder = composer("\\033[2mAsk Codex to do anything\\033[0m");
        tmux::respawn_pane("rlord_s1", "/tmp", &placeholder).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let v = send(&d, &json!({"ref": &id, "text": "two"})).unwrap();
        assert_eq!(v["sent"], true, "{v}");
        assert_eq!(v["replaced_pending"], true, "{v}");
        assert!(pending(&d.db()).unwrap().is_empty());
        std::thread::sleep(Duration::from_millis(200));
        assert!(screen().contains("two"), "{}", screen());

        // Right after: one paste per settle.
        let e = send(&d, &json!({"ref": &id, "text": "three"})).unwrap_err();
        assert!(e.to_string().contains("moments ago"), "{e}");
        let q = send(&d, &json!({"ref": &id, "text": "three", "when_idle": true})).unwrap();
        assert_eq!(q["queued"], true);

        tmux::kill_session("rlord_s1").unwrap();
        tx(&["kill-server"]);
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
