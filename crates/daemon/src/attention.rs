//! The attention queue: which sessions wait on the user, and in what
//! order. Plain code, no AI. (Plan and evidence:
//! `docs/working/attention-queue.md`.)
//!
//! The queue is a pure function of the session rows as the list shows
//! them and of who is looking (`queue`), not a second state machine: the
//! overlord's `attention` tool, `rlord attention` and (later) the bar and
//! the jump key all call it.
//!
//! - **needs you**: stored `waiting_input` (a permission prompt or a
//!   question mid-turn), with its reason. Not for a session whose
//!   needs-you can't be seen (Codex before its hooks are trusted, OpenCode
//!   while its server is out of reach): those are listed as "can't tell".
//! - **your turn**: shown as your turn (stored `idle`, no usage limit or
//!   background work over it), since its latest finished turn or the end
//!   of the background work. Leaves the queue once a person's terminal has
//!   shown the session since (`last_viewed_at`): the user chose to leave
//!   it. Needs you stays until it is answered.
//! - The overlord's own sessions are never in it.
//!
//! **Shadow run.** Nothing is shown or announced yet. A watcher keeps the
//! queue as it changes and logs what an announcement would have done, as
//! session events: `attention_entered`, `attention_settled` (the moment
//! an announcement would be made, and whether it would have been withheld
//! because the session was being looked at), `attention_cleared` (what
//! ended it, and how long it waited). `rlord attention report` reads
//! them. Entries already there when the daemon starts are taken over
//! silently (`carried`).
//!
//! **Presence** is tmux's: a session is being looked at when a focused
//! client of rlord's server shows it (`focus-events` is on; a terminal
//! that never reports focus counts as focused). Looked at again when tmux's
//! client hooks poke (a terminal moved or changed focus), every second
//! while an entry settles, and every 15 s.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};

use rlord_core::db::EventRow;
use rlord_core::timefmt::rel_age_at;
use rlord_core::tmux;
use rlord_core::types::now_ms;

use crate::Daemon;

/// How long an entry must last before it would be announced: needs you
/// is set by a definite event (the shortest real one lasted 3 s); 14% of
/// finished turns were followed by a prompt within 10 s.
pub const SETTLE_NEEDS_YOU_MS: i64 = 3_000;
pub const SETTLE_YOUR_TURN_MS: i64 = 10_000;

const TICK: Duration = Duration::from_secs(1);
/// With nothing settling or unseen, a look every this many ticks (and on
/// every change).
const IDLE_TICKS: u32 = 15;
/// A stable name for a kind of error (not its text, which carries
/// ids and paths): the database's or the system's error kind, else its
/// type.
fn error_kind(e: &anyhow::Error) -> String {
    if let Some(r) = e.downcast_ref::<rusqlite::Error>() {
        return match r {
            rusqlite::Error::SqliteFailure(f, _) => format!("sqlite {}", f.extended_code),
            other => format!("sqlite {:?}", std::mem::discriminant(other)),
        };
    }
    if let Some(io) = e.downcast_ref::<std::io::Error>() {
        return format!("io {:?}", io.kind());
    }
    "other".into()
}

/// How long a presence sample stands for `attention.list`.
const SAMPLE_FRESH_MS: i64 = 60_000;
/// Entries `attention.list` returns by default.
const LIMIT: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    NeedsYou,
    YourTurn,
}

impl Kind {
    fn parse(s: &str) -> Option<Kind> {
        match s {
            "needs_you" => Some(Kind::NeedsYou),
            "your_turn" => Some(Kind::YourTurn),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::NeedsYou => "needs_you",
            Kind::YourTurn => "your_turn",
        }
    }

    fn settle_ms(self) -> i64 {
        match self {
            Kind::NeedsYou => SETTLE_NEEDS_YOU_MS,
            Kind::YourTurn => SETTLE_YOUR_TURN_MS,
        }
    }
}

/// What can't be seen machine-wide, for `input_of`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Blind {
    /// rlord's Codex hooks aren't trusted (`/hooks` in Codex).
    pub codex_untrusted: bool,
    /// No live event stream from OpenCode's server.
    pub opencode: bool,
}

/// One session as the queue sees it (`input_of` builds it from a list row).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Input {
    pub id: String,
    pub name: String,
    /// Stored status, and the status shown (an overlay over `idle`).
    pub status: String,
    pub shown: String,
    pub overlord: bool,
    /// Not archived, not stopped, with a tmux session.
    pub live: bool,
    pub state_since: Option<i64>,
    pub agent_msg_at: Option<i64>,
    /// When background work or a limit last stopped being shown over it.
    pub overlay_ended_at: Option<i64>,
    pub reason: Option<String>,
    /// Whether "needs you" can be seen for it at all.
    pub needs_you_known: bool,
    /// Why not, when it can't.
    pub unknown_why: Option<&'static str>,
    pub last_viewed_at: Option<i64>,
    /// A person's terminal shows it now.
    pub viewed: bool,
    /// Its latest prompt was rlord's (a held message, a resume), not the
    /// user's.
    pub prompted_by_rlord: bool,
    /// Its latest prompt, the user's or rlord's (ms).
    pub last_prompt_at: Option<i64>,
}

/// One entry of the queue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub session: String,
    pub name: String,
    pub kind: Kind,
    /// `permission` | `question` (needs you); `finished` | `failed` (your
    /// turn).
    pub reason: String,
    /// Since when (ms).
    pub since: i64,
    pub waited_ms: i64,
    /// A person's terminal has shown the session since.
    pub seen: bool,
    /// Past its settle time (an announcement would have been made).
    pub settled: bool,
    /// Who sent the prompt this turn answered, when it was rlord.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompted_by: Option<&'static str>,
    /// Your turn: the end of the turn it is about (`last_agent_msg_at`).
    #[serde(skip)]
    pub turn_end: Option<i64>,
}

/// Every entry there is, seen ones included, in the rows' order.
pub fn entries(rows: &[Input], now: i64) -> Vec<Entry> {
    rows.iter()
        .filter(|r| r.live && !r.overlord)
        .filter_map(|r| {
            let (kind, since, reason) = if r.status == "waiting_input" {
                if !r.needs_you_known {
                    return None;
                }
                let reason = match r.reason.as_deref() {
                    Some("question") => "question",
                    _ => "permission",
                };
                (Kind::NeedsYou, r.state_since?, reason)
            } else if r.shown == "idle" {
                // A turn's end not answered yet: none since (just started),
                // or a prompt after it (a restart mid-turn, an interrupt):
                // nothing. From the turn's end, or the background work's
                // end after it; going idle alone (a restart) is no turn.
                let end = r.agent_msg_at?;
                if r.last_prompt_at.is_some_and(|p| p > end) {
                    return None;
                }
                let since = end.max(r.overlay_ended_at.unwrap_or(0));
                let reason = match r.reason.as_deref() {
                    Some("failed") => "failed",
                    _ => "finished",
                };
                (Kind::YourTurn, since, reason)
            } else {
                return None;
            };
            Some(Entry {
                session: r.id.clone(),
                name: r.name.clone(),
                kind,
                reason: reason.to_string(),
                since,
                waited_ms: (now - since).max(0),
                seen: r.viewed || r.last_viewed_at.is_some_and(|v| v >= since),
                settled: now - since >= kind.settle_ms(),
                prompted_by: (kind == Kind::YourTurn && r.prompted_by_rlord).then_some("rlord"),
                turn_end: if kind == Kind::YourTurn {
                    r.agent_msg_at
                } else {
                    None
                },
            })
        })
        .collect()
}

/// The queue: needs you first, then your turn not yet seen; oldest first
/// within each (the longest wait is the most overdue, and the order must
/// not reshuffle under a jump key); ties by session id.
pub fn queue(rows: &[Input], now: i64) -> Vec<Entry> {
    let mut q: Vec<Entry> = entries(rows, now)
        .into_iter()
        .filter(|e| e.kind == Kind::NeedsYou || !e.seen)
        .collect();
    q.sort_by(|a, b| (a.kind, a.since, &a.session).cmp(&(b.kind, b.since, &b.session)));
    q
}

/// Live sessions whose "needs you" can't be seen.
pub fn unknown(rows: &[Input]) -> Vec<&Input> {
    rows.iter()
        .filter(|r| r.live && !r.overlord && !r.needs_you_known)
        .collect()
}

