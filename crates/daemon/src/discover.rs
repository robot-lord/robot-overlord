//! Sessions whose agent isn't reporting through hooks: learn what the hooks
//! would have said from the agent's own files. Plain code, no AI.
//!
//! Codex assigns its conversation id itself and runs a hook only once the
//! user has trusted it (`/hooks`). Until then, a Codex session's
//! conversation is found in Codex's thread index (the thread started in the
//! session's folder since it was launched), and its status is read from the
//! end of its transcript (a turn started / completed) and its pane (the
//! program exited). Once a hook has confirmed the session
//! (`agent_session_trust = hook`) it is left to the hooks.
//!
//! One slow tick over the few sessions this applies to; a transcript is
//! read incrementally (only what was appended since the last look). All
//! file and tmux reads happen without the DB lock.
//!
//! Binding is deliberately cautious: only a thread started from the CLI in
//! the session's folder since its launch, whose first prompt is the one the
//! session was launched with (when it had one), and only when no other
//! session is waiting for a thread in the same folder. Otherwise the hooks
//! (or the user) decide.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use rlord_core::agents::{codex, transcript, Agent};
use rlord_core::db::Db;
use rlord_core::tmux;
use rlord_core::types::{now_ms, Session, Status};

use crate::{methods, Daemon};

const TICK: Duration = Duration::from_secs(3);

/// Start the watcher.
pub fn start(d: &Arc<Daemon>) {
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        // How many ticks in a row each session's pane has looked gone.
        let mut seen: HashMap<String, u8> = HashMap::new();
        loop {
            std::thread::sleep(TICK);
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pass(&d, &mut seen)));
            match r {
                Ok(Err(e)) => tracing::debug!(error = %e, "following sessions without hooks"),
                Err(_) => tracing::warn!("following sessions without hooks panicked"),
                Ok(Ok(())) => {}
            }
        }
    });
}

/// Live sessions of agents that assign their own id, not confirmed by a
/// hook; and confirmed ones that are still "starting" (a resumed Codex runs
/// no hook until its next prompt: see `follow`).
fn unhooked(d: &Daemon) -> Result<Vec<Session>> {
    Ok(d.db()
        .running_sessions()?
        .into_iter()
        .filter(|s| Agent::parse(&s.agent).is_some_and(|a| !a.mints_id()))
        .filter(|s| {
            s.agent_session.is_none()
                || s.agent_session_trust != "hook"
                || s.status == Status::Starting
        })
        .collect())
}

/// Live sessions on Codex's daemon that the hooks follow (not `unhooked`):
/// only whether their pane is gone is watched here.
fn on_daemon(d: &Daemon, unhooked: &[Session]) -> Result<Vec<Session>> {
    Ok(d.db()
        .running_sessions()?
        .into_iter()
        .filter(|s| s.codex_daemon && !unhooked.iter().any(|u| u.id == s.id))
        .collect())
}

fn pass(d: &Daemon, gone: &mut HashMap<String, u8>) -> Result<()> {
    // (Whatever changed the sessions on Codex's daemon, the hook gate
    // catches up here: a write only when it differs.)
    crate::codex_daemon::sync_gate(&d.db());
    let sessions = unhooked(d)?;
    let daemon = on_daemon(d, &sessions)?;
    gone.retain(|id, _| sessions.iter().chain(&daemon).any(|s| &s.id == id));
    let mut changed = false;
    for s in &daemon {
        if pane_looks(s, gone) >= 2 {
            changed |= pane_gone(d, s, gone)?;
        }
    }
    let waiting = waiters(d, &sessions)?;
    let now = now_ms();
    for s in &sessions {
        // Its program exited (no SessionEnd hook to say so).
        match pane_looks(s, gone) {
            0 => {}
            1 => continue,
            _ => {
                changed |= pane_gone(d, s, gone)?;
                continue;
            }
        }
        if s.agent_session.is_none() {
            let (others, queue) = place_of(&waiting, s, now);
            changed |= bind(d, s, others, queue)? || first_prompt_due(d, s)?;
        } else {
            changed |= follow(d, s)?;
        }
    }
    if changed {
        d.changed();
    }
    Ok(())
}

/// How many looks in a row session `s`'s pane has looked gone (its tmux
/// session missing, or its program exited), counted in `gone`. One failed
/// look isn't proof (tmux busy, a restart in between): act on two.
pub(crate) fn pane_looks(s: &Session, gone: &mut HashMap<String, u8>) -> u8 {
    let dead = s
        .tmux_session
        .as_deref()
        .is_some_and(|t| !tmux::has_session(t) || tmux::format(t, "#{pane_dead}") == "1");
    let looks = gone.entry(s.id.clone()).or_default();
    *looks = if dead { looks.saturating_add(1) } else { 0 };
    *looks
}

