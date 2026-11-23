//! Passing a message to an existing session: the overlord's
//! `send_to_session` tool (`session.send`). Any agent the overlord runs on
//! can use it, and any agent's session can receive it. The text is framed
//! by code (`context::frame_message`): the user's words, then the
//! overlord's optional `context`, labelled as not the user's.
//!
//! How it gets there, per agent:
//!
//! - **OpenCode:** POSTed to its server with `delivery: "queue"`
//!   (`Service::queue_prompt`), mid-turn only with `when_idle`: OpenCode's inbox runs
//!   it once the current turn is over. Only a POST that surely failed
//!   falls back to typing, below.
//! - **Claude and Codex, mid-turn** (with `when_idle`): held, and claimed by
//!   the session's own Stop hook (`stop_claim`): the hook answers
//!   `decision: block` with the message as its reason, so the agent
//!   carries on with it instead of ending its turn. At most `CLAIMS_MAX`
//!   in a row; then the turn ends and it is typed in.
//! - **Idle:** typed into the agent pane as one bracketed paste, then
//!   Enter, the same way `overlord.ask` reaches the overlord.
//!
//! Only a session whose turn is over ("your turn") is typed into. One that
//! is working, or waiting on the user (where the text would answer its
//! permission prompt), is refused, or with `when_idle` the message waits
//! for its turn to end. Messages that wait add up: a later one joins the
//! pending ones (kept in kv `send_pending`, framed as one), and a message
//! sent directly takes them with it. A slow tick (`start`) types them in
//! once the turn is over. A session that stops, is archived or goes away
//! drops its pending messages. Every send, queue, delivery and drop is in
//! the session's log.
//!
//! Right before each paste (`type_into`), under the session's lock
//! (locks.rs: a restart, stop or archive waits for it, and it for them),
//! the session is read again and its pane is looked at. It is not typed
//! into, and the message waits (`when_idle`) or is refused, when:
//!
//! - its screen isn't at its input prompt (`Agent::at_prompt`): a dialog
//!   would take the Enter as its choice, whatever the status says;
//! - its input already holds a draft (`draft`): the paste would be added
//!   to it and sent with it (Claude: stashed instead when safe, stash.rs);
//! - it was typed into in the last `SETTLE` (two messages in one turn);
//! - someone used a terminal attached to it in the last `USER_ACTIVE_S`
//!   (the paste would land in the middle of what they are typing). The
//!   terminal the overlord's overlay is open on doesn't count: the user is
//!   typing to the overlord in the popup, not to the session behind it.
//!
//! Once every check passed, the prompt the paste will make is marked as
//! rlord's (`own.rs`), so the agent's report of it never counts as the
//! user typing.
//!
//! A queued message held back by these is logged once ("message_held",
//! with why), so a message stuck in the queue shows in the session's log,
//! and its row in the list shows it waiting (`annotate`: `✉`, `✉ draft`).
//!
//! A draft in a **Claude** session's input doesn't hold a message for
//! long: once it has sat unchanged a while, Claude Code's own prompt
//! stash sets it aside, the message goes in, and the draft is put back
//! after the turn (stash.rs).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::opencode_service::{Posted, Service};
use rlord_core::agents::{strip_ansi, Agent};
use rlord_core::db::Db;
use rlord_core::tmux;
use rlord_core::types::{now_ms, Session, Status};

use crate::own::{self, Source};
use crate::Daemon;

/// kv key: pending messages, `{session id: {"parts": [{"text",
/// "context"}], "text", "at"}}`: `parts` are the messages that wait, in
/// order; `text` is all of them framed as one (what is delivered). While
/// a Stop hook's answer carries them, `claim` (its ack id) and `claimed`
/// (how many parts it carried).
pub const PENDING_KEY: &str = "send_pending";

/// How many Stop hooks in a row may carry a held message on: then the
/// turn ends, and what is left is typed in at the idle prompt.
pub const CLAIMS_MAX: u64 = 3;

/// Whether Codex's Stop hook claims held messages. On since its block
/// (`decision: block`) was checked live (Codex 0.169.0, in the pane): the
/// reason reaches the agent, which goes on in the same turn. In-pane
/// sessions only: on Codex's daemon (hooks run there) it isn't checked
/// live yet, and those take them at the idle prompt.
pub const CODEX_STOP_CLAIM: bool = true;

/// How long the idle paste leaves alone messages a Stop hook's answer
/// carries, waiting for its ack.
const CLAIM_GRACE_MS: i64 = 10_000;

/// kv key: how many Stop hooks in a row carried a held message on, for
/// session `id`. Cleared by a prompt and by a Stop that carries nothing.
fn claims_key(id: &str) -> String {
    format!("stop_claims:{id}")
}

/// kv key: first prompts that couldn't go another way (an OpenCode launch
/// whose server POST surely failed), `{session id: {"text", "at"}}`. Their
/// own slot: delivered before, and never replaced by, the overlord's
/// messages.
pub const FIRST_KEY: &str = "send_pending_first";

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
/// else is taken while it is held. (The caller's session lock keeps two
/// pastes into one session apart; this, pastes into different ones.)
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
    /// Who is typing, when it is rlord's own message: the prompt this
    /// makes is marked as rlord's (`own::mark`), not the user's. None:
    /// the text is the user's own (`overlord ask`), and so is its prompt.
    pub source: Option<Source>,
}

/// What `type_into` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Typed {
    Done,
    /// Not typed: now is the wrong moment (why), though it may be fine
    /// later.
    NotNow(Why),
}

/// Why a message wasn't typed now.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Why {
    pub text: String,
    /// Held by a draft in the session's input (the row shows `✉ draft`).
    pub draft: bool,
}

impl Why {
    fn draft(text: impl Into<String>) -> Self {
        Why {
            text: text.into(),
            draft: true,
        }
    }
}

impl From<String> for Why {
    fn from(text: String) -> Self {
        Why { text, draft: false }
    }
}