/// `Input` from a row as `session.list` gives it after `annotate_status`.
/// `blind`: whose needs-you can't be seen machine-wide
/// (`Blind`).
pub(crate) fn input_of(
    row: &Value,
    viewing: &HashSet<String>,
    overlay_ended: Option<i64>,
    blind: Blind,
) -> Input {
    let str_of = |k: &str| row[k].as_str().unwrap_or("").to_string();
    let id = str_of("id");
    let status = str_of("status");
    // Codex: its hooks haven't reached this session, and aren't trusted.
    // (Still starting, or no conversation yet: nothing to tell yet.)
    let codex_untrusted = status != "starting"
        && !row["agent_session"].is_null()
        && row["agent_session_trust"] != "hook"
        && blind.codex_untrusted;
    let unknown_why = match row["agent"].as_str() {
        Some("codex") if codex_untrusted => Some("Codex hooks not trusted yet"),
        Some("opencode") if blind.opencode => Some("OpenCode's server is out of reach"),
        _ => None,
    };
    let at = |k: &str| row[k].as_i64();
    Input {
        name: str_of("name"),
        shown: row["shown_status"]
            .as_str()
            .map_or_else(|| status.clone(), str::to_string),
        overlord: crate::overlord::is_overlord_role(row["role"].as_str().unwrap_or("")),
        live: row["archived_at"].is_null()
            && status != "stopped"
            && row["tmux_session"].as_str().is_some_and(|t| !t.is_empty()),
        state_since: at("last_state_change_at"),
        agent_msg_at: at("last_agent_msg_at"),
        overlay_ended_at: overlay_ended,
        reason: row["attention_reason"].as_str().map(str::to_string),
        needs_you_known: unknown_why.is_none(),
        unknown_why,
        last_viewed_at: at("last_viewed_at"),
        viewed: viewing.contains(&id),
        prompted_by_rlord: at("last_rlord_msg_at") > at("last_user_msg_at"),
        last_prompt_at: at("last_rlord_msg_at").max(at("last_user_msg_at")),
        status,
        id,
    }
}

// ---- the watcher (shadow run) ----

type Key = (String, Kind, i64);

/// What the watcher knows of one entry.
#[derive(Debug, Clone, Default)]
struct Track {
    settled: bool,
    /// An announcement would have been made at its settle time.
    announced: bool,
    /// Already there when the daemon started: taken over, never announced.
    carried: bool,
    seen: bool,
    /// Its session can't be told right now (`Blind`): kept as it was, not
    /// cleared, until it can be again.
    suspended: bool,
}

#[derive(Default)]
struct State {
    /// The first look is done.
    started: bool,
    tracked: HashMap<Key, Track>,
    /// Sessions a person's terminal showed at the last sample.
    viewing: HashSet<String>,
    /// When each session's overlay (background work, a limit) last ended,
    /// as recorded at the source (`overlay_ends`).
    overlay_ended: HashMap<String, i64>,
    /// The last real presence sample: when, and who was looking (for
    /// `attention.list`, which asks no tmux).
    sample: Option<(i64, HashSet<String>)>,
    /// Each session's turn (its end, `last_agent_msg_at`) a your-turn
    /// announcement was last made for: never twice for one turn.
    announced_turn: HashMap<String, i64>,
}

/// The watcher's memory (on `Daemon`).
#[derive(Default)]
pub struct Watch {
    state: Mutex<State>,
    /// `codex_hooks_trusted`, and when it was read.
    #[cfg_attr(test, allow(dead_code))]
    codex_trusted: Mutex<Option<(std::time::Instant, bool)>>,
    /// Set by `poke`: a client may have moved.
    moved: AtomicBool,
}

/// The rows the queue reads: live sessions as the list shows them.
fn rows(d: &Daemon) -> Result<Vec<Value>> {
    let listed = crate::methods::session_list(&d.db(), &json!({}))?;
    let mut rows = match listed {
        Value::Array(a) => a,
        _ => Vec::new(),
    };
    crate::annotate_status(d, &mut rows, false);
    Ok(rows)
}

/// When each listed session's overlay (background work, a usage limit)
/// last ended, as recorded where it ends (kv `overlay_end:<id>`:
/// `background.rs`, `limits.rs`): one query, kept for the sessions in
/// `rows`.
fn overlay_ends(d: &Daemon, rows: &[Value]) -> Result<HashMap<String, i64>> {
    let listed: HashSet<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
    Ok(d.db()
        .kv_prefix("overlay_end:")?
        .into_iter()
        .filter_map(|(k, v)| {
            let id = k.strip_prefix("overlay_end:")?;
            if !listed.contains(id) {
                return None;
            }
            Some((id.to_string(), v.parse().ok()?))
        })
        .collect())
}

/// Inputs from `rows`, with the watcher's memory.
fn inputs(d: &Daemon, rows: &[Value], st: &State) -> Vec<Input> {
    inputs_seen(d, rows, st, &st.viewing)
}

/// `inputs`, with who is looking given.
fn inputs_seen(d: &Daemon, rows: &[Value], st: &State, viewing: &HashSet<String>) -> Vec<Input> {
    let codex = rows.iter().any(|r| r["agent"] == "codex");
    let blind = Blind {
        codex_untrusted: codex && !codex_hooks_trusted(d),
        opencode: d.opencode_blind.load(Ordering::Relaxed),
    };
    rows.iter()
        .map(|r| {
            let id = r["id"].as_str().unwrap_or("");
            input_of(r, viewing, st.overlay_ended.get(id).copied(), blind)
        })
        .collect()
}

/// Whether rlord's Codex hooks are trusted (Codex's own config, read at
/// most once a minute).
#[cfg(not(test))]
fn codex_hooks_trusted(d: &Daemon) -> bool {
    let mut c = d
        .attention
        .codex_trusted
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    match *c {
        Some((at, v)) if at.elapsed() < Duration::from_secs(60) => v,
        _ => {
            let v = rlord_core::agents::install::codex_hooks_trusted(
                &rlord_core::agents::codex::home(),
            ) == Some(true);
            *c = Some((std::time::Instant::now(), v));
            v
        }
    }
}

/// (Tests never read the machine's Codex config.)
#[cfg(test)]
fn codex_hooks_trusted(_: &Daemon) -> bool {
    true
}

/// `attention.list`: the queue now, at most `limit` entries, with the
/// counts and the sessions whose needs-you can't be seen.
pub fn list(d: &Daemon, params: &Value) -> Result<Value> {
    let limit = params["limit"]
        .as_u64()
        .map_or(LIMIT, |n| n.clamp(1, 200) as usize);
    let rows = rows(d)?;
    let now = now_ms();
    let inputs = {
        let st = d.attention.state();
        // Who was looking at the last real sample (a minute at most: it
        // asks no tmux itself).
        let viewing = st
            .sample
            .as_ref()
            .filter(|(at, _)| now - at <= SAMPLE_FRESH_MS)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        inputs_seen(d, &rows, &st, &viewing)
    };
    Ok(shape(&inputs, now, limit))
}

/// `list`'s answer, from the inputs.
fn shape(inputs: &[Input], now: i64, limit: usize) -> Value {
    let q = queue(inputs, now);
    let count = |k: Kind| q.iter().filter(|e| e.kind == k).count();
    let shown: Vec<Value> = q
        .iter()
        .take(limit)
        .map(|e| {
            let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
            v["waited"] = json!(rel_age_at(e.since, now));
            v
        })
        .collect();
    let cant: Vec<Value> = unknown(inputs)
        .iter()
        .map(|r| json!({"session": r.id, "name": r.name, "why": r.unknown_why}))
        .collect();
    json!({
        "needs_you": count(Kind::NeedsYou),
        "your_turn": count(Kind::YourTurn),
        "queue": shown,
        "more": q.len().saturating_sub(limit),
        "cant_tell": cant,
    })
}