/// Session `s`'s pane is gone (looked so twice). In the pane, its agent
/// ended with it: stopped. On Codex's daemon the turn goes on without the
/// pane: stopped only once no turn runs there (asked again every few
/// passes while one does, logged once as `pane_gone_turn_running`).
fn pane_gone(d: &Daemon, s: &Session, gone: &mut HashMap<String, u8>) -> Result<bool> {
    if !s.codex_daemon {
        return stop_from_pane(&d.db(), s);
    }
    let looks = gone.get(&s.id).copied().unwrap_or(2);
    // (Every fifth look after the second: the daemon is asked over a
    // fresh connection each time.)
    if (looks - 2) % 5 != 0 {
        if looks > 200 {
            gone.insert(s.id.clone(), 2);
        }
        return Ok(false);
    }
    let transcript = d.db().transcript_source(&s.id)?;
    // (Asked without the DB lock.)
    let running = crate::codex_daemon::turn_running(s, transcript.as_deref());
    if running != Some(true) {
        return stop_from_pane(&d.db(), s);
    }
    let db = d.db();
    let key = format!("pane_gone_turn:{}", s.id);
    if db.kv_get(&key)?.is_none() {
        db.kv_set(&key, &now_ms().to_string())?;
        db.insert_event(&s.id, now_ms(), "pane_gone_turn_running", "{}")?;
        tracing::info!(session = %s.id, "pane gone, its turn still runs on Codex's server");
        return Ok(true);
    }
    Ok(false)
}

/// Session `s` is stopped: its pane ended. Not when its status moved since
/// `s` was read (restarted meanwhile: not this pane's end). Returns
/// whether it was written.
pub(crate) fn stop_from_pane(db: &Db, s: &Session) -> Result<bool> {
    // Gone (deleted meanwhile) or moved on: leave it to the next pass.
    match db.resolve_ref(&s.id) {
        Ok(cur) if cur.status == s.status => {}
        _ => return Ok(false),
    }
    let now = now_ms();
    db.set_status(&s.id, Status::Stopped, now)?;
    db.insert_event(&s.id, now, "session_end", r#"{"from":"pane"}"#)?;
    db.kv_delete(&format!("pane_gone_turn:{}", s.id))?;
    // (Its thread leaves the gate: a stopped session's hooks are dropped.)
    crate::codex_daemon::sync_gate(db);
    Ok(true)
}

/// How long after a launch without a prompt the agent's screen is taken
/// to be up.
const UP_AFTER_MS: i64 = 4_000;

/// A session launched without a prompt has nothing to report until its
/// first one: Codex creates the conversation (and runs SessionStart) only
/// then. So once its pane has been up a moment it is the user's turn, not
/// "starting" for ever. (The overlord starts this way: `ask` waits for it
/// to leave "starting" before typing into it.) Returns whether it changed.
fn first_prompt_due(d: &Daemon, s: &Session) -> Result<bool> {
    if s.status != Status::Starting {
        return Ok(false);
    }
    let (launched_bare, since) = {
        let db = d.db();
        let launched_bare = db
            .kv_get(&methods::bind_prompt_key(&s.id))?
            .is_some_and(|p| p.trim().is_empty());
        let since = db
            .kv_get(&methods::bind_since_key(&s.id))?
            .and_then(|v| v.parse::<i64>().ok());
        (launched_bare, since)
    };
    let now = now_ms();
    if !launched_bare || !since.is_some_and(|t| up_long_enough(t, now)) {
        return Ok(false);
    }
    // Up means its composer is on screen, not a startup dialog in front of
    // it (where typed text would land on the wrong thing). Read without
    // the DB lock.
    let Some(composer_seen) = screen_settles(s, since, now) else {
        return Ok(false);
    };
    let db = d.db();
    // Moved on meanwhile (a hook, a restart): not ours to say.
    let cur = db.resolve_ref(&s.id)?;
    if cur.status != Status::Starting || cur.agent_session.is_some() {
        return Ok(false);
    }
    db.set_status(&s.id, Status::Idle, now)?;
    db.insert_event(
        &s.id,
        now,
        "awaiting_first_prompt",
        &json!({"from": "pane", "composer_seen": composer_seen}).to_string(),
    )?;
    Ok(true)
}

fn up_long_enough(launched_ms: i64, now_ms: i64) -> bool {
    now_ms - launched_ms >= UP_AFTER_MS
}

/// How long a starting Codex session waits for its composer to be
/// recognised before it settles on any screen without a dialog.
const SETTLE_ANYWAY_MS: i64 = 20_000;

/// Whether a starting session (up since `since`, epoch ms) may settle,
/// from its agent pane: Some(true) when Codex's composer is shown
/// (`codex::composer_shown`); Some(false) once it has been up
/// `SETTLE_ANYWAY_MS` with no numbered-choice dialog on screen (a composer
/// drawn in a way `composer_shown` doesn't know must not leave it starting
/// for ever; logged); None: not yet.
fn screen_settles(s: &Session, since: Option<i64>, now: i64) -> Option<bool> {
    let screen = s
        .tmux_session
        .as_deref()
        .and_then(|t| tmux::capture_pane(t).ok())?;
    let r = settles(&screen, since.map(|t| now - t));
    if r == Some(false) {
        tracing::info!(
            session = %s.name,
            "no Codex composer seen after {}s, and no dialog: settling anyway",
            SETTLE_ANYWAY_MS / 1000
        );
    }
    r
}

/// `screen_settles` for a screen up for `up_ms`.
fn settles(screen: &str, up_ms: Option<i64>) -> Option<bool> {
    if codex::composer_shown(screen) {
        return Some(true);
    }
    let long = up_ms.is_some_and(|ms| ms >= SETTLE_ANYWAY_MS);
    (long && !codex::dialog_shown(screen)).then_some(false)
}

/// Find the conversation a session launched, in its agent's index (see
/// `pick_thread`). `others`: another session waits for one in this folder.
/// `queue`: for a user fork, its place among the forks of the same parent
/// waiting in the same folder (`place_of`).
fn bind(d: &Daemon, s: &Session, others: bool, queue: Option<Queue>) -> Result<bool> {
    let w = bind_inputs(&d.db(), s)?;
    // (Codex's index and the rollouts are read without the DB lock.)
    let candidates = untracked_threads(s, w.since, &w.tracked);
    let fork = match (&w.fork, queue) {
        (Some(parent), Some(q)) => Some(ForkQueue {
            parent,
            since: w.since,
            place: q.place,
            waiting: q.waiting,
        }),
        (Some(parent), None) => Some(ForkQueue {
            parent,
            since: w.since,
            place: 0,
            waiting: 1,
        }),
        (None, _) => None,
    };
    let Some(t) = pick_thread(
        &candidates,
        w.prompt.as_deref(),
        others,
        fork,
        &|t| rollout_fork(d, t),
        now_ms(),
    ) else {
        return Ok(false);
    };
    record_bind(&d.db(), s, t)
}

/// A session waiting for its thread: its folder, the thread it was forked
/// from (a user fork) and since when it waits.
#[derive(Debug, Clone)]
struct Waiter {
    id: String,
    folder: String,
    fork: Option<String>,
    since: i64,
}

/// Every session in `sessions` still waiting for its thread (one short
/// read of their keys).
fn waiters(d: &Daemon, sessions: &[Session]) -> Result<Vec<Waiter>> {
    let db = d.db();
    let mut out = Vec::new();
    for s in sessions.iter().filter(|s| s.agent_session.is_none()) {
        let w = bind_inputs(&db, s)?;
        out.push(Waiter {
            id: s.id.clone(),
            folder: s.project_path.clone(),
            fork: w.fork,
            since: w.since,
        });
    }
    Ok(out)
}

/// A fork's place in the queue of forks of the same parent waiting in the
/// same folder, oldest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Queue {
    place: usize,
    waiting: usize,
}

