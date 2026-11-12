//! Remote Control for rlord's sessions: continue any Claude Code session
//! from the Claude app or claude.ai while it keeps running here.
//!
//! rlord never types into a session. Remote Control is a launch flag:
//!
//! - **The wish** is per session, in its launch spec (`remote_control`: the
//!   name it shows in the app; `remote_control_at`: when it was set). Every
//!   start, resume and fresh start passes `--remote-control <name>`.
//! - **Turning it on or off** in a running session restarts it (resuming the
//!   conversation) with the flag, or with `disableRemoteControl` in a
//!   `--settings` override (a plain `--resume` reconnects a conversation that
//!   was last connected). A session with work going (its
//!   turn running, sub-agents, a permission prompt, unsent text in its input)
//!   isn't restarted unasked: `remote.set` answers `needs_confirm` with the
//!   reasons, and the caller picks `restart: "now"` or `"when_idle"` (the
//!   keeper restarts it as soon as nothing is going on). The overlord's own
//!   seats always wait for idle: restarting mid-reply would cut it off.
//! - **The keeper** (every few seconds) runs those deferred restarts and
//!   works out each session's state from Claude's own live-state files. A
//!   session started with the flag that never connects is reported with the
//!   likely reason (an org that hasn't allowed it) rather than retried.
//! - **Availability** is checked up front (`remote::check`, cached) so the
//!   overlord and the list say plainly when this machine can't use it.

use std::collections::{HashMap, HashSet};

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::remote::{self, Availability, Live};
use rlord_core::db::{Db, SessionQuery};
use rlord_core::tmux;
use rlord_core::types::{Session, Status};

use crate::{methods, overlord, Daemon};

/// How often the keeper looks.
const TICK: Duration = Duration::from_secs(5);
/// A deferred restart waits for the session to be quiet this long.
const QUIET_MS: i64 = 5_000;
/// How long a session started with the flag may take to connect.
const SETTLE_MS: i64 = 45_000;
/// Availability is re-checked this often (a login or plan change).
const RECHECK: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub struct Remote {
    avail: Mutex<Option<(Instant, Availability)>>,
    /// Sessions to restart (onto their current wish) once nothing is going
    /// on in them.
    /// (When asked, ms: a process started after it has the change.)
    /// Persisted in kv (`remote_restart_queue`): a daemon restart (an
    /// update) doesn't forget it. Loaded on the first pass.
    restart_when_idle: Mutex<HashMap<String, i64>>,
    queue_loaded: std::sync::atomic::AtomicBool,
    /// Last state per session, for the list (cheap to annotate).
    snapshot: Mutex<HashMap<String, Value>>,
    /// One keeper pass at a time (the keeper and a `remote.set`).
    tick: Mutex<()>,
}

/// Every live session, the overlord's two seats included.
fn all_sessions() -> SessionQuery {
    SessionQuery {
        include_overlord: true,
        ..SessionQuery::default()
    }
}

/// kv: whether the mastermind carries Remote Control ("1") — kept so a new
/// or fresh mastermind gets it, and the keeper keeps it running.
pub const MASTERMIND_REMOTE: &str = "mastermind_remote";

/// The name a session shows in the Claude app.
pub fn remote_name(name: &str, role: &str) -> String {
    let n = match role {
        "overlord" | "mastermind" => format!("rlord {role}"),
        _ => name.trim().to_string(),
    };
    // Never mistaken for a flag.
    let n = n.trim_start_matches('-').trim();
    if n.is_empty() {
        "rlord session".into()
    } else {
        n.to_string()
    }
}

impl Remote {
    /// Whether this machine can use Remote Control (cached; `force`
    /// re-checks).
    pub fn availability(&self, force: bool) -> Availability {
        if !force {
            if let Some((at, a)) = self.avail.lock().expect("remote mutex").as_ref() {
                // A check that failed outright is retried within a minute.
                let ttl = if a.transient {
                    Duration::from_secs(60)
                } else {
                    RECHECK
                };
                if at.elapsed() < ttl {
                    return a.clone();
                }
            }
        }
        // Outside the lock: it runs `claude auth status`.
        let a = remote::check();
        *self.avail.lock().expect("remote mutex") = Some((Instant::now(), a.clone()));
        a
    }