/// Start the watcher.
pub fn start(d: &Arc<Daemon>) {
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        let mut changes = d.notify.subscribe();
        let mut settling = true;
        let mut quiet = 0u32;
        // The kind of error last warned, and when.
        let mut warned: Option<(String, Instant)> = None;
        loop {
            std::thread::sleep(TICK);
            let poked = d.attention.moved.swap(false, Ordering::Relaxed);
            let mut changed = poked;
            loop {
                use tokio::sync::broadcast::error::TryRecvError;
                match changes.try_recv() {
                    Ok(_) | Err(TryRecvError::Lagged(_)) => changed = true,
                    Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
                }
            }
            quiet += 1;
            // A change (a status, a terminal moving or changing focus:
            // tmux's client hooks poke), or the slow fallback every
            // `IDLE_TICKS`; every tick only while an entry settles.
            let due = changed || quiet >= IDLE_TICKS;
            if !(due || settling) {
                continue;
            }
            if due {
                quiet = 0;
            }
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                look(&d, now_ms(), &tmux::list_clients, due, poked)
            }));
            settling = match r {
                Ok(Ok(p)) => p,
                // (After a failure, back to the idle cadence: a change or
                // the slow fallback looks again, not every tick.)
                // (Warned when the kind of error differs from the last one
                // warned, or ten minutes on: a look that keeps failing the
                // same way, between good looks or not, doesn't flood the
                // log.)
                Ok(Err(e)) => {
                    let kind = error_kind(&e);
                    let again = warned.as_ref().is_none_or(|(k, at)| {
                        *k != kind || at.elapsed() >= Duration::from_secs(600)
                    });
                    if again {
                        warned = Some((kind, Instant::now()));
                        tracing::warn!(error = %format!("{e:#}"), "attention queue");
                    } else {
                        tracing::debug!(error = %e, "attention queue");
                    }
                    false
                }
                Err(_) => {
                    tracing::warn!("attention queue panicked");
                    false
                }
            };
        }
    });
}