/// For session `s`, waiting: whether another session waits for a thread in
/// its folder (`others`, for a session that isn't a fork: a waiting fork
/// counts only for `UNREAD_GRACE_MS`, so forks never hold an ordinary
/// session unbound), and, for a fork, its `Queue` among its parent's forks
/// waiting there.
fn place_of(waiting: &[Waiter], s: &Session, now: i64) -> (bool, Option<Queue>) {
    let me = waiting.iter().find(|w| w.id == s.id);
    let here = |w: &&Waiter| w.id != s.id && w.folder == s.project_path;
    let others = waiting
        .iter()
        .filter(here)
        .any(|w| w.fork.is_none() || now - w.since < UNREAD_GRACE_MS);
    let queue = me.and_then(|me| {
        let parent = me.fork.as_deref()?;
        let mut peers: Vec<&Waiter> = waiting
            .iter()
            .filter(|w| w.folder == me.folder && w.fork.as_deref() == Some(parent))
            .collect();
        peers.sort_by(|a, b| (a.since, &a.id).cmp(&(b.since, &b.id)));
        let place = peers.iter().position(|w| w.id == me.id)?;
        Some(Queue {
            place,
            waiting: peers.len(),
        })
    });
    (others, queue)
}

/// A user fork looking for its thread: its parent's thread, since when it
/// waits, and its place among the forks of that parent waiting in its
/// folder (`Queue`).
#[derive(Debug, Clone, Copy)]
struct ForkQueue<'a> {
    parent: &'a str,
    since: i64,
    place: usize,
    waiting: usize,
}

/// `bind`, once, under the caller's DB lock: for a stopped overlord about
/// to be started again, whose thread may have turned up after rlord last
/// looked. Nobody else waits in the folder it is asked for. No rollout is
/// read under the lock: the overlord is never forked (refused) and works
/// in rlord's own folder, where no other session's fork runs; an archive
/// summary's fork is never indexed (`--ephemeral`).
pub(crate) fn bind_now(db: &Db, s: &Session) -> Result<bool> {
    let w = bind_inputs(db, s)?;
    if w.fork.is_some() {
        return Ok(false); // (a fork: left to `bind`, outside the lock)
    }
    let candidates = untracked_threads(s, w.since, &w.tracked);
    let not_forked = |_: &codex::Thread| Some(None);
    match pick_thread(
        &candidates,
        w.prompt.as_deref(),
        false,
        None,
        &not_forked,
        now_ms(),
    ) {
        Some(t) => record_bind(db, s, t),
        None => Ok(false),
    }
}

/// What the search for a session's thread goes by.
struct Wanted {
    /// Started since when.
    since: i64,
    /// With which first prompt (if any).
    prompt: Option<String>,
    /// A user fork: the thread it was forked from.
    fork: Option<String>,
    /// The threads already taken.
    tracked: std::collections::HashSet<String>,
}