    /// The last check, without running one (None before the first).
    pub fn cached(&self) -> Option<Availability> {
        self.avail
            .lock()
            .expect("remote mutex")
            .as_ref()
            .map(|(_, a)| a.clone())
    }

    /// Each session's remote state, for the list.
    pub fn snapshot(&self) -> HashMap<String, Value> {
        self.snapshot.lock().expect("remote mutex").clone()
    }
}

fn avail_json(a: &Availability) -> Value {
    json!({"available": a.available, "reason": a.reason, "note": a.note})
}

/// The wish: the name it connects under, and when it was set (ms).
fn wanted(db: &Db, id: &str) -> Result<Option<(String, i64)>> {
    let spec: Value = serde_json::from_str(&db.launch_spec(id)?).unwrap_or(json!({}));
    Ok(spec["remote_control"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(|n| {
            (
                n.to_string(),
                spec["remote_control_at"].as_i64().unwrap_or(0),
            )
        }))
}

/// rlord turned Remote Control off for it (held off on every launch).
fn turned_off_spec(db: &Db, id: &str) -> bool {
    db.launch_spec(id)
        .ok()
        .and_then(|sp| serde_json::from_str::<Value>(&sp).ok())
        .is_some_and(|sp| sp["remote_control_off"] == true)
}

fn set_wanted(db: &Db, s: &Session, on: bool) -> Result<()> {
    let mut spec: Value = serde_json::from_str(&db.launch_spec(&s.id)?).unwrap_or(json!({}));
    if let Some(o) = spec.as_object_mut() {
        if on {
            o.remove("remote_control_off");
            o.insert(
                "remote_control".into(),
                json!(remote_name(&s.name, &s.role)),
            );
            o.insert(
                "remote_control_at".into(),
                json!(rlord_core::types::now_ms()),
            );
        } else {
            o.remove("remote_control");
            o.remove("remote_control_at");
            // Kept off on resume (see methods::spec_args).
            o.insert("remote_control_off".into(), json!(true));
        }
    }
    db.set_launch_spec(&s.id, &spec.to_string())
}

fn pid_alive(pid: u32) -> bool {
    // Signal 0: exists (EPERM also means it exists).
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Running Claude processes by conversation id (stale files dropped).
fn live() -> HashMap<String, Live> {
    remote::live_sessions(pid_alive)
}

/// The text the user has typed on the agent's input line, read from its
/// screen with attributes (never typed into). Claude draws its placeholder
/// and suggested next prompt dim (`ESC[2m`): those aren't the user's.
fn input_text(screen: &str) -> Option<String> {
    let line = screen
        .lines()
        .rev()
        .find(|l| strip_ansi(l).starts_with('❯'))?;
    let rest = line.split_once('❯')?.1;
    let text = undimmed(rest);
    let text = text.trim_matches([' ', '\u{a0}']);
    // One character is the cursor drawn over a suggestion, not a draft.
    if text.chars().count() <= 1 {
        return None;
    }
    // A placeholder in a capture without attributes.
    (!text.starts_with("Try \"")).then(|| text.to_string())
}

/// The text of `s` drawn without the dim attribute (SGR 2, cleared by 0
/// or 22), however the codes are combined.
fn undimmed(s: &str) -> String {
    let mut out = String::new();
    let mut dim = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            let mut params = String::new();
            let mut fin = ' ';
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    fin = c;
                    break;
                }
                params.push(c);
            }
            if fin == 'm' {
                let ps: Vec<&str> = params.split(';').collect();
                let mut i = 0;
                while i < ps.len() {
                    match ps[i] {
                        "" | "0" | "22" => dim = false,
                        "2" => dim = true,
                        // Extended colours carry their own numbers (a 2 or
                        // 0 in an RGB triple isn't an attribute): skip them.
                        "38" | "48" | "58" => {
                            i += match ps.get(i + 1) {
                                Some(&"5") => 2,
                                Some(&"2") => 4,
                                _ => 0,
                            };
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
        } else if !dim {
            out.push(c);
        }
    }
    out
}

/// Drop ANSI escape sequences (CSI: ESC [ … final byte).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// How many commands the agent has running in its own shells (they
/// source Claude's shell snapshot; MCP servers, even shell-wrapped ones,
/// don't).
fn background_shells(pid: u32) -> usize {
    // `pgrep -P` (children's pids) and `ps -o command= -p` read the same on
    // macOS and procps; `pgrep -l`/`-a` differ between them.
    let Ok(kids) = std::process::Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
    else {
        return 0;
    };
    let pids: Vec<String> = String::from_utf8_lossy(&kids.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if pids.is_empty() {
        return 0;
    }
    std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pids.join(",")])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.contains("shell-snapshots"))
                .count()
        })
        .unwrap_or(0)
}