impl Watch {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // (A panic mid-look leaves the memory as it was: still usable.)
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A client may have moved to another session or changed focus (`ui.*`
    /// calls, tmux's client hooks: `attention.poke`): look at presence
    /// again on the next tick.
    pub(crate) fn poke(&self) {
        self.moved.store(true, Ordering::Relaxed);
    }
}

/// The sessions focused clients show (`by_tmux`: tmux session name to id).
fn viewing_of(clients: &[tmux::Client], by_tmux: &HashMap<String, String>) -> HashSet<String> {
    clients
        .iter()
        .filter(|c| c.focused)
        .filter_map(|c| by_tmux.get(&c.session).cloned())
        .collect()
}

/// One look: the queue now against what was tracked, shadow events for
/// what changed. `clients` lists tmux's clients (tests pass their own);
/// asked while an entry settles, or (`unseen_too`) while one is unseen,
/// or (`poked`) because a terminal moved or changed focus. Not asked:
/// nobody counts as looking.
/// No lock is held while tmux is asked. Returns whether an entry is still
/// settling (look again next tick).
pub(crate) fn look(
    d: &Daemon,
    now: i64,
    clients: &dyn Fn() -> Vec<tmux::Client>,
    unseen_too: bool,
    poked: bool,
) -> Result<bool> {
    let rows = rows(d)?;
    let ask = {
        let ends = overlay_ends(d, &rows)?;
        let mut st = d.attention.state();
        // When each overlay ended, as recorded where it ends (background
        // work: `background.rs`; a limit: its reset): a turn is yours from
        // then.
        st.overlay_ended = ends;
        st.announced_turn
            .retain(|id, _| rows.iter().any(|r| r["id"] == id.as_str()));
        let all = entries(&inputs(d, &rows, &st), now);
        // (An entry that settles on this look needs who is looking now:
        // its announcement depends on it.)
        let settles_now = |e: &Entry| {
            e.settled
                && !st
                    .tracked
                    .get(&(e.session.clone(), e.kind, e.since))
                    .is_some_and(|t| t.settled)
        };
        poked
            || all
                .iter()
                .any(|e| !e.settled || settles_now(e) || (unseen_too && !e.seen))
    };
    // Who is looking (tmux, with no lock held).
    let by_tmux: HashMap<String, String> = rows
        .iter()
        .filter_map(|r| Some((r["tmux_session"].as_str()?.into(), r["id"].as_str()?.into())))
        .collect();
    let viewing = ask.then(|| viewing_of(&clients(), &by_tmux));

    let mut guard = d.attention.state();
    // (Fields borrowed apart below.)
    let st = &mut *guard;
    // (No focused client, or not asked: nobody.)
    if let Some(v) = &viewing {
        st.sample = Some((now, v.clone()));
    }
    st.viewing = viewing.unwrap_or_default();
    let ins = inputs(d, &rows, st);
    let all = entries(&ins, now);
    let db = d.db();
    // Looked at since its entry began: stamped once (`last_viewed_at`
    // older than the entry), so it stays seen after the user leaves.
    let stamp: Vec<&str> = ins
        .iter()
        .filter(|r| r.viewed)
        .filter(|r| {
            all.iter()
                .any(|e| e.session == r.id && r.last_viewed_at.is_none_or(|v| v < e.since))
        })
        .map(|r| r.id.as_str())
        .collect();
    if !stamp.is_empty() {
        db.set_last_viewed(&stamp, now)?;
    }

    let first = !st.started;
    let current: HashSet<Key> = all
        .iter()
        .map(|e| (e.session.clone(), e.kind, e.since))
        .collect();
    // After a daemon start: what the log left open and is gone now ended
    // while rlord wasn't watching (`by: restart`). What is still there
    // goes on (`carried`).
    if first {
        for rlord_core::db::OpenEntry {
            session: id,
            kind,
            since,
            seq,
        } in db.open_attention_entries()?
        {
            // (A row the log can't say: closed, and said so, once: the
            // marker matches its kind, unreadable or none, so it isn't
            // open at the next start.)
            let (Some(kind), Some(since)) =
                (kind.clone().filter(|k| Kind::parse(k).is_some()), since)
            else {
                tracing::warn!(session = %id, "an attention entry in the log can't be read: closed");
                let p = json!({"kind": kind, "by": "unreadable", "closes_seq": seq});
                db.insert_event(&id, now, "attention_cleared", &p.to_string())?;
                continue;
            };
            let still = all
                .iter()
                .any(|e| e.session == id && e.kind.as_str() == kind && e.since == since);
            if still {
                continue;
            }
            // Its session can't be told right now: not ended, suspended
            // (as a blind spell is), resolved once it can be told again.
            let blind = ins
                .iter()
                .any(|r| r.id == id && r.live && !r.needs_you_known);
            if blind && kind == Kind::NeedsYou.as_str() {
                let p = json!({"kind": kind, "since": since});
                db.insert_event(&id, now, "attention_blind", &p.to_string())?;
                st.tracked.insert(
                    (id, Kind::NeedsYou, since),
                    Track {
                        settled: true,
                        carried: true,
                        suspended: true,
                        ..Default::default()
                    },
                );
                continue;
            }
            let p = json!({"kind": kind, "by": "restart", "waited_ms": now - since,
                           "announced": false, "seen": false});
            db.insert_event(&id, now, "attention_cleared", &p.to_string())?;
        }
        // (Only once that went through: else the next look tries again.)
        st.started = true;
    }
    // Gone: cleared. Forgotten only once that is written (a failed write
    // is tried again on the next look).
    let gone: Vec<Key> = st
        .tracked
        .keys()
        .filter(|k| !current.contains(*k))
        .cloned()
        .collect();
    for key in gone {
        let t = st.tracked.get(&key).cloned().unwrap_or_default();
        let (id, kind, since) = &key;
        let row = ins.iter().find(|r| &r.id == id);
        // Its session can't be told now: suspended, not cleared (a marker
        // once), and taken up again as it was when it can.
        if *kind == Kind::NeedsYou && row.is_some_and(|r| r.live && !r.needs_you_known) {
            if !t.suspended {
                let p = json!({"kind": kind.as_str(), "since": since});
                match db.insert_event(id, now, "attention_blind", &p.to_string()) {
                    Ok(()) => {
                        if let Some(t) = st.tracked.get_mut(&key) {
                            t.suspended = true;
                        }
                    }
                    Err(e) => tracing::debug!(session = %id, error = %e, "attention_blind"),
                }
            }
            continue;
        }
        // (What ended it: the first clearing event after its own trigger.)
        // (Only the kinds that can clear an entry, all of them: however
        // many other events came first.)
        let written = db
            .events_of_kinds_after(id, *since, CLEARING_KINDS)
            .and_then(|evs| {
                // (A turn's end logged after the fact carries its own
                // time: one at or before the entry's since is its trigger.)
                let evs: Vec<EventRow> = evs
                    .into_iter()
                    .filter(|e| e.payload["idle_at"].as_i64().is_none_or(|t| t > *since))
                    .collect();
                let by = cleared_by(*kind, row, &evs, *since);
                let mut p = json!({"kind": kind.as_str(), "by": by, "waited_ms": now - since,
                               "announced": t.announced, "seen": t.seen});
                if by == "ended" {
                    let (how, at) = ended_how(&evs);
                    p["ended"] = json!(how);
                    if let Some(at) = at.filter(|_| how == "pane") {
                        p["dead_at"] = json!(at);
                    }
                }
                if t.carried {
                    p["carried"] = json!(true);
                }
                db.insert_event(id, now, "attention_cleared", &p.to_string())
            });
        match written {
            Ok(()) => {
                st.tracked.remove(&key);
            }
            Err(e) => tracing::debug!(session = %id, error = %e, "attention_cleared"),
        }
    }
    // New, settling, seen.
    for e in &all {
        let key = (e.session.clone(), e.kind, e.since);
        let t = st.tracked.entry(key).or_insert_with(|| {
            if first {
                return Track {
                    settled: true,
                    carried: true,
                    ..Default::default()
                };
            }
            let p = json!({"kind": e.kind.as_str(), "reason": e.reason, "since": e.since});
            if let Err(err) = db.insert_event(&e.session, now, "attention_entered", &p.to_string())
            {
                tracing::debug!(error = %err, "attention_entered");
            }
            Track::default()
        });
        // Back from "can't tell": as it was (no entered, no announcement).
        t.suspended = false;
        if !t.settled && e.settled {
            let viewed = ins.iter().any(|r| r.id == e.session && r.viewed);
            // Once a turn: an entry for a turn already announced (it left
            // the queue and came back) isn't announced again.
            let repeat = e
                .turn_end
                .is_some_and(|t| st.announced_turn.get(&e.session) == Some(&t));
            // Never announced while looked at; nor a turn already seen.
            let announced = !viewed && !e.seen && !repeat;
            let mut p = json!({"kind": e.kind.as_str(), "viewed": viewed, "seen": e.seen,
                               "announced": announced, "reason": e.reason});
            if repeat {
                p["repeat"] = json!(true);
            }
            // (Not written: settled on a later look.)
            match db.insert_event(&e.session, now, "attention_settled", &p.to_string()) {
                Ok(()) => {
                    t.settled = true;
                    t.announced = announced;
                    if let (true, Some(turn)) = (announced, e.turn_end) {
                        st.announced_turn.insert(e.session.clone(), turn);
                    }
                }
                Err(err) => tracing::debug!(error = %err, "attention_settled"),
            }
        }
        t.seen |= e.seen;
    }
    Ok(all.iter().any(|e| !e.settled))
}

/// How an ended entry's session ended, from its events after the entry
/// began (oldest first), and when: `pane` (its
/// pane or tmux session was found gone: a crash, a kill), `exit` (the
/// agent ended itself), `stop` / `archive` (the user), else `unknown`.
pub fn ended_how(events: &[EventRow]) -> (&'static str, Option<i64>) {
    for e in events {
        let how = match e.kind.as_str() {
            "reconciled_dead" => "pane",
            "session_end" if e.payload["from"] == "pane" => "pane",
            "session_end" => "exit",
            "stopped" => "stop",
            "archived" => "archive",
            _ => continue,
        };
        return (how, Some(e.ts));
    }
    ("unknown", None)
}

/// The event kinds `cleared_by` and `ended_how` read.
/// Each one is logged with a status change where it is logged (checked
/// call site by call site: the hooks, OpenCode, `discover`, restarts,
/// stops, the reconcile), but two: `message_delivered` changes the status
/// only from idle (a Stop's held messages), and `fresh_conversation`,
/// logged by a hook for a `/clear`, comes with no change of its own (a
/// fresh conversation started by rlord sets `starting`). A limit or
/// background work changes none: `cleared_by` reads those from the row as
/// `overlay`.
const CLEARING_KINDS: &[&str] = &[
    "prompt_submitted",
    "rlord_prompt",
    "tool",
    "answered",
    "turn_started",
    "agent_stopped",
    "session_end",
    "reconciled_dead",
    "stopped",
    "archived",
    "session_start",
    "restarted",
    "resumed",
    "reforked",
    "fresh_conversation",
    "message_delivered",
    "awaiting_first_prompt",
    "permission_request",
    "question_asked",
    "question",
    "notification",
];

/// What ended an entry, from the session as it is now (`row`) and its
/// events after the entry's own trigger (oldest first): the first one that
/// clears it. One of `ended` (stopped,
/// archived, gone), `overlay` (background work or a limit now shown over
/// it), `prompt` (the user's), `rlord` (a prompt rlord typed), `answer`
/// (a tool ran: the prompt was answered), `queued` (a turn the user
/// queued started), `stop` (another turn ended), `unknown` (no event
/// says),
/// or `self` (the agent moved
/// on with no user action: it asked, or worked, by itself).
pub fn cleared_by(
    kind: Kind,
    row: Option<&Input>,
    events: &[EventRow],
    since: i64,
) -> &'static str {
    let Some(row) = row.filter(|r| r.live) else {
        return "ended";
    };
    if kind == Kind::YourTurn && matches!(row.shown.as_str(), "limited" | "background") {
        return "overlay";
    }
    for e in events {
        let by = match e.kind.as_str() {
            "prompt_submitted" if e.payload["wake"] == true => "self",
            "prompt_submitted" => "prompt",
            // rlord moved it: a prompt or held message it typed, a start
            // it settled.
            "rlord_prompt" | "message_delivered" | "awaiting_first_prompt" => "rlord",
            "tool" | "answered" if kind == Kind::NeedsYou => "answer",
            // A turn the user queued while the agent worked (no prompt
            // hook for it: `methods::wakes_idle`).
            "tool" if e.payload["woke"] == true => "queued",
            "tool" | "answered" => "self",
            // (OpenCode: a turn began with no prompt seen.)
            "turn_started" => "self",
            "agent_stopped" => "stop",
            "session_end" | "reconciled_dead" | "stopped" | "archived" | "session_start"
            | "restarted" | "resumed" | "reforked" | "fresh_conversation" => "ended",
            // (Claude fires a question's `PreToolUse` and its
            // `PermissionRequest` together, and a permission notification
            // with a request: the other half of the entry's own trigger.)
            "permission_request" | "question_asked" | "question" | "notification"
                if kind == Kind::NeedsYou && e.ts - since <= 1_000 =>
            {
                continue
            }
            "permission_request" | "question_asked" | "question" | "notification" => "self",
            _ => continue,
        };
        return by;
    }
    // No event says; for a turn of the user's, an overlay that ended after
    // it began does (it went to background work or a limit and back).
    if kind == Kind::YourTurn && row.overlay_ended_at.is_some_and(|t| t > since) {
        return "overlay";
    }
    // (Nothing logged that says: never guessed to be the agent's own.)
    "unknown"
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000_000;

    fn row(id: &str, status: &str) -> Input {
        Input {
            id: id.into(),
            name: id.into(),
            status: status.into(),
            shown: status.into(),
            live: true,
            needs_you_known: true,
            state_since: Some(NOW - 60_000),
            agent_msg_at: Some(NOW - 60_000),
            ..Default::default()
        }
    }

    fn kinds(q: &[Entry]) -> Vec<(&str, Kind)> {
        q.iter().map(|e| (e.session.as_str(), e.kind)).collect()
    }