fn bind_inputs(db: &Db, s: &Session) -> Result<Wanted> {
    let since = db
        .kv_get(&methods::bind_since_key(&s.id))?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(s.created_at);
    let prompt = db
        .kv_get(&methods::bind_prompt_key(&s.id))?
        .filter(|p| !p.trim().is_empty());
    let fork = db
        .kv_get(&methods::bind_fork_key(&s.id))?
        .filter(|p| !p.trim().is_empty());
    Ok(Wanted {
        since,
        prompt,
        fork,
        tracked: db.agent_session_ids()?,
    })
}

fn untracked_threads(
    s: &Session,
    since: i64,
    tracked: &std::collections::HashSet<String>,
) -> Vec<codex::Thread> {
    codex::threads_started_in(&codex::home(), &s.project_path, since, s.codex_daemon)
        .into_iter()
        .filter(|t| !tracked.contains(&t.id))
        .collect()
}

/// What each Codex thread's rollout says it was forked from, remembered
/// (`Daemon::fork_flags`): a complete first line never changes, so each is
/// read once. Bounded: past `FLAGS_MAX` the oldest answers go first.
#[derive(Default)]
pub struct ForkFlags {
    seen: HashMap<String, Option<String>>,
    order: std::collections::VecDeque<String>,
}

const FLAGS_MAX: usize = 4096;

impl ForkFlags {
    /// Thread `t`'s answer (`codex::rollout_forked_from` through `read`;
    /// None: can't be told yet, asked again next time, not remembered).
    fn of(
        &mut self,
        t: &codex::Thread,
        read: impl Fn(&std::path::Path) -> Option<Option<String>>,
    ) -> Option<Option<String>> {
        if let Some(known) = self.seen.get(&t.id) {
            return Some(known.clone());
        }
        let forked = read(std::path::Path::new(&t.rollout_path))?;
        while self.order.len() >= FLAGS_MAX {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        self.order.push_back(t.id.clone());
        self.seen.insert(t.id.clone(), forked.clone());
        Some(forked)
    }
}

/// What thread `t`'s rollout says it was forked from, through the
/// daemon's remembered answers (`ForkFlags`).
fn rollout_fork(d: &Daemon, t: &codex::Thread) -> Option<Option<String>> {
    let mut flags = d.fork_flags.lock().unwrap_or_else(|e| e.into_inner());
    flags.of(t, codex::rollout_forked_from)
}

fn record_bind(db: &Db, s: &Session, t: &codex::Thread) -> Result<bool> {
    // A hook may have bound it meanwhile.
    if db.resolve_ref(&s.id)?.agent_session.is_some() {
        return Ok(false);
    }
    // rlord's own attribution ("minted"), until a hook confirms it.
    db.set_agent_session(&s.id, &t.id, "minted")?;
    db.upsert_transcript_source(&s.id, &t.rollout_path)?;
    // A fork whose rollout is written: resumed at a restart, not forked again.
    crate::fork::conversation_seen(db, &s.id, &t.rollout_path)?;
    // Bound: what the search for its thread needed is done with.
    methods::clear_bind_keys(db, &s.id)?;
    db.insert_event(
        &s.id,
        now_ms(),
        "agent_session_bound",
        &json!({"to": t.id, "from": "codex thread index"}).to_string(),
    )?;
    tracing::info!(session = %s.id, thread = %t.id, "bound a Codex session from its thread index");
    // (On Codex's daemon: its hooks now pass the gate by thread.)
    crate::codex_daemon::sync_gate(db);
    Ok(true)
}

/// The thread a session launched, among the untracked CLI threads started
/// in its folder since (oldest first), or None when it can't be told:
///
/// - a user fork (`fork`: the thread it was forked from): the thread whose
///   rollout says it was forked from that one. Its first message is its
///   parent's, so the prompt can't tell it.
/// - otherwise, by the rules below; the one thread they narrow to is
///   taken only if its rollout says it is no fork (not a user fork of
///   another session in the same folder, nor an archive summary's `codex
///   exec fork`):
/// - launched with a prompt: the thread whose first message is that
///   prompt. One that has no first message yet may still become it: wait.
/// - no prompt (a fresh conversation, or launched without one): the only
///   candidate, and only when no other session is waiting in this folder.
/// - an older Codex whose index keeps no first message: as with no
///   prompt (the only candidate, nobody else waiting).
/// - two that fit equally: neither (the hooks will say).
///
/// `forked_from` reads a thread's rollout (`rollout_fork`): every
/// candidate's for a fork, else only the one left. A rollout that can't
/// be read yet (None): for a fork, that candidate is skipped; for any
/// other session, wait `UNREAD_GRACE_MS` after the thread was indexed,
/// then take it (as before forks: never unbound for ever).
fn pick_thread<'a>(
    candidates: &'a [codex::Thread],
    prompt: Option<&str>,
    others_waiting: bool,
    fork: Option<ForkQueue>,
    forked_from: &dyn Fn(&codex::Thread) -> Option<Option<String>>,
    now: i64,
) -> Option<&'a codex::Thread> {
    if let Some(q) = fork {
        // The threads whose rollout names the parent, oldest first (one
        // not readable yet is skipped: it can't be told apart yet).
        let mut found: Vec<&codex::Thread> = candidates
            .iter()
            .filter(|t| forked_from(t).flatten().as_deref() == Some(q.parent))
            .collect();
        found.sort_by_key(|t| t.created_at_ms);
        // The one made closest after this fork started.
        let mine = found.iter().find(|t| t.created_at_ms >= q.since).copied();
        let late = now - q.since >= UNREAD_GRACE_MS;
        return match (q.waiting, found.as_slice()) {
            // The only fork waiting: its one thread; two (an earlier
            // attempt's too) are told apart by time only once it waited.
            (0 | 1, [one]) => Some(*one),
            (0 | 1, _) => mine.filter(|_| late),
            // Several forks of the parent waiting here: oldest first, once
            // each has a thread (or it waited); a later one on its own only
            // once it waited (never unbound for ever).
            _ if q.place == 0 && (found.len() >= q.waiting || late) => mine,
            _ => mine.filter(|_| late),
        };
    }
    let same = |a: &str, b: &str| {
        let (a, b) = (a.trim(), b.trim());
        // (Codex may keep only the start of a long first message.)
        !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
    };
    // (The index keeps first messages, or it doesn't: all or none.)
    let messages = candidates.iter().any(|t| t.first_user_message.is_some());
    let fit: Vec<&codex::Thread> = match prompt.filter(|_| messages) {
        Some(p) => candidates
            .iter()
            .filter(|t| same(t.first_user_message.as_deref().unwrap_or(""), p))
            .collect(),
        None if others_waiting => Vec::new(),
        None => candidates.iter().collect(),
    };
    // Narrowed to one: not a fork (a user fork binds by `bind_fork`, never
    // here). A rollout not readable yet: wait a moment, then take it, as
    // before forks (an ordinary session must not stay unbound).
    match fit.as_slice() {
        [one] => match forked_from(one) {
            Some(None) => Some(*one),
            Some(Some(_)) => None,
            None => (now - one.created_at_ms >= UNREAD_GRACE_MS).then_some(*one),
        },
        _ => None,
    }
}