/// What a restart would interrupt, in words for the user (empty: nothing).
fn work_going(s: &Session, live: Option<&Live>) -> Vec<String> {
    let mut why: Vec<String> = Vec::new();
    match live.map(|l| l.status.as_str()) {
        Some("idle") | None => {}
        Some("waiting") => why.push("it's waiting on you (a prompt or a question)".into()),
        Some("shell") => why.push("a background command it started is running".into()),
        Some(_) => why.push("it's working".into()),
    }
    // Background shells (the agent's run-in-background commands) are the
    // claude process's shell children; a restart would kill them.
    if let Some(l) = live {
        if !why.iter().any(|w| w.contains("background")) && background_shells(l.pid) > 0 {
            why.push("a background command it started is running".into());
        }
    }
    if why.is_empty() && s.status == Status::Working {
        why.push("it's working".into());
    }
    if s.agents_running > 0 {
        why.push(format!("{} sub-agent(s) running", s.agents_running));
    }
    let draft = s
        .tmux_session
        .as_deref()
        .and_then(|n| tmux::pane_id(n).ok())
        .and_then(|p| tmux::capture_pane_id_ansi(&p).ok())
        .and_then(|sc| input_text(&sc));
    if draft.is_some() {
        why.push("there's unsent text in its input".into());
    }
    why
}

/// `{state, url?, reason?, wanted, name}` for one session.
fn state_of(
    s: &Session,
    want: Option<&(String, i64)>,
    live: Option<&Live>,
    avail: &Availability,
    turned_off: bool,
    restart_queued: bool,
    now: i64,
) -> Value {
    let url = live.and_then(Live::url);
    let (state, reason): (&str, Option<String>) = match (want, live) {
        // rlord turned it off; the restart that applies it is still to come.
        _ if url.is_some() && want.is_none() && turned_off => ("disconnecting", None),
        // Connected from inside the session (/remote-control), not by rlord.
        _ if url.is_some() && want.is_none() => {
            ("on", Some("connected from inside the session".into()))
        }
        _ if url.is_some() => ("on", None),
        (None, _) => ("off", None),
        _ if s.agent != "claude" => (
            "unavailable",
            Some(format!("{} has no Remote Control", s.agent)),
        ),
        _ if !avail.available => ("unavailable", avail.reason.clone()),
        (Some(_), None) if s.status == Status::Stopped => ("stopped", None),
        // Between a restart's old process and its new one.
        (Some(_), None) if now - s.last_state_change_at.unwrap_or(now) < SETTLE_MS => {
            ("connecting", None)
        }
        // Running, but Claude Code reports no live state for it.
        (Some(_), None) => (
            "failed",
            Some(
                "Claude Code reports no live state for it (an older version?); update Claude Code"
                    .into(),
            ),
        ),
        (Some((_, at)), Some(l)) if l.started_at < *at => (
            "pending",
            Some(if restart_queued {
                "restarts when its work is done".into()
            } else {
                "needs a restart to connect".into()
            }),
        ),
        (Some(_), Some(l)) if now - l.started_at < SETTLE_MS => ("connecting", None),
        (Some(_), Some(_)) => (
            "failed",
            Some(if avail.note.is_some() {
                "didn't connect: your organization may not allow Remote Control (an admin turns it on at claude.ai/admin-settings/claude-code)".into()
            } else {
                "didn't connect or dropped (offline, or Claude's servers unreachable); turn it on again to restart it".into()
            }),
        ),
    };
    json!({"state": state, "url": url, "reason": reason, "wanted": want.is_some(),
           "name": want.map(|w| &w.0)})
}