    #[test]
    fn needs_you_first_then_unseen_turns_oldest_first() {
        let mut a = row("a", "idle");
        a.agent_msg_at = Some(NOW - 5_000);
        let mut b = row("b", "waiting_input");
        b.state_since = Some(NOW - 1_000);
        b.reason = Some("question".into());
        let mut c = row("c", "idle");
        c.agent_msg_at = Some(NOW - 90_000);
        let mut d = row("d", "waiting_input");
        d.state_since = Some(NOW - 20_000);
        let q = queue(&[a, b, c, d], NOW);
        use Kind::*;
        assert_eq!(
            kinds(&q),
            [
                ("d", NeedsYou),
                ("b", NeedsYou),
                ("c", YourTurn),
                ("a", YourTurn)
            ]
        );
        assert_eq!(q[1].reason, "question");
        assert_eq!(
            q[0].reason, "permission",
            "unknown reason reads as a prompt to allow"
        );
        // Settle: needs you after 3 s, your turn after 10 s.
        assert!(!q[1].settled && q[0].settled);
        assert!(q[2].settled && !q[3].settled);
        // Ties by id, so the order never reshuffles.
        let (x, y) = (row("x", "idle"), row("y", "idle"));
        assert_eq!(kinds(&queue(&[y, x], NOW))[0].0, "x");
    }

    #[test]
    fn what_is_never_an_entry() {
        let mut rows = vec![];
        for st in ["working", "starting", "stopped", "error"] {
            rows.push(row(st, st));
        }
        let mut bg = row("bg", "idle");
        bg.shown = "background".into();
        let mut lim = row("lim", "idle");
        lim.shown = "limited".into();
        let mut dead = row("dead", "waiting_input");
        dead.live = false;
        // The overlord, either kind (the user's decision, 2026-11-18).
        let mut o1 = row("o1", "waiting_input");
        o1.overlord = true;
        let mut o2 = row("o2", "idle");
        o2.overlord = true;
        // Never finished a turn: just started.
        let mut fresh = row("fresh", "idle");
        fresh.agent_msg_at = None;
        // Codex without trusted hooks: can't tell, never a guess.
        let mut codex = row("codex", "waiting_input");
        codex.needs_you_known = false;
        codex.unknown_why = Some("Codex hooks not trusted yet");
        rows.extend([bg, lim, dead, o1, o2, fresh, codex]);
        assert!(entries(&rows, NOW).is_empty());
        let cant: Vec<&str> = unknown(&rows).iter().map(|r| r.id.as_str()).collect();
        assert_eq!(cant, ["codex"]);
    }

    #[test]
    fn looking_clears_your_turn_but_not_needs_you() {
        let mut t = row("t", "idle");
        t.last_viewed_at = Some(NOW - 30_000);
        let mut n = row("n", "waiting_input");
        n.last_viewed_at = Some(NOW - 30_000);
        let q = queue(&[t.clone(), n.clone()], NOW);
        assert_eq!(kinds(&q), [("n", Kind::NeedsYou)]);
        assert!(q[0].seen);
        // A later turn's end is new again.
        t.agent_msg_at = Some(NOW - 10_000);
        assert_eq!(queue(&[t.clone()], NOW).len(), 1);
        // Being looked at now counts as seen.
        t.viewed = true;
        assert!(queue(&[t], NOW).is_empty());
    }

    #[test]
    fn your_turn_dates_from_the_end_of_background_work() {
        let mut r = row("r", "idle");
        r.agent_msg_at = Some(NOW - 600_000);
        r.overlay_ended_at = Some(NOW - 2_000);
        r.reason = Some("failed".into());
        r.prompted_by_rlord = true;
        let e = &entries(&[r], NOW)[0];
        assert_eq!(
            (e.since, e.waited_ms, e.settled),
            (NOW - 2_000, 2_000, false)
        );
        assert_eq!(
            (e.reason.as_str(), e.prompted_by),
            ("failed", Some("rlord"))
        );
    }

    fn ev(kind: &str, payload: Value) -> EventRow {
        EventRow {
            seq: 0,
            ts: NOW,
            kind: kind.into(),
            payload,
        }
    }

    #[test]
    fn what_cleared_an_entry() {
        use Kind::*;
        let working = row("s", "working");
        let by = |k, evs: &[EventRow]| cleared_by(k, Some(&working), evs, NOW - 5_000);
        let hook = |h: &str| json!({"hook": h});
        assert_eq!(by(NeedsYou, &[ev("tool", hook("PostToolUse"))]), "answer");
        assert_eq!(
            by(
                YourTurn,
                &[ev("prompt_submitted", hook("UserPromptSubmit"))]
            ),
            "prompt"
        );
        assert_eq!(
            by(
                YourTurn,
                &[ev(
                    "prompt_submitted",
                    json!({"hook": "UserPromptSubmit", "wake": true})
                )]
            ),
            "self",
            "a background task's finish is no prompt of the user's"
        );
        assert_eq!(by(YourTurn, &[ev("tool", json!({"woke": true}))]), "queued");
        // The other half of a needs-you's own trigger (within a second):
        // not what cleared it; the answer after is.
        let mut pair = ev("permission_request", hook("PermissionRequest"));
        pair.ts = NOW - 4_500;
        assert_eq!(
            by(NeedsYou, &[pair, ev("tool", hook("PostToolUse"))]),
            "answer"
        );
        assert_eq!(
            by(YourTurn, &[ev("rlord_prompt", hook("UserPromptSubmit"))]),
            "rlord"
        );
        assert_eq!(by(YourTurn, &[ev("agent_stopped", hook("Stop"))]), "stop");
        // The old false needs-you: an idle session asked by itself.
        assert_eq!(
            by(YourTurn, &[ev("notification", hook("Notification"))]),
            "self"
        );
        // The first clearing event after the entry's trigger decides (an
        // OpenCode prompt, then its turn starting: the prompt); unrelated
        // ones are skipped.
        let evs = [
            ev("renamed", json!({})),
            ev("prompt_submitted", json!({"from": "opencode session"})),
            ev("turn_started", json!({})),
            ev("agent_stopped", hook("Stop")),
        ];
        assert_eq!(by(YourTurn, &evs), "prompt");
        assert_eq!(by(YourTurn, &[ev("turn_started", json!({}))]), "self");
        // Not a status change: not what cleared it.
        for k in ["limit_hit", "pane_gone_turn_running"] {
            assert_eq!(by(YourTurn, &[ev(k, json!({}))]), "unknown", "{k}");
        }
        // A restart in a new conversation, or forked again: it (re)started.
        for k in ["fresh_conversation", "reforked"] {
            assert_eq!(by(YourTurn, &[ev(k, json!({}))]), "ended", "{k}");
        }
        assert_eq!(by(NeedsYou, &[ev("restarted", json!({}))]), "ended");
        // How it ended, and when.
        let mut pane = ev("session_end", json!({"from": "pane"}));
        pane.ts = NOW + 5;
        assert_eq!(
            ended_how(&[ev("renamed", json!({})), pane]),
            ("pane", Some(NOW + 5))
        );
        assert_eq!(ended_how(&[ev("archived", json!({}))]).0, "archive");
        assert_eq!(ended_how(&[]), ("unknown", None));
        assert_eq!(by(YourTurn, &[]), "unknown", "nothing says: never self");
        // rlord moved it: its own `by`.
        for k in ["message_delivered", "awaiting_first_prompt"] {
            assert_eq!(by(YourTurn, &[ev(k, json!({}))]), "rlord", "{k}");
        }
        // A kind nobody maps: skipped, then unknown.
        assert_eq!(by(YourTurn, &[ev("renamed", json!({}))]), "unknown");
        let mut bg = row("s", "idle");
        bg.shown = "background".into();
        assert_eq!(cleared_by(YourTurn, Some(&bg), &[], NOW), "overlay");
        // No event says, but an overlay ended after it began: overlay.
        let mut back = row("s", "idle");
        back.overlay_ended_at = Some(NOW + 5);
        assert_eq!(cleared_by(YourTurn, Some(&back), &[], NOW), "overlay");
        // (Needs you: an overlay doesn't end one.)
        assert_eq!(cleared_by(NeedsYou, Some(&back), &[], NOW), "unknown");
        back.overlay_ended_at = Some(NOW - 5);
        assert_eq!(cleared_by(YourTurn, Some(&back), &[], NOW), "unknown");
        let mut gone = row("s", "stopped");
        gone.live = false;
        assert_eq!(cleared_by(NeedsYou, Some(&gone), &[], NOW), "ended");
        assert_eq!(cleared_by(NeedsYou, None, &[], NOW), "ended");
    }