/// How long after a thread is indexed binding waits for its rollout's
/// first line, before taking it as no fork.
const UNREAD_GRACE_MS: i64 = 15_000;

/// Read a session's status from its transcript (incrementally).
///
/// A session the hooks have confirmed is theirs, except while it is still
/// "starting": Codex runs SessionStart for a resumed conversation only
/// with its next prompt, so a resume would stay "starting" until then (and
/// the overlord, which is typed into only once it has left "starting",
/// could not be asked anything). Once its pane has been up a moment, its
/// transcript says whether a turn is open.
fn follow(d: &Daemon, s: &Session) -> Result<bool> {
    let hooked = s.agent_session_trust == "hook";
    if hooked
        && (s.status != Status::Starting
            || !s
                .last_state_change_at
                .is_some_and(|t| up_long_enough(t, now_ms())))
    {
        return Ok(false);
    }
    let Some(src) = d.db().transcript_source(&s.id)? else {
        return Ok(false);
    };
    let path = Path::new(&src);
    // Still starting (just launched or resumed): only what the agent has
    // written since counts. A conversation killed mid-turn ends on a turn
    // that never closes; resumed, it waits for the user.
    let working = match (s.status, s.last_state_change_at) {
        (Status::Starting, Some(t)) => transcript::working_since(path, t),
        _ => transcript::working(path),
    };
    // Settling a start with nothing in the transcript: only once the
    // composer is up (not an update or trust prompt in front of it).
    if s.status == Status::Starting
        && working.is_none()
        && screen_settles(s, s.last_state_change_at, now_ms()).is_none()
    {
        return Ok(false);
    }
    let Some(next) = next_status(s.status, working) else {
        return Ok(false);
    };
    let db = d.db();
    methods::save_cursors(&db);
    // The hooks started reporting meanwhile: theirs stands.
    let cur = db.resolve_ref(&s.id)?;
    if (cur.agent_session_trust == "hook" && !hooked) || cur.status != s.status {
        return Ok(false);
    }
    let now = now_ms();
    db.set_status(&s.id, next, now)?;
    let (kind, source) = match next {
        // A turn began: the user's prompt, or one rlord typed (`own.rs`;
        // only that a turn is open was read, so by time).
        Status::Working => crate::own::prompt(&db, &s.id, None, now, crate::own::Via::Transcript)?,
        _ => {
            if s.status == Status::Working {
                db.touch_last_agent_msg(&s.id, now)?;
            }
            ("agent_stopped", None)
        }
    };
    let detail = crate::own::with_source(json!({"from": "transcript"}), source);
    db.insert_event(&s.id, now, kind, &detail.to_string())?;
    Ok(true)
}