const QUEUE_KEY: &str = "remote_restart_queue";

/// Saved while the queue's lock is held, so saves land in order.
fn save_queue(d: &Daemon, q: &HashMap<String, i64>) {
    if let Err(e) = d.db().kv_set(QUEUE_KEY, &json!(q).to_string()) {
        tracing::warn!(error = %e, "remote control: saving the restart queue");
    }
}

/// Bring back deferred restarts saved by an earlier run (once).
fn load_queue(d: &Daemon) {
    use std::sync::atomic::Ordering;
    if d.remote.queue_loaded.swap(true, Ordering::SeqCst) {
        return;
    }
    let saved: HashMap<String, i64> = d
        .db()
        .kv_get(QUEUE_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default();
    let mut q = d.remote.restart_when_idle.lock().expect("remote mutex");
    for (k, v) in saved {
        q.entry(k).or_insert(v);
    }
}

fn enqueue(d: &Daemon, id: &str) {
    // The saved queue first: never overwrite it with a partial one.
    load_queue(d);
    let mut q = d.remote.restart_when_idle.lock().expect("remote mutex");
    q.insert(id.to_string(), rlord_core::types::now_ms());
    save_queue(d, &q);
}

fn dequeue(d: &Daemon, id: &str) {
    load_queue(d);
    let mut q = d.remote.restart_when_idle.lock().expect("remote mutex");
    q.remove(id);
    save_queue(d, &q);
}

/// Restart a session onto its current wish (resuming the conversation).
/// `only_if_quiet`: check again, right before, that nothing is going on
/// (and has been for a few seconds); false means it wasn't restarted.
fn restart(d: &Daemon, id: &str, only_if_quiet: bool) -> Result<bool> {
    // Not while a hand-off is being pasted into the mastermind (and checked
    // under that lock, after any hand-off that held it).
    let is_mastermind = overlord::MASTERMIND
        .session(&d.db())?
        .is_some_and(|m| m.id == id);
    let _send = is_mastermind.then(|| d.mastermind_send.lock().expect("mastermind send lock"));
    if only_if_quiet {
        let s = d.db().resolve_ref(id)?;
        let live = live();
        let l = s.agent_session.as_deref().and_then(|a| live.get(a));
        let now = rlord_core::types::now_ms();
        let quiet = l.is_none_or(|l| now - l.status_at > QUIET_MS)
            && s.last_user_msg_at.is_none_or(|u| now - u > QUIET_MS);
        if !quiet || !work_going(&s, l).is_empty() {
            return Ok(false);
        }
    }
    methods::session_restart(&d.db(), &json!({"ref": id}))?;
    d.changed();
    Ok(true)
}

/// One keeper pass: run deferred restarts whose sessions went quiet, and
/// refresh each session's state for the list. Returns the states.
fn pass(d: &Daemon) -> Result<HashMap<String, Value>> {
    let _one = d.remote.tick.lock().expect("remote tick");
    let sessions = d.db().sessions(&all_sessions())?;
    let live = live();
    let mut wants = HashMap::new();
    let mut turned_off = HashSet::new();
    {
        let db = d.db();
        for s in &sessions {
            if let Some(w) = wanted(&db, &s.id)? {
                wants.insert(s.id.clone(), w);
            } else if turned_off_spec(&db, &s.id) {
                turned_off.insert(s.id.clone());
            }
        }
    }
    // Checked when someone wants it (cached; the keeper's first pass warms
    // it for the setup dialog and the offer).
    // (Re-checked every RECHECK: a login or plan change shows up.)
    let avail = d.remote.availability(false);
    let now = rlord_core::types::now_ms();
    load_queue(d);
    let queued: Vec<(String, i64)> = d
        .remote
        .restart_when_idle
        .lock()
        .expect("remote mutex")
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    for (id, asked_at) in queued {
        let Some(s) = sessions.iter().find(|s| s.id == id) else {
            dequeue(d, &id);
            continue;
        };
        let l = s.agent_session.as_deref().and_then(|a| live.get(a));
        // Done once a process started after the request runs (or none
        // does: the next start applies it).
        // No live state while it's stopped: the next start applies it. A
        // running one without (a file caught mid-write) is checked again.
        let done = match l {
            Some(l) => l.started_at >= asked_at,
            None => s.status == Status::Stopped,
        };
        let remove = |d: &Daemon| dequeue(d, &id);
        if done {
            remove(d);
        } else if work_going(s, l).is_empty() {
            match restart(d, &id, true) {
                Ok(true) => remove(d),
                Ok(false) => {}
                Err(e) => {
                    remove(d);
                    tracing::warn!(session = %id, error = %e, "remote control: restart");
                }
            }
        }
    }
    let queued = d
        .remote
        .restart_when_idle
        .lock()
        .expect("remote mutex")
        .clone();
    let mut out = HashMap::new();
    for s in &sessions {
        let want = wants.get(&s.id);
        let l = s.agent_session.as_deref().and_then(|a| live.get(a));
        let st = state_of(
            s,
            want,
            l,
            &avail,
            turned_off.contains(&s.id),
            queued.contains_key(&s.id),
            now,
        );
        if st["state"] != "off" {
            out.insert(s.id.clone(), st);
        }
    }
    let changed = {
        let mut snap = d.remote.snapshot.lock().expect("remote mutex");
        let changed = *snap != out;
        *snap = out.clone();
        changed
    };
    if changed {
        d.changed();
    }
    Ok(out)
}

/// Start the keeper.
pub fn start(d: &std::sync::Arc<Daemon>) {
    let d = std::sync::Arc::clone(d);
    std::thread::spawn(move || {
        let mut last_ensure: Option<Instant> = None;
        loop {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Each step on its own: one failing never stalls the rest.
                if let Err(e) = move_front_desk_remote(&d) {
                    tracing::warn!(error = %e, "remote control: moving the front desk's");
                }
                if let Err(e) = keep_mastermind_up(&d, &mut last_ensure) {
                    tracing::warn!(error = %e, "remote control: starting the mastermind");
                }
                pass(&d).map(|_| ())
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("panicked")));
            if let Err(e) = r {
                tracing::debug!(error = %e, "remote control keeper");
            }
            std::thread::sleep(TICK);
        }
    });
}