    #[test]
    fn an_input_from_a_list_row() {
        let viewing: HashSet<String> = ["01a".to_string()].into();
        let r = json!({"id": "01a", "name": "api", "agent": "claude", "status": "idle",
            "shown_status": "background", "role": "", "archived_at": null, "tmux_session": "api-rlord",
            "last_state_change_at": 5, "last_agent_msg_at": 4, "attention_reason": "finished",
            "last_viewed_at": 3, "last_rlord_msg_at": 9, "last_user_msg_at": 2});
        let i = input_of(&r, &viewing, Some(7), Blind::default());
        assert_eq!(
            (i.status.as_str(), i.shown.as_str()),
            ("idle", "background")
        );
        assert!(i.live && i.viewed && i.needs_you_known && i.prompted_by_rlord && !i.overlord);
        assert_eq!(
            (
                i.state_since,
                i.agent_msg_at,
                i.overlay_ended_at,
                i.last_viewed_at
            ),
            (Some(5), Some(4), Some(7), Some(3))
        );
        let stopped =
            json!({"id": "01b", "status": "stopped", "tmux_session": "b", "archived_at": null});
        assert!(!input_of(&stopped, &viewing, None, Blind::default()).live);
        let oc = json!({"id": "01c", "agent": "opencode", "status": "working", "tmux_session": "c", "archived_at": null});
        assert!(
            !input_of(
                &oc,
                &viewing,
                None,
                Blind {
                    opencode: true,
                    ..Default::default()
                }
            )
            .needs_you_known
        );
        assert!(input_of(&oc, &viewing, None, Blind::default()).needs_you_known);
        let over = json!({"id": "01o", "role": "overlord", "status": "idle", "tmux_session": "o", "archived_at": null});
        assert!(input_of(&over, &viewing, None, Blind::default()).overlord);
    }

    // ---- the watcher, over a database (no tmux: clients are passed in) ----

    use rlord_core::config::Config;
    use rlord_core::db::Db;
    use rlord_core::types::Status;

    fn add(db: &Db, id: &str, status: Status, at: i64) {
        add_as(db, id, "claude", status, at)
    }

    fn add_as(db: &Db, id: &str, agent: &str, status: Status, at: i64) {
        db.insert_session(&rlord_core::db::NewSession {
            id,
            name: id,
            agent,
            agent_session: Some(id),
            project_path: "/tmp",
            tmux_session: Some(&format!("t-{id}")),
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
        db.touch_last_agent_msg(id, at).unwrap();
    }

    fn client(session: &str, focused: bool) -> tmux::Client {
        tmux::Client {
            name: "/dev/ttys001".into(),
            session: session.into(),
            activity: 0,
            focused,
        }
    }

    fn shadow(d: &Daemon, id: &str) -> Vec<(String, Value)> {
        d.db()
            .list_events(id, 50)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind.starts_with("attention_"))
            .map(|e| (e.kind, e.payload))
            .collect()
    }

    #[test]
    fn the_shadow_run_logs_what_an_announcement_would_have_done() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let t0 = NOW;
        {
            let db = d.db();
            add(&db, "a", Status::Idle, t0);
            add(&db, "b", Status::WaitingInput, t0);
        }
        let none = Vec::new;
        // At start: what is there is taken over, nothing logged.
        assert!(
            !look(&d, t0 + 60_000, &none, true, false).unwrap(),
            "nothing settling"
        );
        assert!(shadow(&d, "a").is_empty() && shadow(&d, "b").is_empty());
        // b is answered: cleared, carried, by the answer.
        {
            let db = d.db();
            db.set_status("b", Status::Working, t0 + 61_000).unwrap();
            db.insert_event("b", t0 + 61_000, "tool", r#"{"hook":"PostToolUse"}"#)
                .unwrap();
        }
        look(&d, t0 + 61_500, &none, true, false).unwrap();
        let b = shadow(&d, "b");
        assert_eq!(b[0].0, "attention_cleared");
        assert_eq!(
            (
                &b[0].1["kind"],
                &b[0].1["by"],
                &b[0].1["carried"],
                &b[0].1["announced"]
            ),
            (
                &json!("needs_you"),
                &json!("answer"),
                &json!(true),
                &json!(false)
            )
        );
        // b asks again: entered; past 3 s with nobody looking, it would be
        // announced.
        let t1 = t0 + 70_000;
        d.db().set_status("b", Status::WaitingInput, t1).unwrap();
        look(&d, t1 + 500, &none, true, false).unwrap();
        look(&d, t1 + 3_500, &none, true, false).unwrap();
        let b = shadow(&d, "b");
        assert_eq!(b[1].0, "attention_entered");
        assert_eq!(b[2].0, "attention_settled");
        assert_eq!(
            (&b[2].1["announced"], &b[2].1["viewed"]),
            (&json!(true), &json!(false))
        );
        // a's turn ends again while the user looks at it: withheld, seen,
        // and it leaves the queue.
        let t2 = t0 + 80_000;
        {
            let db = d.db();
            db.touch_last_agent_msg("a", t2).unwrap();
            db.insert_event("a", t2, "agent_stopped", r#"{"hook":"Stop"}"#)
                .unwrap();
        }
        let looking = || vec![client("t-a", true)];
        look(&d, t2 + 500, &looking, true, false).unwrap();
        let settling = look(&d, t2 + 10_500, &looking, true, false).unwrap();
        let a = shadow(&d, "a");
        let kinds: Vec<&str> = a.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "attention_cleared",
                "attention_entered",
                "attention_settled"
            ]
        );
        assert_eq!(a[0].1["by"], "stop", "the old turn gave way to the new one");
        assert_eq!(
            (&a[2].1["announced"], &a[2].1["viewed"]),
            (&json!(false), &json!(true))
        );
        // (Stamped once, when it made the entry seen.)
        assert_eq!(
            d.db().resolve_ref("a").unwrap().last_viewed_at,
            Some(t2 + 500)
        );
        let q = list(&d, &json!({})).unwrap();
        assert_eq!(q["your_turn"], 0);
        assert_eq!(q["needs_you"], 1);
        // Nothing settles any more: no look every tick.
        assert!(!settling);
        // The user leaves: still seen (stamped while looked at).
        look(&d, t2 + 11_000, &none, true, false).unwrap();
        assert_eq!(list(&d, &json!({})).unwrap()["your_turn"], 0);
        // A window without focus doesn't count as looking.
        let t3 = t0 + 100_000;
        d.db().touch_last_agent_msg("a", t3).unwrap();
        let unfocused = || vec![client("t-a", false)];
        look(&d, t3 + 500, &unfocused, true, false).unwrap();
        look(&d, t3 + 10_500, &unfocused, true, false).unwrap();
        let a = shadow(&d, "a");
        assert_eq!(a.last().unwrap().1["announced"], true);
        // Its pane found dead: cleared as ended, by the pane.
        {
            let db = d.db();
            db.set_status("a", Status::Stopped, t3 + 20_000).unwrap();
            db.insert_event("a", t3 + 20_000, "session_end", r#"{"from":"pane"}"#)
                .unwrap();
        }
        look(&d, t3 + 20_500, &none, true, false).unwrap();
        let last = shadow(&d, "a").last().unwrap().1.clone();
        assert_eq!(
            (&last["by"], &last["ended"]),
            (&json!("ended"), &json!("pane"))
        );
    }

    /// After a daemon start: an entry the log left open and that is gone
    /// now is closed (`restart`); one still there goes on, carried.
    #[test]
    fn a_restart_closes_what_ended_unwatched() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let t0 = NOW;
        {
            let db = d.db();
            add(&db, "a", Status::Idle, t0);
            add(&db, "b", Status::Working, t0);
            let entered = |id: &str, kind: &str, since: i64| {
                let p = json!({"kind": kind, "since": since});
                db.insert_event(id, since, "attention_entered", &p.to_string())
                    .unwrap();
            };
            // a's turn: still there. b's question: answered while down.
            entered("a", "your_turn", t0);
            entered("b", "needs_you", t0 - 5_000);
        }
        look(&d, t0 + 60_000, &Vec::new, true, false).unwrap();
        assert!(shadow(&d, "a")
            .iter()
            .all(|(k, _)| k == "attention_entered"));
        let b = shadow(&d, "b");
        assert_eq!(b.last().unwrap().0, "attention_cleared");
        assert_eq!(b.last().unwrap().1["by"], "restart");
        // Logged once: the next look doesn't close it again.
        look(&d, t0 + 61_000, &Vec::new, true, false).unwrap();
        assert_eq!(shadow(&d, "b").len(), 2);
        // a goes on and is cleared as carried.
        d.db()
            .set_status("a", Status::Working, t0 + 62_000)
            .unwrap();
        look(&d, t0 + 62_500, &Vec::new, true, false).unwrap();
        let a = shadow(&d, "a");
        assert_eq!(a.last().unwrap().1["carried"], true);
    }