impl From<&str> for Why {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Type `text` into `pane` (a pane id, `%n`: the session's pinned agent
/// pane, never whatever pane is active) and submit it, unless `checks`
/// say not now: out of copy mode first (its keys would take the Enter),
/// one bracketed paste, a moment for it to land, then Enter. `session`:
/// whose pane, remembered for `SETTLE`. Right before the paste the prompt
/// to come is marked as rlord's (`own::mark`, durable; not for the
/// user's own text, `Checks::source`), so it is never taken for the
/// user's; a paste that fails takes its marker back. The
/// caller holds the session's lock (locks.rs), and not the DB.
pub fn type_into(
    d: &Daemon,
    session: &str,
    pane: &str,
    text: &str,
    checks: &Checks,
) -> Result<Typed> {
    let settled = || {
        let typed = TYPING.lock().unwrap_or_else(|e| e.into_inner());
        !checks.settle
            || typed
                .as_ref()
                .and_then(|m| m.get(session))
                .is_none_or(|t| t.elapsed() >= SETTLE)
    };
    if !settled() {
        return Ok(Typed::NotNow(
            "it was typed into moments ago (its turn may not have started yet)".into(),
        ));
    }
    if let Some(t) = checks.attached {
        let now_s = now_ms() / 1000;
        if user_active(&tmux::list_clients(), t, checks.overlay, now_s) {
            // Someone at it: a draft there may be in the making.
            crate::stash::forget_draft(session);
            return Ok(Typed::NotNow(
                "someone is typing in it (a terminal showing it was just used)".into(),
            ));
        }
    }
    // With attributes: a dim placeholder isn't a draft.
    let ansi = tmux::capture_pane_id_ansi(pane)?;
    let plain = strip_ansi(&ansi);
    if !checks.agent.at_prompt(&plain) {
        return Ok(Typed::NotNow(
            "its screen isn't at its input prompt (a dialog or question is up)".into(),
        ));
    }
    if let Some(why) = resume_held(checks, &plain) {
        return Ok(Typed::NotNow(why.into()));
    }
    // A draft holds the message, except in Claude, whose own prompt
    // stash sets it aside (stash.rs) when that is safe.
    let mut stash: Option<String> = None;
    if checks.draft {
        match draft(checks.agent, &ansi) {
            Some(first_line) if checks.agent == Agent::Claude => {
                // Aged by the whole box, not just its first line.
                let whole = crate::stash::input_region(&plain).unwrap_or_default();
                let age = crate::stash::draft_age(session, &whole);
                let pending = crate::stash::pending(d, session);
                match crate::stash::plan(&plain, crate::stash::stash_key_ok(), pending, age) {
                    crate::stash::Plan::Hold(why) => return Ok(Typed::NotNow(Why::draft(why))),
                    crate::stash::Plan::Stash => stash = Some(first_line),
                }
            }
            Some(_) => {
                return Ok(Typed::NotNow(Why::draft(
                    "its input holds unsent text (a draft; the message would be added to it)",
                )))
            }
            None => crate::stash::forget_draft(session),
        }
    }
    // (Without `TYPING`: this takes the DB.) Not typed without its
    // marker: the prompt would be counted as the user's.
    let marked = match checks.source {
        Some(source) => match own::mark(&d.db(), session, text, source, now_ms()) {
            Ok(at) => Some(at),
            Err(e) => {
                tracing::warn!(session, error = %e, "marking rlord's own prompt");
                return Ok(Typed::NotNow(
                    "rlord couldn't record that it is the one typing (its database)".into(),
                ));
            }
        },
        None => None,
    };
    let unmark = || {
        if let Some(at) = marked {
            let _ = own::unmark(&d.db(), session, at);
        }
    };
    if let Some(first_line) = &stash {
        // Ctrl+S, then a look. (`TYPING` only around the keys: recording
        // the stash takes the DB.)
        let after = {
            let _typing = typing();
            let _ = tmux::cancel_mode(pane);
            tmux::send_keys(pane, &["C-s"]).and_then(|()| {
                std::thread::sleep(crate::stash::REDRAW);
                tmux::capture_pane_id_ansi(pane)
            })
        };
        let step = crate::stash::after_stash(after.as_ref().ok().map(String::as_str));
        // Recorded before anything is typed: whatever happens next, the
        // restore looks for it.
        if step != crate::stash::AfterStash::Untouched {
            if let Err(e) = crate::stash::stashed(d, session, first_line) {
                tracing::warn!(session, error = %e, "recording a stashed draft");
            }
        }
        if step != crate::stash::AfterStash::Paste {
            unmark();
            tracing::warn!(
                session,
                ?step,
                "Claude's prompt stash: not typing this time"
            );
            return Ok(Typed::NotNow(Why::draft(
                "its input held a draft, and Claude Code's prompt stash (Ctrl+S) didn't clearly \
                 take it: tried again later",
            )));
        }
    }
    let mut typed = typing();
    let last = typed.get_or_insert_with(HashMap::new);
    let _ = tmux::cancel_mode(pane);
    let pasted = tmux::paste(pane, text).and_then(|()| {
        std::thread::sleep(Duration::from_millis(150));
        tmux::send_keys(pane, &["Enter"])
    });
    if let Err(e) = pasted {
        drop(typed);
        unmark();
        return Err(e);
    }
    last.insert(session.to_string(), Instant::now());
    Ok(Typed::Done)
}

/// `TYPING`, for another key press into a pane (stash.rs's restore).
pub(crate) fn typing() -> std::sync::MutexGuard<'static, Option<HashMap<String, Instant>>> {
    TYPING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Session `id` wasn't typed into in the last `SETTLE`.
pub(crate) fn settled_since_typed(id: &str) -> bool {
    typing()
        .as_ref()
        .and_then(|m| m.get(id))
        .is_none_or(|t| t.elapsed() >= SETTLE)
}

/// Someone used a terminal showing tmux session `t` just now (the
/// overlord's overlay aside is not known here: counted).
pub(crate) fn user_typing(t: &str) -> bool {
    user_active(&tmux::list_clients(), t, None, now_ms() / 1000)
}

/// Why a resume after a usage limit (`Source::Resume`) can't be typed
/// into this screen now, if it can't: OpenCode counting down its own
/// retry (`opencode::retrying`). The message would stack on that turn;
/// the job tries again next tick.
fn resume_held(checks: &Checks, screen: &str) -> Option<&'static str> {
    (checks.source == Some(Source::Resume)
        && checks.agent == Agent::OpenCode
        && rlord_core::agents::opencode::retrying(screen))
    .then_some("OpenCode is retrying the request itself (\"retrying in …\")")
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
/// attributes): Claude's input box (`remote::input_text`), Codex's
/// composer (`codex::draft`), OpenCode's prompt box (`opencode::draft`).
fn draft(agent: Agent, screen: &str) -> Option<String> {
    match agent {
        Agent::Claude => crate::remote::input_text(screen),
        Agent::Codex => rlord_core::agents::codex::draft(screen),
        Agent::OpenCode => rlord_core::agents::opencode::draft(screen),
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
/// the last checks (`type_into`). `source`: who sends it (None: the
/// user's own words, a first prompt).
pub(crate) fn type_message(
    d: &Daemon,
    s: &Session,
    text: &str,
    source: Option<Source>,
) -> Result<Typed> {
    // Its first prompt goes before anything rlord types (`source`); the
    // first prompt itself (None) is exempt.
    if source.is_some() && first_waiting(&d.db(), &s.id)? {
        return Ok(Typed::NotNow(
            format!("{}'s first prompt hasn't gone in yet", s.name).into(),
        ));
    }
    let pane = agent_pane(s)?;
    // On Codex's daemon another client may have started a turn rlord's
    // status doesn't show: the daemon says.
    if let Some(why) = crate::codex_daemon::busy_for_send(s) {
        return Ok(Typed::NotNow(why.into()));
    }
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
        source,
    };
    type_into(d, &s.id, &pane, text, &checks)
}

/// `session.send {ref, text, context, when_idle}`.
pub fn send(d: &Daemon, params: &Value) -> Result<Value> {
    let r = params
        .get("ref")
        .and_then(Value::as_str)
        .context("missing 'ref'")?;
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .map(clean_words)
        .filter(|t| !t.is_empty())
        .context("nothing to send")?;
    let text = text.as_str();
    let background = params
        .get("context")
        .and_then(Value::as_str)
        .map(crate::context::clean)
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty());
    let part = json!({"text": text, "context": background});
    if framed(std::slice::from_ref(&part)).chars().count() > MAX_CHARS {
        bail!("message too long ({MAX_CHARS} characters at most, with its context)");
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
            // Held until the limit resets (then a settled turn). (Not
            // "try again": it is queued.)
            let why = match limited.filter(|t| *t > now_ms()) {
                Some(t) => format!(
                    "{} is stopped on a usage limit until {}: queued, it goes once the limit has cleared",
                    s.name,
                    rlord_core::timefmt::clock(t)
                ),
                None => format!(
                    "{}'s usage limit reset just now: queued, it goes once the limit has cleared",
                    s.name
                ),
            };
            return not_now(d, &s, &part, true, &why);
        }
        bail!("not sent: {why}");
    }
    // Mid-turn or needing the user: only with `when_idle`, for every
    // agent (OpenCode's queue too).
    if s.status != Status::Idle && !when_idle {
        let why = format!("{} is {} (mid-turn)", s.name, s.status.label());
        return not_now(d, &s, &part, false, &why);
    }
    // A Stop hook's answer is carrying the waiting ones right now: this
    // one joins the queue (sent with them it could arrive twice).
    if claim_out(&d.db(), &s.id)? {
        let why = format!("{} is taking its waiting messages now", s.name);
        return not_now(d, &s, &part, when_idle, &why);
    }
    // Messages still waiting for it go first, with this one, as one.
    let waiting = pending_parts(&d.db(), &s.id)?;
    let mut parts = waiting.clone();
    parts.push(part.clone());
    let whole = framed(&parts);
    if whole.chars().count() > MAX_CHARS {
        bail!(
            "not sent: with the {} message(s) waiting for {} it would be too long \
             ({MAX_CHARS} characters at most)",
            waiting.len(),
            s.name
        );
    }
    // OpenCode: its server's inbox queues it behind any turn (first
    // prompt first). Only a POST that surely failed goes the pane's way.
    if Agent::named(&s.agent)? == Agent::OpenCode {
        if first_waiting(&d.db(), &s.id)? {
            let why = format!("{}'s first prompt hasn't gone in yet", s.name);
            return not_now(d, &s, &part, when_idle, &why);
        }
        match post(d, &s, &whole) {
            Some(Posted::Refused(why)) => {
                tracing::info!(session = %s.id, why, "OpenCode refused a message: typing it");
            }
            None => {}
            Some(p) => return sent(d, &s, &waiting, &whole, Via::Server(p)),
        }
    }
    if s.status != Status::Idle {
        let why = format!("{} is {} (mid-turn)", s.name, s.status.label());
        return not_now(d, &s, &part, when_idle, &why);
    }
    // (tmux without the DB lock.)
    if let Typed::NotNow(why) = type_message(d, &s, &whole, Some(Source::Send))? {
        return not_now(d, &s, &part, when_idle, &format!("{}: {why}", s.name));
    }
    sent(d, &s, &waiting, &whole, Via::Typed)
}

/// The user's words made safe like the overlord's notes
/// (`context::clean`: no control, escape, bidi or zero-width characters),
/// but keeping their tabs; trimmed.
fn clean_words(text: &str) -> String {
    text.split('\t')
        .map(crate::context::clean)
        .collect::<Vec<_>>()
        .join("\t")
        .trim()
        .to_string()
}

/// How a message went in.
enum Via {
    Typed,
    /// POSTed to OpenCode's server (`Sent`, or `Unknown`: it may have
    /// been admitted, so it is never sent again).
    Server(Posted),
}

/// `whole` went in (`via`), carrying `taken`, the messages that were
/// waiting: those leave the queue, and the log says so.
fn sent(d: &Daemon, s: &Session, taken: &[Value], whole: &str, via: Via) -> Result<Value> {
    let db = d.db();
    take(&db, &s.id, taken)?;
    let (how, unknown) = match &via {
        Via::Typed => ("typed", None),
        Via::Server(Posted::Unknown(why)) => ("server", Some(why.clone())),
        Via::Server(_) => ("server", None),
    };
    db.insert_event(
        &s.id,
        now_ms(),
        "message_sent",
        &json!({"from": "overlord", "chars": whole.chars().count(), "via": how,
                "with_pending": taken.len(), "unknown": unknown})
        .to_string(),
    )?;
    drop(db);
    d.changed();
    let mut v = json!({"sent": true, "session": s.name, "via": how,
                       "with_pending": taken.len()});
    if let Some(why) = unknown {
        v["sent"] = json!("unknown");
        v["why"] = json!(format!(
            "OpenCode didn't answer; it may have taken it, so it isn't sent again: {why}"
        ));
    } else if how == "server" {
        v["note"] = json!("in its inbox: it runs once the current turn is over");
    }
    Ok(v)
}

/// POST `text` to OpenCode session `s`'s server, queued behind its turn
/// (`Service::queue_prompt`), marked as rlord's first (`own::mark`, taken
/// back if surely refused). None: no server or session id to post to, or
/// the marker couldn't be written; it goes the pane's way.
fn post(d: &Daemon, s: &Session, text: &str) -> Option<Posted> {
    let id = s.agent_session.as_deref()?;
    let svc = Service::read(&rlord_core::agents::opencode::service_file())?;
    let at = own::mark(&d.db(), &s.id, text, Source::Send, now_ms()).ok()?;
    let r = svc.queue_prompt(id, text);
    if matches!(r, Posted::Refused(_)) {
        let _ = own::unmark(&d.db(), &s.id, at);
    }
    Some(r)
}

/// Not typed now (`why`): held for later with `when_idle`, else refused.
fn not_now(d: &Daemon, s: &Session, part: &Value, when_idle: bool, why: &str) -> Result<Value> {
    if !when_idle {
        bail!(
            "not sent: {why}. Pass when_idle: true to deliver it when it can take it, or tell \
             the user"
        );
    }
    let earlier = queue(&d.db(), &s.id, part)?;
    d.changed();
    Ok(
        json!({"queued": true, "session": s.name, "status": s.status.label(),
              "why": why, "with_pending": earlier}),
    )
}

/// Whether a Stop hook's answer is carrying this pending entry now,
/// waiting for its ack. Only for a while: a lost ack leaves it to the
/// paste.
fn claimed_now(entry: &Value) -> bool {
    entry["claim_at"]
        .as_i64()
        .is_some_and(|t| now_ms() - t < CLAIM_GRACE_MS)
}

/// `claimed_now` for session `id`'s pending messages.
fn claim_out(db: &Db, id: &str) -> Result<bool> {
    Ok(db.kv_map(PENDING_KEY)?.get(id).is_some_and(claimed_now))
}

/// All `parts` framed as one message: one header, then each part as
/// `context::frame_message` frames it (the user's words, then the
/// overlord's background for them), in order.
pub(crate) fn framed(parts: &[Value]) -> String {
    let header = crate::context::frame_message("", None);
    let body = |p: &Value| {
        let text = p["text"].as_str().unwrap_or_default();
        let one = crate::context::frame_message(text, p["context"].as_str());
        one.strip_prefix(&header).unwrap_or(&one).to_string()
    };
    let bodies: Vec<String> = parts.iter().map(body).collect();
    format!("{header}{}", bodies.join("\n\n---\n\n"))
}

/// The messages waiting for session `id`, in order.
fn pending_parts(db: &Db, id: &str) -> Result<Vec<Value>> {
    Ok(db
        .kv_map(PENDING_KEY)?
        .get(id)
        .map_or_else(Vec::new, parts_of))
}

/// A pending entry's messages. One written before parts existed (only
/// `text`, maybe `context`) is one message, so taking it clears it.
fn parts_of(entry: &Value) -> Vec<Value> {
    if let Some(parts) = entry["parts"].as_array() {
        return parts.clone();
    }
    match entry["text"].as_str() {
        Some(text) => vec![json!({"text": text, "context": entry["context"].as_str()})],
        None => Vec::new(),
    }
}

/// Hold message `part` for session `id` until it can take it, after any
/// already waiting (all framed as one). Returns how many were waiting.
pub(crate) fn queue(db: &Db, id: &str, part: &Value) -> Result<usize> {
    let mut m = db.kv_map(PENDING_KEY)?;
    let now = now_ms();
    let mut parts = m.get(id).map_or_else(Vec::new, parts_of);
    let earlier = parts.len();
    parts.push(part.clone());
    let text = framed(&parts);
    if text.chars().count() > MAX_CHARS {
        bail!(
            "not queued: with the {earlier} message(s) already waiting it would be too long \
             ({MAX_CHARS} characters at most)"
        );
    }
    // (A Stop hook's claim on the earlier ones stays: `claimed` counts
    // them.)
    let entry = m.entry(id.to_string()).or_insert_with(|| json!({}));
    entry["parts"] = json!(parts);
    entry["text"] = json!(text);
    entry["at"] = json!(now);
    if let Some(e) = entry.as_object_mut() {
        e.remove("held");
    }
    db.kv_set_map(PENDING_KEY, &m)?;
    db.insert_event(
        id,
        now,
        "message_queued",
        &json!({"from": "overlord", "chars": part["text"].as_str().map_or(0, |t| t.chars().count()),
                "with_pending": earlier})
        .to_string(),
    )?;
    Ok(earlier)
}

/// `taken` (the first of session `id`'s waiting messages, as read) went
/// in: they leave the queue. Messages added since stay, framed anew; if
/// the queue no longer starts with them (another path took them), it is
/// left alone.
fn take(db: &Db, id: &str, taken: &[Value]) -> Result<()> {
    if taken.is_empty() {
        return Ok(());
    }
    let mut m = db.kv_map(PENDING_KEY)?;
    let Some(entry) = m.get(id) else {
        return Ok(());
    };
    let parts = parts_of(entry);
    if !parts.starts_with(taken) {
        return Ok(());
    }
    let rest = &parts[taken.len()..];
    if rest.is_empty() {
        m.remove(id);
    } else {
        let at = entry["at"].clone();
        m.insert(
            id.to_string(),
            json!({"parts": rest, "text": framed(rest), "at": at}),
        );
    }
    db.kv_set_map(PENDING_KEY, &m)
}

/// A Stop hook of session `s`'s own conversation (Claude, Codex) while
/// messages wait for it: claim them. The answer blocks the stop
/// (`decision: block`) with them as its reason, so the agent goes on with
/// them in the same turn. They leave the queue only when the hook client
/// acknowledges it printed the answer (`claim_ack`); unacknowledged, the
/// next Stop (or the idle prompt) carries them again. At most
/// `CLAIMS_MAX` in a row: then the turn ends, and they are typed in.
/// None (and the count cleared) when nothing waits, or its first prompt
/// hasn't gone in.
pub(crate) fn stop_claim(
    db: &Db,
    s: &Session,
    agent: Agent,
    now: i64,
) -> Result<Option<crate::context::Reply>> {
    let key = claims_key(&s.id);
    // Most Stops: nothing waits (one read, one delete ending a run).
    let mut m = db.kv_map(PENDING_KEY)?;
    if !m.get(&s.id).is_some_and(|e| !parts_of(e).is_empty()) {
        db.kv_delete(&key)?;
        return Ok(None);
    }
    // Not for a session that can't take it now: stopped, restarting or
    // in error, or on a usage limit. (A turn that ended on an API error,
    // a usage limit included, ends with Claude's `StopFailure`, never
    // `Stop`: so this Stop isn't a limit reply.) Codex while
    // `CODEX_STOP_CLAIM` is on, in the pane only, and not when its turn
    // ended on an error or limit (it has no `StopFailure`: its
    // transcript's last `task_complete` says).
    let claims = match agent {
        Agent::Claude => true,
        Agent::Codex => CODEX_STOP_CLAIM,
        Agent::OpenCode => false,
    };
    if !claims
        || crate::overlord::is_overlord_role(&s.role)
        || s.archived_at.is_some()
        || matches!(s.status, Status::Stopped | Status::Starting | Status::Error)
        || crate::limits::limited_in(db, &s.id, now)
        || (agent == Agent::Codex && !codex_claims(db, s))
    {
        return Ok(None);
    }
    let in_row: u64 = db.kv_get(&key)?.and_then(|v| v.parse().ok()).unwrap_or(0);
    if in_row >= CLAIMS_MAX || first_waiting(db, &s.id)? {
        db.kv_delete(&key)?;
        return Ok(None);
    }
    let Some(entry) = m.get_mut(&s.id).and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    let ack = format!("{CLAIM_PREFIX}{}:{now}", s.id);
    let claimed = entry
        .get("parts")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let text = entry
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    entry.insert("claim".into(), json!(ack));
    entry.insert("claimed".into(), json!(claimed));
    entry.insert("claim_at".into(), json!(now));
    db.kv_set_map(PENDING_KEY, &m)?;
    Ok(Some(crate::context::Reply {
        output: json!({"decision": "block", "reason": text}),
        ack,
    }))
}

/// Codex's own conditions for a Stop claim: an in-pane session whose last
/// turn didn't end on an error (see `stop_claim`).
fn codex_claims(db: &Db, s: &Session) -> bool {
    let spec: Value = db
        .launch_spec(&s.id)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if rlord_core::agents::codex::on_daemon(&spec) {
        return false;
    }
    !db.transcript_source(&s.id)
        .ok()
        .flatten()
        .is_some_and(|p| rlord_core::agents::codex::last_turn_failed(std::path::Path::new(&p)))
}

/// What a Stop-hook claim's ack id starts with (`hook.ack` tells them
/// from the background's).
pub const CLAIM_PREFIX: &str = "send:";

/// A prompt went in (the user's or rlord's): the Stop hooks that carried
/// messages on are no longer "in a row".
pub(crate) fn claims_reset(db: &Db, id: &str) -> Result<()> {
    db.kv_delete(&claims_key(id))
}

/// The hook client printed the Stop-hook answer `ack` names: the messages
/// it carried leave the queue (those added since stay) and the log says
/// they were delivered. Returns whether `ack` was still the claim.
pub(crate) fn claim_ack(db: &Db, ack: &str, now: i64) -> Result<bool> {
    let Some(id) = ack
        .strip_prefix(CLAIM_PREFIX)
        .and_then(|rest| rest.rsplit_once(':'))
        .map(|(id, _)| id)
    else {
        return Ok(false);
    };
    let entry = db.kv_map(PENDING_KEY)?.get(id).cloned();
    let Some(entry) = entry.filter(|e| e["claim"] == ack) else {
        return Ok(false);
    };
    let parts = parts_of(&entry);
    let n = entry["claimed"]
        .as_u64()
        .map_or(parts.len(), |n| n as usize)
        .min(parts.len());
    let taken = &parts[..n];
    take(db, id, taken)?;
    // One more Stop in a row carried messages on (counted only once they
    // surely went).
    let key = claims_key(id);
    let in_row: u64 = db.kv_get(&key)?.and_then(|v| v.parse().ok()).unwrap_or(0);
    db.kv_set(&key, &(in_row + 1).to_string())?;
    // The Stop set it idle; it carries on with them. (Only from idle: a
    // stop or restart meanwhile wins.)
    // (Logged only when it moved the status: the event is read as what
    // ended a wait.)
    if let Some(s) = db.session_by_id(id)?.filter(|s| s.status == Status::Idle) {
        if db.set_status(id, Status::Working, now)? {
            crate::reason::record(db, &s, "message_delivered", Status::Working, false, false)?;
            db.insert_event(
                id,
                now,
                "message_delivered",
                &json!({"from": "overlord", "via": "stop_hook", "messages": n,
                        "chars": framed(taken).chars().count()})
                .to_string(),
            )?;
        }
    }
    Ok(true)
}

/// Whether session `id`'s first prompt hasn't reached it yet: held for its
/// server (`first_prompt::pending_key`) or for typing (`FIRST_KEY`). The
/// overlord's messages wait for it.
fn first_waiting(db: &Db, id: &str) -> Result<bool> {
    Ok(db.kv_map(FIRST_KEY)?.contains_key(id) || crate::first_prompt::waiting(db, id, now_ms())?)
}

/// How many times typing a first prompt may fail before it is dropped.
const FIRST_TRIES: u64 = 5;

/// Hold first prompt `text` of session `id` for its first idle turn (its
/// own slot, `FIRST_KEY`).
pub(crate) fn queue_first(db: &Db, id: &str, text: &str) -> Result<()> {
    let mut m = db.kv_map(FIRST_KEY)?;
    m.insert(id.to_string(), json!({"text": text, "at": now_ms()}));
    db.kv_set_map(FIRST_KEY, &m)
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

/// Annotate list rows with `mail`: `{"held": why or null, "draft": bool}`
/// for a session with messages waiting for it (`draft`: held by a draft
/// in its input), else null.
pub fn annotate(d: &Daemon, rows: &mut [Value]) {
    let m = d.db().kv_map(PENDING_KEY).unwrap_or_default();
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        row["mail"] = match m.get(id).filter(|e| !parts_of(e).is_empty()) {
            Some(e) => {
                let why = e["held"].as_str();
                json!({"held": why, "draft": why.is_some() && e["held_draft"] == true})
            }
            None => Value::Null,
        };
    }
}

/// A hold lifts when its session leaves "your turn" (a turn started:
/// whatever held it, the next look decides afresh). Returns whether
/// `entry` changed.
fn lift_hold(entry: &mut Value, s: Option<&Session>) -> bool {
    let busy = s.is_some_and(|s| s.status != Status::Idle);
    match entry.as_object_mut() {
        Some(e) if busy && e.contains_key("held") => {
            e.remove("held");
            e.remove("held_draft");
            true
        }
        _ => false,
    }
}

fn tick(d: &Daemon) -> Result<()> {
    let limited = crate::limits::limited(d);
    let mut due: Vec<(String, &'static str)> = Vec::new();
    {
        let db = d.db();
        for key in [FIRST_KEY, PENDING_KEY] {
            let mut m = db.kv_map(key)?;
            let now = now_ms();
            let mut dirty = false;
            for id in m.keys().cloned().collect::<Vec<_>>() {
                let s = db.resolve_ref(&id).ok();
                match next(s.as_ref(), limited.get(&id).copied(), now) {
                    Next::Wait => {
                        if let Some(entry) = m.get_mut(&id) {
                            dirty |= lift_hold(entry, s.as_ref());
                        }
                    }
                    Next::Deliver => due.push((id, key)),
                    // A first prompt of a session that stopped waits
                    // for its restart; only archived or gone drops it.
                    Next::Drop(_)
                        if key == FIRST_KEY
                            && s.as_ref().is_some_and(|s| s.archived_at.is_none()) => {}
                    Next::Drop(why) => {
                        m.remove(&id);
                        dirty = true;
                        if s.is_some() {
                            db.insert_event(
                                &id,
                                now,
                                dropped_kind(key),
                                &json!({"reason": why}).to_string(),
                            )?;
                        }
                    }
                }
            }
            if dirty {
                db.kv_set_map(key, &m)?;
            }
        }
    }
    for (id, key) in due {
        deliver(d, &id, key)?;
    }
    // A draft's age starts again once its session has left "your turn".
    {
        let db = d.db();
        crate::stash::prune_drafts(|id| {
            db.session_by_id(id)
                .ok()
                .flatten()
                .is_some_and(|s| s.status == Status::Idle)
        });
    }
    crate::stash::tick(d)
}

/// The log entry for a pending text that is dropped.
fn dropped_kind(key: &str) -> &'static str {
    if key == FIRST_KEY {
        "first_prompt_dropped"
    } else {
        "message_dropped"
    }
}

/// Deliver session `id`'s pending message if it is still due: looked at
/// again under the session's lock, right before typing.
fn deliver(d: &Daemon, id: &str, key: &str) -> Result<()> {
    let _session = d.sessions.lock(id);
    let limited = crate::limits::limited(d).get(id).copied();
    let (s, text, parts) = {
        let db = d.db();
        let Ok(s) = db.resolve_ref(id) else {
            return Ok(()); // (gone: the next tick drops it)
        };
        let Some(entry) = db.kv_map(key)?.get(id).cloned() else {
            return Ok(()); // (sent directly meanwhile)
        };
        let Some(text) = entry["text"].as_str().map(str::to_string) else {
            return Ok(());
        };
        // A Stop hook's answer carries them right now: its ack decides.
        if claimed_now(&entry) {
            return Ok(());
        }
        (s, text, parts_of(&entry))
    };
    if next(Some(&s), limited, now_ms()) != Next::Deliver {
        return Ok(());
    }
    // An OpenCode session's messages wait here only when its server
    // refused them: tried there again first.
    // (Its first prompt first: then the paste path holds it.)
    if key == PENDING_KEY && s.agent == Agent::OpenCode.name() && !first_waiting(&d.db(), &s.id)? {
        match post(d, &s, &text) {
            None | Some(Posted::Refused(_)) => {}
            Some(p) => {
                let db = d.db();
                take(&db, id, &parts)?;
                let unknown = match p {
                    Posted::Unknown(why) => Some(why),
                    _ => None,
                };
                db.insert_event(
                    id,
                    now_ms(),
                    "message_delivered",
                    &json!({"from": "overlord", "chars": text.chars().count(),
                            "via": "server", "unknown": unknown})
                    .to_string(),
                )?;
                drop(db);
                d.changed();
                return Ok(());
            }
        }
    }
    // Without the DB lock: tmux.
    // (A first prompt is the user's own words: its prompt is theirs.)
    let source = (key != FIRST_KEY).then_some(Source::Send);
    let r = type_message(d, &s, &text, source);
    if let Ok(Typed::NotNow(why)) = &r {
        // A later tick; said once, so a stuck queue is visible.
        return held(d, id, &text, why, key);
    }
    let db = d.db();
    let mut m = db.kv_map(key)?;
    // A first prompt that failed to type: tried again at a later tick, up
    // to `FIRST_TRIES` times.
    if let (Err(e), true) = (&r, key == FIRST_KEY) {
        if let Some(entry) = m.get_mut(id).and_then(Value::as_object_mut) {
            let tries = entry.get("tries").and_then(Value::as_u64).unwrap_or(0) + 1;
            if tries < FIRST_TRIES {
                entry.insert("tries".into(), json!(tries));
                db.kv_set_map(key, &m)?;
                tracing::info!(session = %id, error = %format!("{e:#}"), tries, "typing a first prompt");
                return Ok(());
            }
        }
    }
    // The messages it carried leave the queue; any added meanwhile (a Stop
    // hook's claim, no session lock) stay.
    if key == PENDING_KEY {
        take(&db, id, &parts)?;
    } else if m.get(id).and_then(|v| v["text"].as_str()) == Some(text.as_str()) {
        m.remove(id);
        db.kv_set_map(key, &m)?;
    }
    // A typed first prompt: what rlord types next waits for its turn to
    // show as started.
    if r.is_ok() && key == FIRST_KEY {
        crate::first_prompt::mark_sent(&db, id, now_ms())?;
    }
    let (kind, detail) = match &r {
        Ok(_) if key == FIRST_KEY => (
            "first_prompt",
            json!({"sent": true, "via": "typed", "chars": text.chars().count()}),
        ),
        Ok(_) => (
            "message_delivered",
            json!({"from": "overlord", "chars": text.chars().count()}),
        ),
        Err(e) => (dropped_kind(key), json!({"reason": format!("{e:#}")})),
    };
    db.insert_event(id, now_ms(), kind, &detail.to_string())?;
    drop(db);
    d.changed();
    Ok(())
}

/// Log, once per queued message, why it wasn't delivered yet (its entry
/// keeps the latest reason in `held`, for the row; a newer message
/// starts unmarked).
fn held(d: &Daemon, id: &str, text: &str, why: &Why, key: &str) -> Result<()> {
    let (why, draft) = (why.text.as_str(), why.draft);
    let db = d.db();
    let mut m = db.kv_map(key)?;
    let Some(entry) = m.get_mut(id).and_then(Value::as_object_mut) else {
        return Ok(());
    };
    if entry.get("text").and_then(Value::as_str) != Some(text)
        || (entry.get("held").and_then(Value::as_str) == Some(why)
            && entry.get("held_draft").and_then(Value::as_bool) == Some(draft))
    {
        return Ok(());
    }
    // The latest reason is kept for the row (`annotate`); logged once.
    let first = !entry.contains_key("held");
    entry.insert("held".into(), json!(why));
    entry.insert("held_draft".into(), json!(draft));
    if !first {
        db.kv_set_map(key, &m)?;
        drop(db);
        d.changed();
        return Ok(());
    }
    db.kv_set_map(key, &m)?;
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

    /// A resume isn't typed into OpenCode while it counts down its own
    /// retry; other messages and agents aren't held by it.
    #[test]
    fn a_resume_waits_for_opencodes_retry() {
        let checks = |agent, source| Checks {
            agent,
            settle: true,
            attached: None,
            overlay: None,
            draft: true,
            source,
        };
        let retrying = "  ┃\n  ╹▀▀▀▀\n  ⠋ Rate limited [retrying in 30s attempt #2]";
        let idle = "  ┃\n  ╹▀▀▀▀\n  ctrl+p commands";
        let resume = checks(Agent::OpenCode, Some(Source::Resume));
        assert!(resume_held(&resume, retrying).is_some());
        assert!(resume_held(&resume, idle).is_none());
        assert!(resume_held(&checks(Agent::OpenCode, Some(Source::Send)), retrying).is_none());
        assert!(resume_held(&checks(Agent::Claude, Some(Source::Resume)), retrying).is_none());
    }

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
            source: Some(Source::Send),
        };
        let d = Daemon::new(
            Db::open_in_memory().unwrap(),
            rlord_core::config::Config::default(),
        );
        let r = type_into(&d, id, "%no-such-pane", "hi", &checks).unwrap();
        assert!(matches!(r, Typed::NotNow(why) if why.text.contains("moments ago")));
    }

    #[test]
    fn someone_typing_in_an_attached_terminal() {
        let c = |session: &str, activity: i64| tmux::Client {
            name: "/dev/ttys001".into(),
            session: session.into(),
            activity,
            focused: true,
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

    fn msg(text: &str) -> Value {
        json!({"text": text, "context": null})
    }

    fn pending_text(db: &Db, id: &str) -> String {
        db.kv_map(PENDING_KEY).unwrap()[id]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A queued message held back is logged once, with why; a newer one
    /// is logged again.
    #[test]
    fn a_held_message_is_logged_once() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        queue(&db, &id, &msg("first")).unwrap();
        let first = pending_text(&db, &id);
        let d = Daemon::new(db, rlord_core::config::Config::default());
        let held_count = || {
            let log = d.db().list_events(&id, 20).unwrap();
            log.iter().filter(|e| e.kind == "message_held").count()
        };
        held(&d, &id, &first, &"a dialog is up".into(), PENDING_KEY).unwrap();
        held(&d, &id, &first, &"a dialog is up".into(), PENDING_KEY).unwrap();
        assert_eq!(held_count(), 1);
        // (Not this message's text: nothing.)
        held(&d, &id, "other", &"x".into(), PENDING_KEY).unwrap();
        assert_eq!(held_count(), 1);
        queue(&d.db(), &id, &msg("second")).unwrap();
        let both = pending_text(&d.db(), &id);
        held(&d, &id, &both, &Why::draft("a draft"), PENDING_KEY).unwrap();
        assert_eq!(held_count(), 2);
        let log = d.db().list_events(&id, 20).unwrap();
        let e = log.iter().find(|e| e.kind == "message_held").unwrap();
        assert!(e.payload["reason"].as_str().is_some(), "{}", e.payload);
    }

    /// The row's `✉ draft` comes from the hold's kind, not its wording;
    /// a hold lifts once the session leaves "your turn".
    #[test]
    fn a_hold_is_shown_by_kind_and_lifts() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        queue(&db, &id, &msg("first")).unwrap();
        let first = pending_text(&db, &id);
        let d = Daemon::new(db, rlord_core::config::Config::default());
        let row = || {
            let mut rows = vec![json!({"id": id})];
            annotate(&d, &mut rows);
            rows[0]["mail"].clone()
        };
        assert_eq!(row(), json!({"held": null, "draft": false}));
        // Worded like a draft, but not one.
        held(&d, &id, &first, &"(a draft) lookalike".into(), PENDING_KEY).unwrap();
        assert_eq!(row()["draft"], false);
        held(&d, &id, &first, &Why::draft("unsent text"), PENDING_KEY).unwrap();
        assert_eq!(row()["draft"], true);
        // Still "your turn": the hold stays.
        let mut m = d.db().kv_map(PENDING_KEY).unwrap();
        let s = d.db().resolve_ref(&id).unwrap();
        let mut idle = s.clone();
        idle.status = Status::Idle;
        assert!(!lift_hold(m.get_mut(&id).unwrap(), Some(&idle)));
        let mut busy = s;
        busy.status = Status::Working;
        assert!(lift_hold(m.get_mut(&id).unwrap(), Some(&busy)));
        assert!(m[&id].get("held").is_none() && m[&id].get("held_draft").is_none());
    }

    /// A failed restore's record doesn't block a new stash; a live one
    /// does.
    #[test]
    fn only_a_live_stash_record_blocks() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        let d = Daemon::new(db, rlord_core::config::Config::default());
        assert!(!crate::stash::pending(&d, &id));
        crate::stash::stashed(&d, &id, "half a thought").unwrap();
        assert!(crate::stash::pending(&d, &id));
        let mut m = d.db().kv_map(crate::stash::STASH_KEY).unwrap();
        m.get_mut(&id).unwrap()["failed"] = json!(true);
        d.db().kv_set_map(crate::stash::STASH_KEY, &m).unwrap();
        assert!(!crate::stash::pending(&d, &id));
    }

    #[test]
    fn a_first_prompt_has_its_own_slot() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        queue_first(&db, &id, "the task").unwrap();
        queue(&db, &id, &msg("a note")).unwrap();
        // The overlord's messages neither replace nor drop it.
        queue(&db, &id, &msg("a newer note")).unwrap();
        let parts = pending_parts(&db, &id).unwrap();
        take(&db, &id, &parts).unwrap();
        assert_eq!(db.kv_map(FIRST_KEY).unwrap()[&id]["text"], "the task");
        assert_eq!(dropped_kind(FIRST_KEY), "first_prompt_dropped");
    }

    /// Messages framed by code: the user's words, then the overlord's
    /// background, labelled; several waiting ones as one.
    #[test]
    fn messages_are_framed_and_add_up() {
        let one = framed(&[json!({"text": "fix the footer", "context": "web/footer.tsx"})]);
        assert_eq!(
            one,
            crate::context::frame_message("fix the footer", Some("web/footer.tsx"))
        );
        let two = framed(&[
            json!({"text": "fix the footer", "context": "web/footer.tsx"}),
            msg("and the header"),
        ]);
        // Each message keeps its own background; one header.
        assert!(
            two.contains("web/footer.tsx\n\n---\n\nand the header"),
            "{two}"
        );
        assert!(two.ends_with("and the header"), "{two}");
        assert_eq!(two.matches("[rlord]").count(), 1, "{two}");
        assert!(!framed(&[msg("go")]).contains("Background"));
    }

    /// A message sent directly takes the waiting ones with it; one added
    /// meanwhile stays.
    #[test]
    fn a_direct_send_takes_the_pending_messages() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        take(&db, &id, &[msg("x")]).unwrap();
        queue(&db, &id, &msg("older")).unwrap();
        let read = pending_parts(&db, &id).unwrap();
        queue(&db, &id, &msg("meanwhile")).unwrap();
        take(&db, &id, &read).unwrap();
        assert_eq!(pending_parts(&db, &id).unwrap(), vec![msg("meanwhile")]);
        assert_eq!(pending_text(&db, &id), framed(&[msg("meanwhile")]));
        // Not what is at the head of the queue any more: left alone.
        take(&db, &id, &read).unwrap();
        assert_eq!(pending_parts(&db, &id).unwrap().len(), 1);
        take(&db, &id, &[msg("meanwhile")]).unwrap();
        assert!(db.kv_map(PENDING_KEY).unwrap().is_empty());
    }

    #[test]
    fn pending_messages_accumulate() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        assert_eq!(queue(&db, &id, &msg("first")).unwrap(), 0);
        assert_eq!(queue(&db, &id, &msg("second")).unwrap(), 1);
        let m = db.kv_map(PENDING_KEY).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(pending_parts(&db, &id).unwrap().len(), 2);
        assert_eq!(m[&id]["text"], framed(&[msg("first"), msg("second")]));
        let log = db.list_events(&id, 10).unwrap();
        assert_eq!(log.iter().filter(|e| e.kind == "message_queued").count(), 2);
        // Too long together: refused, the waiting ones kept.
        let big = "x".repeat(MAX_CHARS - 10);
        assert!(queue(&db, &id, &msg(&big)).is_err());
        assert_eq!(pending_parts(&db, &id).unwrap().len(), 2);
        db.kv_set_map(PENDING_KEY, &serde_json::Map::new()).unwrap();
        assert!(db.kv_get(PENDING_KEY).unwrap().is_none());
    }

    /// An entry from before `parts` (text only) is one message: taking
    /// it clears it, so it isn't typed again and again.
    #[test]
    fn an_entry_without_parts_is_one_message() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        let mut m = serde_json::Map::new();
        m.insert(id.clone(), json!({"text": "old one", "at": 1}));
        db.kv_set_map(PENDING_KEY, &m).unwrap();
        let parts = pending_parts(&db, &id).unwrap();
        assert_eq!(parts, vec![json!({"text": "old one", "context": null})]);
        take(&db, &id, &parts).unwrap();
        assert!(db.kv_map(PENDING_KEY).unwrap().is_empty());
        // A newer message joins it instead of losing it.
        db.kv_set_map(PENDING_KEY, &m).unwrap();
        assert_eq!(queue(&db, &id, &msg("new")).unwrap(), 1);
        assert_eq!(pending_parts(&db, &id).unwrap().len(), 2);
    }

    #[test]
    fn the_users_words_are_cleaned_but_keep_tabs() {
        assert_eq!(
            clean_words(" a\tb\u{1b}[31m\u{202E}c\u{200B}\r\nd "),
            "a\tb[31mc\nd"
        );
    }

    /// A Stop hook claims what waits: `decision: block` with the framed
    /// text; it leaves the queue only on the ack; at most `CLAIMS_MAX` in
    /// a row, and a prompt starts the count again.
    #[test]
    fn a_stop_hook_claims_waiting_messages() {
        let db = Db::open_in_memory().unwrap();
        let id = insert(&db);
        db.set_status(&id, Status::Working, 1).unwrap();
        let s = db.resolve_ref(&id).unwrap();
        // Nothing waits: no answer.
        assert!(stop_claim(&db, &s, Agent::Codex, 1).unwrap().is_none());
        queue(&db, &id, &msg("also the footer")).unwrap();
        // OpenCode has no Stop hook. Codex (in the pane) claims as Claude
        // does; its claim is dropped again (no ack) for the Claude run.
        assert!(stop_claim(&db, &s, Agent::OpenCode, 1).unwrap().is_none());
        if CODEX_STOP_CLAIM {
            let c = stop_claim(&db, &s, Agent::Codex, 1).unwrap().unwrap();
            assert_eq!(c.output["decision"], "block");
            assert_eq!(c.output["reason"], framed(&[msg("also the footer")]));
            assert!(c.ack.starts_with(CLAIM_PREFIX), "{}", c.ack);
            db.kv_delete(&claims_key(&id)).unwrap();
            // On Codex's daemon: not claimed (not checked live there).
            let spec = json!({"codex_host": rlord_core::agents::codex::HOST_DAEMON});
            db.set_launch_spec(&id, &spec.to_string()).unwrap();
            assert!(stop_claim(&db, &s, Agent::Codex, 1).unwrap().is_none());
            db.set_launch_spec(&id, "{}").unwrap();
        }
        let r = stop_claim(&db, &s, Agent::Claude, 2).unwrap().unwrap();
        assert_eq!(r.output["decision"], "block");
        assert_eq!(r.output["reason"], framed(&[msg("also the footer")]));
        assert!(r.ack.starts_with(CLAIM_PREFIX), "{}", r.ack);
        // Unacknowledged: still waiting; the next Stop carries it again.
        let r2 = stop_claim(&db, &s, Agent::Claude, 3).unwrap().unwrap();
        // The older answer's ack no longer counts.
        assert!(!claim_ack(&db, &r.ack, 4).unwrap());
        assert_eq!(pending_parts(&db, &id).unwrap().len(), 1);
        // One more came while the answer was out: it stays.
        queue(&db, &id, &msg("and the header")).unwrap();
        // (The Stop set it idle: the ack carries it on, and logs that.)
        db.set_status(&id, Status::Idle, 4).unwrap();
        assert!(claim_ack(&db, &r2.ack, 5).unwrap());
        assert_eq!(
            pending_parts(&db, &id).unwrap(),
            vec![msg("and the header")]
        );
        let log = db.list_events(&id, 20).unwrap();
        let e = log.iter().find(|e| e.kind == "message_delivered").unwrap();
        assert_eq!(e.payload["via"], "stop_hook", "{}", e.payload);
        // Only acknowledged claims count: one so far. An unacked one
        // doesn't.
        let unacked = stop_claim(&db, &s, Agent::Claude, 6).unwrap().unwrap();
        assert!(!unacked.ack.is_empty());
        for at in [61, 62] {
            queue(&db, &id, &msg("more")).unwrap();
            let r = stop_claim(&db, &s, Agent::Claude, at).unwrap().unwrap();
            assert!(claim_ack(&db, &r.ack, at).unwrap());
        }
        // Three delivered in a row: the next Stop lets the turn end.
        queue(&db, &id, &msg("later")).unwrap();
        assert!(stop_claim(&db, &s, Agent::Claude, 7).unwrap().is_none());
        // (That cleared the count.) A prompt clears it too.
        assert!(stop_claim(&db, &s, Agent::Claude, 8).unwrap().is_some());
        claims_reset(&db, &id).unwrap();
        assert!(db.kv_get(&claims_key(&id)).unwrap().is_none());
        // Stopped, or on a usage limit: not claimed.
        let mut stopped = s.clone();
        stopped.status = Status::Stopped;
        assert!(stop_claim(&db, &stopped, Agent::Claude, 9)
            .unwrap()
            .is_none());
        let limits = json!({ &id: {"resets_at": now_ms() + 60_000} });
        db.kv_set(crate::limits::LIMITS_KEY, &limits.to_string())
            .unwrap();
        assert!(stop_claim(&db, &s, Agent::Claude, now_ms())
            .unwrap()
            .is_none());
        db.kv_delete(crate::limits::LIMITS_KEY).unwrap();
        // Its first prompt hasn't gone in: not claimed.
        queue_first(&db, &id, "the task").unwrap();
        assert!(stop_claim(&db, &s, Agent::Claude, 9).unwrap().is_none());
        // An ack that isn't a claim's: nothing.
        assert!(!claim_ack(&db, "send:nope", 10).unwrap());
        assert!(!claim_ack(&db, "s1.abc", 10).unwrap());
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

        // At its prompt again (the placeholder is dim): typed, framed,
        // with the queued one before it.
        let placeholder = composer("\\033[2mAsk Codex to do anything\\033[0m");
        tmux::respawn_pane("rlord_s1", "/tmp", &placeholder).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let v = send(&d, &json!({"ref": &id, "text": "two"})).unwrap();
        assert_eq!(v["sent"], true, "{v}");
        assert_eq!(v["with_pending"], 1, "{v}");
        assert!(d.db().kv_map(PENDING_KEY).unwrap().is_empty());
        std::thread::sleep(Duration::from_millis(200));
        assert!(screen().contains("two"), "{}", screen());
        // The prompt to come is marked as rlord's (durable); the agent's
        // report of it is then not the user's.
        let marked = d.db().kv_get(own::KEY).unwrap().unwrap_or_default();
        let two = rlord_core::types::PromptMark::hash_of(&framed(&[msg("one"), msg("two")]));
        assert!(marked.contains(&two), "{marked}");
        let (kind, source) = own::prompt(&d.db(), &id, None, now_ms(), own::Via::Hook).unwrap();
        assert_eq!((kind, source.as_deref()), ("rlord_prompt", Some("send")));
        let s = d.db().resolve_ref(&id).unwrap();
        assert!(s.last_user_msg_at.is_none() && s.last_rlord_msg_at.is_some());

        // Right after: one paste per settle.
        let e = send(&d, &json!({"ref": &id, "text": "three"})).unwrap_err();
        assert!(e.to_string().contains("moments ago"), "{e}");
        let q = send(&d, &json!({"ref": &id, "text": "three", "when_idle": true})).unwrap();
        assert_eq!(q["queued"], true);

        // The user's own text (`overlord ask`: no source) is typed the
        // same way but not marked: its prompt is the user's, and a resume
        // job sees them take the session over.
        let checks = Checks {
            agent: Agent::Codex,
            settle: false,
            attached: None,
            overlay: None,
            draft: false,
            source: None,
        };
        let pane = tmux::pane_id("rlord_s1").unwrap();
        let asked = "what is left to do";
        let before = d.db().kv_get(own::KEY).unwrap();
        let r = type_into(&d, &id, &pane, asked, &checks).unwrap();
        assert_eq!(r, Typed::Done);
        assert_eq!(d.db().kv_get(own::KEY).unwrap(), before);
        let text = rlord_core::types::PromptMark::of(asked);
        let (kind, source) =
            own::prompt(&d.db(), &id, Some(&text), now_ms(), own::Via::Hook).unwrap();
        assert_eq!((kind, source), ("prompt_submitted", None));
        assert!(d.db().resolve_ref(&id).unwrap().last_user_msg_at.is_some());

        tmux::kill_session("rlord_s1").unwrap();
        tx(&["kill-server"]);
    }
}