/// An overlord set up when Remote Control lived on the front desk: move it
/// to the mastermind (the front desk is turned off once it's idle).
fn move_front_desk_remote(d: &Daemon) -> Result<()> {
    let Some(front) = overlord::FRONT.session(&d.db())? else {
        return Ok(());
    };
    if wanted(&d.db(), &front.id)?.is_none() {
        return Ok(());
    }
    tracing::info!("moving the overlord's Remote Control to the mastermind");
    // The choice moves to the mastermind whatever happens next, and the
    // front desk's wish goes (so this runs once, not every tick).
    d.db().kv_set(MASTERMIND_REMOTE, "1")?;
    set_wanted(&d.db(), &front, false)?;
    // Its running link (if any) drops when it next restarts idle.
    enqueue(d, &front.id);
    Ok(())
}

/// With Remote Control on the mastermind, keep it running: it's what the
/// user reaches from the Claude app, so it must be there when they're away.
/// (Started lazily otherwise; retried at most once a minute.)
fn keep_mastermind_up(d: &Daemon, last: &mut Option<Instant>) -> Result<()> {
    if d.db().kv_get(MASTERMIND_REMOTE)?.as_deref() != Some("1") {
        return Ok(());
    }
    if overlord::FRONT.session(&d.db())?.is_none() {
        return Ok(()); // no overlord set up
    }
    let up = overlord::MASTERMIND.session(&d.db())?.is_some_and(|m| {
        m.status != Status::Stopped && m.tmux_session.as_deref().is_some_and(tmux::has_session)
    });
    if up || last.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
        return Ok(());
    }
    *last = Some(Instant::now());
    // Not while a hand-off is being pasted into it.
    let _send = d.mastermind_send.lock().expect("mastermind send lock");
    overlord::ensure(&d.db(), &d.config(), &json!({"seat": "mastermind"}))?;
    tracing::info!("started the mastermind for Remote Control");
    d.changed();
    Ok(())
}