    /// A turn is announced once: back in the queue for the same turn (it
    /// went idle again with no new turn end), it isn't announced again.
    /// After a start, a needs-you the log left open on a session that
    /// can't be told right now is suspended, not closed by the restart;
    /// once it can be told, it is resolved as usual.
    #[test]
    fn a_restart_keeps_a_blind_sessions_entry() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let t0 = NOW;
        {
            let db = d.db();
            add_as(&db, "oc", "opencode", Status::WaitingInput, t0);
            let p = json!({"kind": "needs_you", "since": t0});
            db.insert_event("oc", t0, "attention_entered", &p.to_string())
                .unwrap();
        }
        d.opencode_blind.store(true, Ordering::Relaxed);
        look(&d, t0 + 60_000, &Vec::new, true, false).unwrap();
        let kinds =
            |d: &Daemon| -> Vec<String> { shadow(d, "oc").into_iter().map(|(k, _)| k).collect() };
        assert_eq!(kinds(&d), ["attention_entered", "attention_blind"]);
        // Still blind: nothing more.
        look(&d, t0 + 61_000, &Vec::new, true, false).unwrap();
        assert_eq!(kinds(&d).len(), 2);
        // Told again, still waiting: goes on (no new entry).
        d.opencode_blind.store(false, Ordering::Relaxed);
        look(&d, t0 + 62_000, &Vec::new, true, false).unwrap();
        assert_eq!(kinds(&d).len(), 2);
        // Answered: cleared, carried.
        {
            let db = d.db();
            db.set_status("oc", Status::Working, t0 + 63_000).unwrap();
            db.insert_event("oc", t0 + 63_000, "answered", "{}")
                .unwrap();
        }
        look(&d, t0 + 63_500, &Vec::new, true, false).unwrap();
        let last = shadow(&d, "oc").last().unwrap().1.clone();
        assert_eq!(
            (&last["by"], &last["carried"]),
            (&json!("answer"), &json!(true))
        );
    }

    /// What cleared an entry is found however many other events came
    /// first (a busy session logs many).
    #[test]
    fn the_answer_is_found_past_many_other_events() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let t0 = NOW;
        add(&d.db(), "w", Status::Working, t0);
        let none = Vec::new;
        look(&d, t0 + 500, &none, true, false).unwrap();
        d.db()
            .set_status("w", Status::WaitingInput, t0 + 1_000)
            .unwrap();
        look(&d, t0 + 1_500, &none, true, false).unwrap();
        {
            let db = d.db();
            for i in 0..80 {
                db.insert_event("w", t0 + 2_000 + i, "subagent_started", "{}")
                    .unwrap();
            }
            db.set_status("w", Status::Working, t0 + 3_000).unwrap();
            db.insert_event("w", t0 + 3_000, "tool", r#"{"hook":"PostToolUse"}"#)
                .unwrap();
        }
        look(&d, t0 + 3_500, &none, true, false).unwrap();
        let last = shadow(&d, "w").last().unwrap().1.clone();
        assert_eq!(
            (&last["kind"], &last["by"]),
            (&json!("needs_you"), &json!("answer"))
        );
    }

    /// OpenCode logs a turn's end when rlord notices it, with its own time
    /// (`idle_at`): that event is the entry's trigger, not what cleared it.
    #[test]
    fn a_turn_end_logged_late_is_its_own_trigger() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let t0 = NOW;
        add_as(&d.db(), "oc", "opencode", Status::Working, t0);
        let none = Vec::new;
        look(&d, t0 + 100, &none, true, false).unwrap();
        {
            let db = d.db();
            db.set_status("oc", Status::Idle, t0 + 5_000).unwrap();
            db.touch_last_agent_msg("oc", t0 + 1_000).unwrap();
            let p = json!({"from": "opencode session", "idle_at": t0 + 1_000});
            db.insert_event("oc", t0 + 5_000, "agent_stopped", &p.to_string())
                .unwrap();
        }
        look(&d, t0 + 5_500, &none, true, false).unwrap();
        {
            let db = d.db();
            db.set_status("oc", Status::Working, t0 + 9_000).unwrap();
            db.insert_event("oc", t0 + 9_000, "prompt_submitted", "{}")
                .unwrap();
        }
        look(&d, t0 + 9_500, &none, true, false).unwrap();
        let last = shadow(&d, "oc").last().unwrap().1.clone();
        assert_eq!(
            (&last["kind"], &last["by"]),
            (&json!("your_turn"), &json!("prompt"))
        );
    }

    /// The look on which an entry settles asks tmux who is looking, even
    /// with nothing else to look for: one session, first seen already
    /// past its settle time, looked at.
    #[test]
    fn the_settling_look_asks_who_is_looking() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let t0 = NOW;
        add(&d.db(), "a", Status::Working, t0);
        let there = || vec![client("t-a", true)];
        look(&d, t0 + 100, &there, false, false).unwrap();
        {
            let db = d.db();
            db.set_status("a", Status::Idle, t0 + 1_000).unwrap();
            db.touch_last_agent_msg("a", t0 + 1_000).unwrap();
        }
        // First seen past its 10 s: it settles on this look.
        look(&d, t0 + 12_000, &there, false, false).unwrap();
        let settled = shadow(&d, "a")
            .into_iter()
            .find(|(k, _)| k == "attention_settled")
            .unwrap()
            .1;
        assert_eq!(
            (&settled["viewed"], &settled["announced"]),
            (&json!(true), &json!(false))
        );
    }

    /// When background work ended is read from where it was recorded
    /// (kv `overlay_end:`), not from the watcher's memory: it holds over a
    /// daemon restart (a new watcher), and for work that came and went
    /// between two looks.
    #[test]
    fn an_overlays_end_survives_a_restart() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let t0 = NOW;
        add(&d.db(), "a", Status::Working, t0);
        let none = Vec::new;
        // (A fresh watcher: nothing in memory.)
        look(&d, t0 + 100, &none, true, false).unwrap();
        {
            let db = d.db();
            db.set_status("a", Status::Idle, t0 + 1_000).unwrap();
            db.touch_last_agent_msg("a", t0 + 1_000).unwrap();
            db.kv_set(
                &crate::background::overlay_end_key("a"),
                &(t0 + 30_000).to_string(),
            )
            .unwrap();
        }
        look(&d, t0 + 31_000, &none, true, false).unwrap();
        let entered = shadow(&d, "a")
            .into_iter()
            .find(|(k, _)| k == "attention_entered")
            .unwrap()
            .1;
        assert_eq!(
            entered["since"],
            t0 + 30_000,
            "from the work's end, not the turn's"
        );
    }

    /// `attention.list` goes by the last real presence sample (a minute at
    /// most), not an emptied set: a turn that ended since the last look,
    /// in the session being looked at, is seen.
    #[test]
    fn the_list_uses_the_last_presence_sample() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let now = now_ms();
        add(&d.db(), "a", Status::Working, now - 5_000);
        let there = || vec![client("t-a", true)];
        look(&d, now - 1_000, &there, false, true).unwrap();
        // A look that asks nobody: `viewing` emptied, the sample kept.
        look(&d, now - 900, &Vec::new, false, false).unwrap();
        assert!(d.attention.state().viewing.is_empty());
        // Its turn ends after that, with no look since.
        {
            let db = d.db();
            db.set_status("a", Status::Idle, now - 500).unwrap();
            db.touch_last_agent_msg("a", now - 500).unwrap();
        }
        assert_eq!(
            list(&d, &json!({})).unwrap()["your_turn"],
            0,
            "looked at: seen"
        );
        // An old sample doesn't count.
        d.attention.state().sample = Some((now - 120_000, ["a".to_string()].into()));
        assert_eq!(list(&d, &json!({})).unwrap()["your_turn"], 1);
    }

    /// Errors are told apart by kind, not text: two of a kind, one warning.
    #[test]
    fn errors_are_kinds_not_texts() {
        let a = anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::NotFound, "/a"));
        let b = anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::NotFound, "/b"));
        assert_eq!(error_kind(&a), error_kind(&b));
        assert_eq!(error_kind(&anyhow::anyhow!("x {}", 1)), "other");
    }

    /// An entry the log can't read is closed at the first start that sees
    /// it, so the next start doesn't warn about it again.
    #[test]
    fn an_unreadable_entry_is_closed_once() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        {
            let db = d.db();
            add(&db, "a", Status::Working, NOW);
            db.insert_event("a", NOW, "attention_entered", "not json")
                .unwrap();
            db.insert_event(
                "a",
                NOW,
                "attention_entered",
                r#"{"kind": "bogus", "since": 1}"#,
            )
            .unwrap();
            // A kind that isn't a string at all.
            db.insert_event("a", NOW, "attention_entered", r#"{"kind": 5, "since": 1}"#)
                .unwrap();
        }
        assert_eq!(d.db().open_attention_entries().unwrap().len(), 3);
        look(&d, NOW + 1_000, &Vec::new, true, false).unwrap();
        assert!(d.db().open_attention_entries().unwrap().is_empty());
    }

    #[test]
    fn a_turn_is_announced_once() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let t0 = NOW;
        add(&d.db(), "a", Status::Working, t0);
        let none = Vec::new;
        look(&d, t0 + 1_000, &none, true, false).unwrap();
        // Its turn ends: announced at the settle time.
        let t1 = t0 + 10_000;
        {
            let db = d.db();
            db.set_status("a", Status::Idle, t1).unwrap();
            db.touch_last_agent_msg("a", t1).unwrap();
        }
        look(&d, t1 + 500, &none, true, false).unwrap();
        look(&d, t1 + 10_500, &none, true, false).unwrap();
        // Working a moment with no new turn end, idle again: a new entry
        // (since the status change), the same turn.
        let t2 = t1 + 20_000;
        d.db().set_status("a", Status::Working, t2).unwrap();
        look(&d, t2 + 100, &none, true, false).unwrap();
        d.db().set_status("a", Status::Idle, t2 + 1_000).unwrap();
        look(&d, t2 + 1_500, &none, true, false).unwrap();
        look(&d, t2 + 11_500, &none, true, false).unwrap();
        let settled: Vec<Value> = shadow(&d, "a")
            .into_iter()
            .filter(|(k, _)| k == "attention_settled")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(settled.len(), 2);
        assert_eq!(settled[0]["announced"], true);
        assert_eq!(
            (&settled[1]["announced"], &settled[1]["repeat"]),
            (&json!(false), &json!(true))
        );
    }

    #[test]
    fn codex_cant_tell_only_when_its_hooks_are_untrusted() {
        let none = HashSet::new();
        let untrusted = Blind {
            codex_untrusted: true,
            ..Default::default()
        };
        let row = |status: &str, conv: Value, trust: &str| {
            json!({"id": "c", "agent": "codex", "status": status, "tmux_session": "c",
                   "archived_at": null, "agent_session": conv, "agent_session_trust": trust})
        };
        let known = |r: Value, b: Blind| input_of(&r, &none, None, b).needs_you_known;
        assert!(!known(row("working", json!("t-1"), "index"), untrusted));
        // Its hooks reached it, or they're trusted: known.
        assert!(known(row("working", json!("t-1"), "hook"), untrusted));
        assert!(known(
            row("working", json!("t-1"), "index"),
            Blind::default()
        ));
        // Nothing to tell yet: starting, or no conversation bound.
        assert!(known(row("starting", json!("t-1"), "index"), untrusted));
        assert!(known(row("idle", Value::Null, "minted"), untrusted));
    }

    /// A session that can't be told for a while: its needs-you is kept as
    /// it was (a `blind` marker, no clear, no new entry, no announcement)
    /// and goes on when it can be told again.
    #[test]
    fn a_blind_spell_suspends_an_entry() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let t0 = NOW;
        add_as(&d.db(), "w", "opencode", Status::Working, t0);
        let none = Vec::new;
        look(&d, t0 + 500, &none, true, false).unwrap();
        // Needs you; settles; would be announced.
        d.db()
            .set_status("w", Status::WaitingInput, t0 + 1_000)
            .unwrap();
        look(&d, t0 + 1_500, &none, true, false).unwrap();
        look(&d, t0 + 4_500, &none, true, false).unwrap();
        let kinds =
            |d: &Daemon| -> Vec<String> { shadow(d, "w").into_iter().map(|(k, _)| k).collect() };
        let before = kinds(&d);
        assert_eq!(before, ["attention_entered", "attention_settled"]);
        // OpenCode's server out of reach: suspended, one marker.
        d.opencode_blind.store(true, Ordering::Relaxed);
        look(&d, t0 + 10_000, &none, true, false).unwrap();
        look(&d, t0 + 11_000, &none, true, false).unwrap();
        let during = kinds(&d);
        assert_eq!(&during[2..], ["attention_blind"], "one marker, no clear");
        // Told again: goes on as it was.
        d.opencode_blind.store(false, Ordering::Relaxed);
        look(&d, t0 + 12_000, &none, true, false).unwrap();
        assert_eq!(kinds(&d), during, "no entered, no settled again");
        // Answered: cleared once, as announced.
        d.db()
            .set_status("w", Status::Working, t0 + 13_000)
            .unwrap();
        look(&d, t0 + 13_500, &none, true, false).unwrap();
        let last = shadow(&d, "w").last().unwrap().1.clone();
        assert_eq!(
            (&last["kind"], &last["announced"]),
            (&json!("needs_you"), &json!(true))
        );
    }

    /// A poke resamples who is looking even with nothing settling or
    /// unseen; with no focused client, nobody is.
    #[test]
    fn a_poke_resamples_presence() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        add(&d.db(), "a", Status::Working, NOW);
        let there = || vec![client("t-a", true)];
        let away = || vec![client("t-a", false)];
        look(&d, NOW, &there, false, true).unwrap();
        assert!(d.attention.state().viewing.contains("a"));
        look(&d, NOW + 1_000, &away, false, true).unwrap();
        assert!(d.attention.state().viewing.is_empty());
        // Not asked: nobody counts as looking.
        look(&d, NOW + 2_000, &there, false, true).unwrap();
        look(&d, NOW + 3_000, &there, false, false).unwrap();
        assert!(d.attention.state().viewing.is_empty());
    }

    /// tmux's client hooks poke the watcher: the next tick looks.
    #[test]
    fn a_poke_asks_for_a_look() {
        let w = Watch::default();
        assert!(!w.moved.swap(false, Ordering::Relaxed));
        w.poke();
        assert!(w.moved.swap(false, Ordering::Relaxed));
    }

    /// A restart (going idle, no new turn end) makes no your-turn; nor
    /// does a turn's end the user already answered with a prompt.
    #[test]
    fn going_idle_alone_is_no_turn() {
        let mut r = row("r", "idle");
        r.agent_msg_at = Some(NOW - 600_000);
        r.state_since = Some(NOW - 1_000);
        r.last_prompt_at = Some(NOW - 300_000);
        assert!(
            entries(&[r.clone()], NOW).is_empty(),
            "prompted since the turn ended"
        );
        // Not prompted since: the turn's end stands, dated from it (not
        // from going idle).
        r.last_prompt_at = Some(NOW - 700_000);
        let e = &entries(&[r], NOW)[0];
        assert_eq!(e.since, NOW - 600_000);
    }

    #[test]
    fn the_tool_answer_counts_and_clips() {
        let mut rows: Vec<Input> = (0..5).map(|i| row(&format!("s{i}"), "idle")).collect();
        rows[0].status = "waiting_input".into();
        let mut cx = row("cx", "working");
        cx.needs_you_known = false;
        cx.unknown_why = Some("Codex hooks not trusted yet");
        rows.push(cx);
        let v = shape(&rows, NOW, 2);
        assert_eq!(
            (v["needs_you"].as_u64(), v["your_turn"].as_u64()),
            (Some(1), Some(4))
        );
        assert_eq!(v["queue"].as_array().unwrap().len(), 2);
        assert_eq!(v["more"], 3);
        assert_eq!(v["queue"][0]["waited"], "1m");
        assert_eq!(v["cant_tell"][0]["why"], "Codex hooks not trusted yet");
    }
}