/// The status a transcript implies, if it differs: `working` is whether
/// a turn is open. None (no turn line read: there was none yet, or it began
/// before what this run has read) only settles a session still starting;
/// it never calls a working session idle. A permission prompt can't be
/// seen this way, so "needs you" is left alone (only a hook sets it).
fn next_status(cur: Status, working: Option<bool>) -> Option<Status> {
    let want = match (working, cur) {
        (Some(true), _) => Status::Working,
        (Some(false), _) => Status::Idle,
        // Up, and nothing says it's in a turn.
        (None, Status::Starting) => Status::Idle,
        (None, _) => return None,
    };
    match cur {
        Status::Starting | Status::Working | Status::Idle if cur != want => Some(want),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_is_up_after_a_moment() {
        assert!(!up_long_enough(10_000, 10_000 + UP_AFTER_MS - 1));
        assert!(up_long_enough(10_000, 10_000 + UP_AFTER_MS));
    }

    #[test]
    fn a_start_settles_on_the_composer_or_after_a_while() {
        let composer = "› Ask Codex to do anything\n\n  ? for shortcuts\n";
        let dialog = "  Update available!\n\n› 1. Update now\n  2. Skip\n";
        let unknown = "  something Codex drew that rlord doesn't know\n";
        assert_eq!(settles(composer, Some(0)), Some(true));
        assert_eq!(settles(unknown, Some(SETTLE_ANYWAY_MS - 1)), None);
        assert_eq!(settles(unknown, None), None);
        // Long enough, and no dialog: settled anyway.
        assert_eq!(settles(unknown, Some(SETTLE_ANYWAY_MS)), Some(false));
        // A dialog never settles it.
        assert_eq!(settles(dialog, Some(10 * SETTLE_ANYWAY_MS)), None);
    }

    #[test]
    fn status_follows_the_open_turn() {
        use Status::*;
        assert_eq!(next_status(Starting, None), Some(Idle));
        assert_eq!(next_status(Starting, Some(true)), Some(Working));
        assert_eq!(next_status(Idle, Some(true)), Some(Working));
        assert_eq!(next_status(Working, Some(false)), Some(Idle));
        // Nothing to change.
        assert_eq!(next_status(Idle, Some(false)), None);
        assert_eq!(next_status(Working, Some(true)), None);
        assert_eq!(next_status(Idle, None), None);
        // Unknown never ends a turn that is running.
        assert_eq!(next_status(Working, None), None);
        // Not a state the transcript can speak to.
        assert_eq!(next_status(WaitingInput, Some(true)), None);
        assert_eq!(next_status(Stopped, Some(true)), None);
    }

    fn thread(id: &str, first: &str) -> codex::Thread {
        codex::Thread {
            id: id.into(),
            rollout_path: format!("/r/{id}.jsonl"),
            created_at_ms: 1,
            first_user_message: Some(first.into()),
        }
    }

    /// What the test's rollouts say: `f…` threads were forked from
    /// `parent`, `g…` from `other`, `u…` can't be read yet; and which
    /// rollouts were read.
    fn rollouts(
        read: &std::cell::RefCell<Vec<String>>,
        t: &codex::Thread,
    ) -> Option<Option<String>> {
        read.borrow_mut().push(t.id.clone());
        match t.id.chars().next() {
            Some('f') => Some(Some("parent".into())),
            Some('g') => Some(Some("other".into())),
            Some('u') => None,
            _ => Some(None),
        }
    }

    #[test]
    fn a_fork_binds_only_the_thread_forked_from_its_parent() {
        let read = std::cell::RefCell::new(Vec::new());
        // (The test's threads were indexed at 1: a moment ago.)
        let pick_at =
            |c: &[codex::Thread], prompt: Option<&str>, others: bool, fork: Option<&str>, now| {
                // (The only fork of its parent waiting, since 0.)
                let q = fork.map(|parent| ForkQueue {
                    parent,
                    since: 0,
                    place: 0,
                    waiting: 1,
                });
                pick_thread(c, prompt, others, q, &|t| rollouts(&read, t), now)
                    .map(|t| t.id.clone())
            };
        let pick = |c: &[codex::Thread], prompt: Option<&str>, others: bool, fork: Option<&str>| {
            pick_at(c, prompt, others, fork, 2)
        };
        // (A fork's first message is its parent's.)
        let mut c = vec![
            thread("own", "fix the login test"),
            thread("f1", "fix the login test"),
            thread("g1", "fix the login test"),
        ];
        // The fork: only its parent's fork, whatever its prompt, even
        // with a sibling waiting.
        assert_eq!(
            pick(&c, Some("try a queue"), true, Some("parent")).as_deref(),
            Some("f1")
        );
        assert_eq!(pick(&c, None, false, Some("parent")).as_deref(), Some("f1"));
        // A rollout not readable yet is skipped: the one that names the
        // parent binds; none readable that does: wait.
        let unread = [thread("f1", "x"), thread("u1", "x")];
        assert_eq!(
            pick(&unread, None, false, Some("parent")).as_deref(),
            Some("f1")
        );
        assert_eq!(
            pick(&[thread("u1", "x")], None, false, Some("parent")),
            None
        );
        // Two forks of the same parent: neither (the hooks will say).
        c.push(thread("f2", "fix the login test"));
        assert_eq!(pick(&c, None, false, Some("parent")), None);
        assert_eq!(pick(&c, None, false, Some("absent")), None);
        // An ordinary session never takes a forked thread: the one it is
        // narrowed to must say it is none.
        let only_forks = [thread("f1", "fix the login test")];
        assert_eq!(pick(&only_forks, None, false, None), None);
        assert_eq!(
            pick(&only_forks, Some("fix the login test"), false, None),
            None
        );
        // One whose rollout can't be read yet: wait a moment, then take
        // it (an ordinary session never stays unbound).
        let unread = [thread("u1", "fix it")];
        assert_eq!(pick(&unread, Some("fix it"), false, None), None);
        assert_eq!(
            pick_at(&unread, Some("fix it"), false, None, 1 + UNREAD_GRACE_MS).as_deref(),
            Some("u1")
        );
        // Its own (and a fork with another first message): taken, reading
        // only the rollout of the one left.
        let mixed = [thread("own", "fix it"), thread("f2", "speed it up")];
        read.borrow_mut().clear();
        assert_eq!(
            pick(&mixed, Some("fix it"), false, None).as_deref(),
            Some("own")
        );
        assert_eq!(*read.borrow(), ["own"]);
        // Two that fit: none, and nothing read.
        read.borrow_mut().clear();
        assert_eq!(pick(&c, Some("fix the login test"), false, None), None);
        assert_eq!(pick(&mixed, None, true, None), None);
        assert!(
            read.borrow().is_empty(),
            "nothing narrowed to one: nothing read"
        );
    }

    /// A rollout's fork flag is read once per thread: the file is not
    /// read again on later passes. One not readable yet (missing, a first
    /// line still being written) is asked again, not remembered.
    #[test]
    fn a_rollouts_fork_flag_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-once.jsonl");
        let mut t = thread("t-once-only", "x");
        t.rollout_path = path.to_string_lossy().into_owned();
        // Its own cache (each daemon has one): nothing shared between tests.
        let mut flags = ForkFlags::default();
        let reads = std::cell::Cell::new(0);
        let read = |p: &std::path::Path| {
            reads.set(reads.get() + 1);
            codex::rollout_forked_from(p)
        };
        assert_eq!(flags.of(&t, read), None, "not there yet");
        let meta = |from: &str| {
            format!(
                "{}\n",
                json!({"type": "session_meta", "payload": {"forked_from_id": from}})
            )
        };
        let line = meta("t-parent");
        std::fs::write(&path, &line[..line.len() - 5]).unwrap();
        assert_eq!(flags.of(&t, read), None, "half written");
        std::fs::write(&path, &line).unwrap();
        assert_eq!(flags.of(&t, read), Some(Some("t-parent".into())));
        // Changed on disk (it never is): the remembered answer stands, and
        // the file isn't read again.
        std::fs::write(&path, meta("t-other")).unwrap();
        let before = reads.get();
        assert_eq!(flags.of(&t, read), Some(Some("t-parent".into())));
        assert_eq!(reads.get(), before);
    }

    /// Past its bound the oldest answers go, not all of them.
    #[test]
    fn fork_flags_forget_the_oldest_first() {
        let mut flags = ForkFlags::default();
        let none = |_: &std::path::Path| Some(None);
        for i in 0..=FLAGS_MAX {
            flags.of(&thread(&format!("t{i}"), "x"), none);
        }
        assert_eq!(flags.seen.len(), FLAGS_MAX);
        assert!(!flags.seen.contains_key("t0"), "the oldest went");
        assert!(flags.seen.contains_key("t1") && flags.seen.contains_key(&format!("t{FLAGS_MAX}")));
    }

    /// Two forks of one parent waiting in one folder: the oldest binds the
    /// thread made closest after its own start, once each has one (or it
    /// waited); the next binds its own after that. A waiting fork holds an
    /// ordinary session back only for the grace period.
    #[test]
    fn forks_of_one_parent_bind_oldest_first() {
        let fork_thread = |id: &str, at: i64| codex::Thread {
            created_at_ms: at,
            ..thread(id, "same first message")
        };
        let forked = |_: &codex::Thread| Some(Some("parent".to_string()));
        let both = [fork_thread("tb", 2_500), fork_thread("ta", 1_500)];
        let q = |since, place, waiting| ForkQueue {
            parent: "parent",
            since,
            place,
            waiting,
        };
        let id = |t: Option<&codex::Thread>| t.map(|t| t.id.clone());
        // Fork A (started 1000, oldest) takes the thread made closest
        // after its start; fork B (2000) waits its turn.
        assert_eq!(
            id(pick_thread(
                &both,
                None,
                true,
                Some(q(1_000, 0, 2)),
                &forked,
                3_000
            ))
            .as_deref(),
            Some("ta")
        );
        assert_eq!(
            pick_thread(&both, None, true, Some(q(2_000, 1, 2)), &forked, 3_000),
            None
        );
        // A bound: B is the only one waiting, with its one thread left.
        let left = [fork_thread("tb", 2_500)];
        assert_eq!(
            id(pick_thread(
                &left,
                None,
                false,
                Some(q(2_000, 0, 1)),
                &forked,
                3_000
            ))
            .as_deref(),
            Some("tb")
        );
        // A's thread not there yet: A waits, rather than take B's.
        assert_eq!(
            pick_thread(&left, None, true, Some(q(1_000, 0, 2)), &forked, 3_000),
            None
        );
        // ...until it waited: then the closest after its start, in
        // creation order (never unbound for ever).
        let late = 1_000 + UNREAD_GRACE_MS;
        assert_eq!(
            id(pick_thread(
                &left,
                None,
                true,
                Some(q(1_000, 0, 2)),
                &forked,
                late
            ))
            .as_deref(),
            Some("tb")
        );
        // A later fork whose elders never got a thread binds after waiting.
        assert_eq!(
            id(pick_thread(
                &left,
                None,
                true,
                Some(q(2_000, 1, 2)),
                &forked,
                2_000 + UNREAD_GRACE_MS
            ))
            .as_deref(),
            Some("tb")
        );

        // The queue, and what holds an ordinary session back.
        let w = |id: &str, fork: Option<&str>, since| Waiter {
            id: id.into(),
            folder: "/src/app".into(),
            fork: fork.map(str::to_string),
            since,
        };
        let waiting = [
            w("fb", Some("parent"), 2_000),
            w("fa", Some("parent"), 1_000),
            w("other", Some("elsewhere"), 1_000),
            w("plain", None, 500),
        ];
        let session = |id: &str| -> Session {
            serde_json::from_value(json!({
                "id": id, "name": id, "name_locked": false, "agent": "codex",
                "agent_session": null, "agent_session_trust": "minted",
                "project_path": "/src/app", "tmux_session": null, "status": "starting",
                "group_path": "", "pinned": false, "created_at": 1, "archived_at": null,
                "last_user_msg_at": null, "last_agent_msg_at": null,
                "last_state_change_at": null, "repo_path": null, "workspace_path": null,
                "branch": null, "model": null}))
            .unwrap()
        };
        assert_eq!(
            place_of(&waiting, &session("fa"), 3_000).1,
            Some(Queue {
                place: 0,
                waiting: 2
            })
        );
        assert_eq!(
            place_of(&waiting, &session("fb"), 3_000).1,
            Some(Queue {
                place: 1,
                waiting: 2
            })
        );
        assert_eq!(place_of(&waiting, &session("plain"), 3_000).1, None);
        // An ordinary session: the forks hold it back only while young.
        let alone = [w("plain", None, 500), w("fa", Some("parent"), 1_000)];
        assert!(place_of(&alone, &session("plain"), 3_000).0);
        assert!(!place_of(&alone, &session("plain"), 1_000 + UNREAD_GRACE_MS).0);
        // Forks past the grace: no longer; another ordinary session always.
        assert!(!place_of(&waiting, &session("plain"), 1_000_000).0);
        let two_plain = [w("plain", None, 500), w("plain2", None, 600)];
        assert!(place_of(&two_plain, &session("plain"), 1_000_000).0);
    }

    /// A fork bound from the index whose rollout is on disk is no longer
    /// one to make again at a restart; one whose rollout isn't, still is.
    #[test]
    fn binding_a_fork_with_its_rollout_ends_its_pending_fork() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        for id in ["a", "b"] {
            db.insert_session(&rlord_core::db::NewSession {
                id,
                name: id,
                agent: "codex",
                agent_session: None,
                project_path: "/src/app",
                tmux_session: None,
                launch_spec: "{}",
                created_at: 1,
                repo_path: None,
                workspace_path: None,
                branch: None,
                role: "",
            })
            .unwrap();
            db.kv_set(&crate::fork::pending_key(id), "{}").unwrap();
        }
        let on_disk = dir.path().join("rollout-ta.jsonl");
        std::fs::write(&on_disk, "{}\n").unwrap();
        let mut t = thread("ta", "x");
        t.rollout_path = on_disk.to_string_lossy().into_owned();
        assert!(record_bind(&db, &db.resolve_ref("a").unwrap(), &t).unwrap());
        assert_eq!(db.kv_get(&crate::fork::pending_key("a")).unwrap(), None);
        let mut t = thread("tb", "x");
        t.rollout_path = dir
            .path()
            .join("absent.jsonl")
            .to_string_lossy()
            .into_owned();
        assert!(record_bind(&db, &db.resolve_ref("b").unwrap(), &t).unwrap());
        assert!(db.kv_get(&crate::fork::pending_key("b")).unwrap().is_some());
    }

    #[test]
    fn a_thread_is_bound_only_when_it_is_clearly_the_sessions() {
        let pick = |c: &[codex::Thread], prompt: Option<&str>, others: bool| {
            pick_thread(c, prompt, others, None, &|_| Some(None), 2).map(|t| t.id.clone())
        };
        let one = [thread("a", "fix the login test")];
        // Launched with that prompt: it's the one, even with a sibling waiting.
        assert_eq!(
            pick(&one, Some("fix the login test"), true).as_deref(),
            Some("a")
        );
        // A long prompt Codex kept only the start of.
        assert_eq!(
            pick(
                &one,
                Some("fix the login test, and keep the retry count"),
                false
            )
            .as_deref(),
            Some("a")
        );
        // Someone else's thread in the folder (another prompt): not ours.
        assert_eq!(pick(&one, Some("speed up the image pipeline"), false), None);
        // No first message yet: it may still become ours; wait.
        assert_eq!(pick(&[thread("a", "")], Some("fix it"), false), None);
        // No prompt to go by: the only thread, unless a sibling also waits.
        assert_eq!(pick(&one, None, false).as_deref(), Some("a"));
        assert_eq!(pick(&one, None, true), None);
        // Two that fit equally: neither.
        let two = [thread("a", "fix it"), thread("b", "fix it")];
        assert_eq!(pick(&two, Some("fix it"), false), None);
        assert_eq!(pick(&two, None, false), None);
        assert_eq!(pick(&[], None, false), None);
        // An older Codex (no first messages in its index): the only
        // thread in the folder, when nobody else waits there.
        let old = [codex::Thread {
            first_user_message: None,
            ..thread("a", "")
        }];
        assert_eq!(pick(&old, Some("fix it"), false).as_deref(), Some("a"));
        assert_eq!(pick(&old, Some("fix it"), true), None);
    }
}