/// `remote.availability {force?}`.
pub fn availability(d: &Daemon, params: &Value) -> Result<Value> {
    let a = d
        .remote
        .availability(params["force"].as_bool().unwrap_or(false));
    Ok(avail_json(&a))
}

/// `remote.status {ref?, force?}`: one session's state, or every session
/// that has Remote Control on or wanted, plus this machine's availability.
pub fn status(d: &Daemon, params: &Value) -> Result<Value> {
    let avail = d
        .remote
        .availability(params["force"].as_bool().unwrap_or(false));
    let states = pass(d)?;
    if let Some(r) = params["ref"].as_str() {
        let s = d.db().resolve_ref(r)?;
        let st = states
            .get(&s.id)
            .cloned()
            .unwrap_or_else(|| json!({"state": "off", "wanted": false}));
        return Ok(json!({"session": s.id, "name": s.name, "remote": st,
                         "availability": avail_json(&avail)}));
    }
    let names: HashMap<String, String> = d
        .db()
        .sessions(&all_sessions())?
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect();
    let list: Vec<Value> = states
        .into_iter()
        .map(|(id, st)| json!({"session": id, "name": names.get(&id), "remote": st}))
        .collect();
    Ok(json!({"availability": avail_json(&avail), "sessions": list}))
}

/// `remote.set {ref, on, restart?: "now" | "when_idle"}`: turn Remote
/// Control on or off for a session. Remembered for every future start; a
/// running session is restarted (resumed) with or without the flag. With
/// work going and no `restart` given, nothing changes: the answer is
/// `needs_confirm` with the reasons, for the user to pick. The overlord's
/// two seats go together and always restart when idle.
pub fn set(d: &Daemon, params: &Value) -> Result<Value> {
    let on = params["on"]
        .as_bool()
        .context("missing 'on' (true or false)")?;
    // `seat` (overlord / mastermind) names a seat unambiguously.
    let s = match params["seat"].as_str() {
        // (The mastermind may not have started yet: either seat will do,
        // both lead to it below.)
        Some(seat) => {
            // Bound first: a guard in a match scrutinee lives through the arms.
            let found = overlord::seat(seat).session(&d.db())?;
            match found {
                Some(s) => s,
                None => overlord::FRONT
                    .session(&d.db())?
                    .context("no overlord yet")?,
            }
        }
        None => d
            .db()
            .resolve_ref(params["ref"].as_str().context("missing 'ref'")?)?,
    };
    if on {
        if s.agent != "claude" {
            bail!(
                "Remote Control is a Claude Code feature; {} has none",
                s.agent
            );
        }
        let a = d.remote.availability(true);
        if !a.available {
            bail!(
                "Remote Control isn't available on this machine: {}",
                a.reason.unwrap_or_default()
            );
        }
    }
    let seats = overlord::is_overlord_role(&s.role);
    let mode = match params["restart"].as_str() {
        None | Some("") => None,
        Some(m @ ("now" | "when_idle")) => Some(m),
        Some(m) => bail!("restart must be \"now\" or \"when_idle\", not {m:?}"),
    };
    // The overlord's Remote Control lives on the mastermind (the front desk
    // stays fast; the mastermind is kept running for it). Asked for either
    // seat, it's the mastermind's; the front desk is always turned off.
    let mut started_now = false;
    let (s, mut targets) = if seats {
        d.db()
            .kv_set(MASTERMIND_REMOTE, if on { "1" } else { "0" })?;
        // Bound first: a guard in a match scrutinee lives through the arms
        // (ensure() below locks the db again).
        let found = overlord::MASTERMIND.session(&d.db())?;
        let mind = match found {
            Some(m) => m,
            // Not started yet: start it now (with the flag, from the kv).
            None if on => {
                overlord::ensure(&d.db(), &d.config(), &json!({"seat": "mastermind"}))?;
                started_now = true;
                overlord::MASTERMIND
                    .session(&d.db())?
                    .context("the mastermind didn't start")?
            }
            None => s.clone(),
        };
        let mut t = vec![(mind.clone(), on)];
        if let Some(f) = overlord::FRONT.session(&d.db())? {
            if f.id != mind.id {
                t.push((f, false));
            }
        }
        (mind, t)
    } else {
        (s.clone(), vec![(s, on)])
    };
    // (A front desk that has nothing to turn off is left alone below.)
    targets.retain(|(t, want)| {
        *want || t.role != overlord::FRONT.key || {
            let db = d.db();
            wanted(&db, &t.id).ok().flatten().is_some() || !turned_off_spec(&db, &t.id)
        }
    });
    let live = live();
    // What a restart now would interrupt (the overlord's seats never restart
    // now: the overlord is mid-reply while it asks for this).
    let lv = |t: &Session| t.agent_session.as_deref().and_then(|a| live.get(a));
    // Running: Claude reports it, or its pane is up and it isn't stopped.
    let running = |t: &Session| {
        lv(t).is_some()
            || (t.status != Status::Stopped
                && t.tmux_session.as_deref().is_some_and(tmux::has_session))
    };
    // Whether turning it on/off needs this session restarted at all.
    let needs_restart = |t: &Session| -> Result<bool> {
        let before = wanted(&d.db(), &t.id)?.is_some();
        let connected = lv(t).is_some_and(|l| l.bridge.is_some());
        Ok(running(t) && !(on && connected) && !(before == on && connected == on))
    };
    let going: Vec<String> = if seats || !needs_restart(&s)? {
        Vec::new()
    } else {
        work_going(&s, lv(&s))
    };
    if !going.is_empty() && mode.is_none() {
        return Ok(json!({
            "session": s.id, "name": s.name, "needs_confirm": true,
            "work_going": going,
            "note": format!(
                "turning Remote Control {} restarts {} (the conversation resumes), which would interrupt: {}. Ask the user: restart now (restart: \"now\"), or when its work is done (restart: \"when_idle\")?",
                if on { "on" } else { "off" }, s.name, going.join("; ")
            ),
        }));
    }
    // The user said no for the overlord: don't offer it again.
    if !on && seats {
        d.db().kv_set("remote_offer_declined", "1")?;
    }
    let mut restarted = Vec::new();
    let mut deferred = Vec::new();
    for (t, on) in &targets {
        let (t, on) = (t, *on);
        // Just launched with the flag: nothing to restart.
        if started_now && t.role == overlord::MASTERMIND.key {
            continue;
        }
        let before = wanted(&d.db(), &t.id)?;
        let connected = lv(t).is_some_and(|l| l.bridge.is_some());
        // Already as asked: nothing to restart. Off is still recorded (a
        // link made inside the session would come back on a plain resume).
        if before.is_some() == on && (connected == on || !running(t)) {
            if !on && !turned_off_spec(&d.db(), &t.id) {
                set_wanted(&d.db(), t, false)?;
            }
            continue;
        }
        set_wanted(&d.db(), t, on)?;
        if !running(t) {
            continue;
        }
        // Connected and still wanted: a running link needs no restart.
        if on && connected {
            continue;
        }
        if seats || mode == Some("when_idle") {
            enqueue(d, &t.id);
            deferred.push(t.name.clone());
        } else {
            restart(d, &t.id, false)?;
            restarted.push(t.name.clone());
        }
    }
    d.changed();
    // A restarted session takes a few seconds to connect: wait briefly so
    // the answer can carry the link.
    let wait = if params["wait"] == false || restarted.is_empty() {
        0
    } else {
        15
    };
    let deadline = Instant::now() + Duration::from_secs(wait);
    let st = loop {
        let states = pass(d)?;
        let st = states
            .get(&s.id)
            .cloned()
            .unwrap_or_else(|| json!({"state": "off", "wanted": false}));
        // Just restarted: "pending" until the new process is up.
        let settling = matches!(
            st["state"].as_str(),
            Some("connecting" | "pending" | "disconnecting")
        );
        if !settling || Instant::now() > deadline {
            break st;
        }
        std::thread::sleep(Duration::from_millis(700));
    };
    let note = match (on, st["state"].as_str()) {
        (true, Some("on")) => {
            "connected: open it in the Claude app (Code tab) or at the url".to_string()
        }
        (true, Some("stopped")) => "it connects when the session next starts".into(),
        (_, _) if !deferred.is_empty() => format!(
            "{} restart(s) as soon as its work is done (the conversation resumes)",
            deferred.join(" and ")
        ),
        (true, Some("connecting")) => "restarted; it's connecting".into(),
        (false, _) => "off; future starts won't connect it".into(),
        _ => String::new(),
    };
    let avail = d.remote.cached();
    Ok(
        json!({"session": s.id, "name": s.name, "remote": st, "note": note,
              "restarted": restarted, "restarts_when_idle": deferred,
              "caveat": avail.and_then(|a| a.note)}),
    )
}

/// For `overlord.status`: can this machine use Remote Control, is the
/// overlord on it, and should the user be offered it (once; `declined`
/// remembers a no). Never runs a check itself (the keeper keeps one cached).
pub fn offer(d: &Daemon) -> Value {
    let Some(a) = d.remote.cached() else {
        return Value::Null;
    };
    let db = d.db();
    let front = overlord::FRONT.session(&db).ok().flatten();
    // It lives on the mastermind.
    let on = db.kv_get(MASTERMIND_REMOTE).ok().flatten().as_deref() == Some("1");
    let claude = front.as_ref().is_none_or(|f| f.agent == "claude");
    let declined = db
        .kv_get("remote_offer_declined")
        .ok()
        .flatten()
        .is_some_and(|v| v == "1");
    json!({
        "available": a.available && claude, "reason": a.reason, "note": a.note,
        "on": on,
        "offer": a.available && claude && front.is_some() && !on && !declined,
    })
}

/// Annotate list rows with their remote state.
pub fn annotate(d: &Daemon, rows: &mut [Value]) {
    let snap = d.remote.snapshot();
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        row["remote"] = snap.get(id).cloned().unwrap_or(Value::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_input_line() {
        assert_eq!(remote_name("my-app", ""), "my-app");
        assert_eq!(remote_name("x", "overlord"), "rlord overlord");
        assert_eq!(remote_name("--evil", ""), "evil");
        assert_eq!(remote_name("  ", ""), "rlord session");
        assert_eq!(input_text("hello\n❯ \n──\n"), None);
        assert_eq!(input_text("❯ Try \"write a test\"\n"), None);
        assert_eq!(
            input_text("❯ half a messa\n").as_deref(),
            Some("half a messa")
        );
        // The last prompt line counts (earlier ones are history).
        assert_eq!(input_text("❯ old\n  done\n❯ \n"), None);
        // Claude's suggested next prompt (dim) isn't a draft.
        let suggestion = "\x1b[39m❯\u{a0}\x1b[2mStart `sleep 60` as a task\x1b[0m\n";
        assert_eq!(input_text(suggestion), None);
        assert_eq!(
            input_text("\x1b[39m❯\u{a0}real draft\n").as_deref(),
            Some("real draft")
        );
        // Combined codes and a cursor cell over the suggestion: still dim.
        assert_eq!(input_text("❯ \x1b[0;2mStart a task\x1b[0m\n"), None);
        // RGB colours (whose numbers include 2s and 0s) aren't dim.
        assert_eq!(
            input_text("❯ \x1b[38;2;200;2;0mmy draft\x1b[48;2;0;0;22m here\n").as_deref(),
            Some("my draft here")
        );
        assert_eq!(input_text("❯ \x1b[2;38;5;2mStart a task\n"), None);
        assert_eq!(
            input_text("❯ \x1b[7mS\x1b[27m\x1b[2mtart a task\x1b[22m\n"),
            None
        );
    }
}
