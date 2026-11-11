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
/// How long a session started with the flag may take to connect.
const SETTLE_MS: i64 = 45_000;
/// Availability is re-checked this often (a login or plan change).
const RECHECK: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub struct Remote {
    avail: Mutex<Option<(Instant, Availability)>>,
    /// Sessions to restart (onto their current wish) once nothing is going
    /// on in them.
    restart_when_idle: Mutex<HashSet<String>>,
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
                if at.elapsed() < RECHECK {
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

/// The text on the agent's input line (after the prompt mark), read from
/// its screen (never typed into). Claude's placeholder counts as empty.
fn input_text(screen: &str) -> Option<String> {
    let line = screen.lines().rev().find(|l| l.starts_with('❯'))?;
    let rest = line.trim_start_matches('❯').trim();
    (!rest.is_empty() && !rest.starts_with("Try \"")).then(|| rest.to_string())
}

/// How many shells the agent process has running (its background
/// commands; MCP servers and helpers aren't shells).
fn background_shells(pid: u32) -> usize {
    std::process::Command::new("pgrep")
        .args(["-l", "-P", &pid.to_string()])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.split_whitespace().nth(1))
                .filter(|name| {
                    let base = name.rsplit('/').next().unwrap_or(name);
                    matches!(base, "zsh" | "bash" | "sh" | "fish" | "dash")
                })
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
        .and_then(|p| tmux::capture_pane_id(&p).ok())
        .and_then(|sc| input_text(&sc));
    if draft.is_some() {
        why.push("there's unsent text in its input".into());
    }
    why
}

/// Whether the running process matches the wish (connected iff wanted,
/// started after the wish was set).
fn running_matches(want: Option<&(String, i64)>, l: &Live) -> bool {
    match want {
        Some((_, at)) => l.bridge.is_some() || l.started_at >= *at,
        None => l.bridge.is_none(),
    }
}

/// `{state, url?, reason?, wanted, name}` for one session.
fn state_of(
    s: &Session,
    want: Option<&(String, i64)>,
    live: Option<&Live>,
    avail: &Availability,
    restart_queued: bool,
    now: i64,
) -> Value {
    let url = live.and_then(Live::url);
    let (state, reason): (&str, Option<String>) = match (want, live) {
        _ if url.is_some() && want.is_none() => ("disconnecting", None),
        _ if url.is_some() => ("on", None),
        (None, _) => ("off", None),
        _ if s.agent != "claude" => (
            "unavailable",
            Some(format!("{} has no Remote Control", s.agent)),
        ),
        _ if !avail.available => ("unavailable", avail.reason.clone()),
        (Some(_), None) => ("stopped", None),
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

/// Restart a session onto its current wish (resuming the conversation).
fn restart(d: &Daemon, id: &str) -> Result<()> {
    // Not while a hand-off is being pasted into the mastermind.
    let is_mastermind = overlord::MASTERMIND
        .session(&d.db())?
        .is_some_and(|m| m.id == id);
    let _send = is_mastermind.then(|| d.mastermind_send.lock().expect("mastermind send lock"));
    methods::session_restart(&d.db(), &json!({"ref": id}))?;
    d.changed();
    Ok(())
}

/// One keeper pass: run deferred restarts whose sessions went quiet, and
/// refresh each session's state for the list. Returns the states.
fn pass(d: &Daemon) -> Result<HashMap<String, Value>> {
    let _one = d.remote.tick.lock().expect("remote tick");
    let sessions = d.db().sessions(&all_sessions())?;
    let live = live();
    let mut wants = HashMap::new();
    {
        let db = d.db();
        for s in &sessions {
            if let Some(w) = wanted(&db, &s.id)? {
                wants.insert(s.id.clone(), w);
            }
        }
    }
    // Checked when someone wants it (cached; the keeper's first pass warms
    // it for the setup dialog and the offer).
    let avail = match d.remote.cached() {
        Some(a) if wants.is_empty() => a,
        _ => d.remote.availability(false),
    };
    let now = rlord_core::types::now_ms();
    let queued: Vec<String> = d
        .remote
        .restart_when_idle
        .lock()
        .expect("remote mutex")
        .iter()
        .cloned()
        .collect();
    for id in queued {
        let Some(s) = sessions.iter().find(|s| s.id == id) else {
            d.remote
                .restart_when_idle
                .lock()
                .expect("remote mutex")
                .remove(&id);
            continue;
        };
        let l = s.agent_session.as_deref().and_then(|a| live.get(a));
        let done = l.is_none_or(|l| running_matches(wants.get(&id), l));
        if done {
            d.remote
                .restart_when_idle
                .lock()
                .expect("remote mutex")
                .remove(&id);
        } else if work_going(s, l).is_empty() {
            d.remote
                .restart_when_idle
                .lock()
                .expect("remote mutex")
                .remove(&id);
            if let Err(e) = restart(d, &id) {
                tracing::warn!(session = %id, error = %e, "remote control: restart");
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
        let st = state_of(s, want, l, &avail, queued.contains(&s.id), now);
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
    std::thread::spawn(move || loop {
        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pass(&d)))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("panicked")))
        {
            tracing::debug!(error = %e, "remote control keeper");
        }
        std::thread::sleep(TICK);
    });
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
        Some(seat) => overlord::seat(seat)
            .session(&d.db())?
            .context("no overlord yet")?,
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
    let mut targets = vec![s.clone()];
    if seats {
        for seat in [&overlord::FRONT, &overlord::MASTERMIND] {
            if let Some(o) = seat.session(&d.db())? {
                if o.id != s.id {
                    targets.push(o);
                }
            }
        }
    }
    let live = live();
    // What a restart now would interrupt (the overlord's seats never restart
    // now: the overlord is mid-reply while it asks for this).
    let lv = |t: &Session| t.agent_session.as_deref().and_then(|a| live.get(a));
    let running = |t: &Session| lv(t).is_some();
    let going: Vec<String> = if seats {
        Vec::new()
    } else {
        work_going(&s, lv(&s))
    };
    if running(&s) && !going.is_empty() && mode.is_none() {
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
    for t in &targets {
        let before = wanted(&d.db(), &t.id)?;
        let connected = lv(t).is_some_and(|l| l.bridge.is_some());
        // Already as asked: nothing to change or restart.
        if before.is_some() == on && (connected == on || !running(t)) {
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
            d.remote
                .restart_when_idle
                .lock()
                .expect("remote mutex")
                .insert(t.id.clone());
            deferred.push(t.name.clone());
        } else {
            restart(d, &t.id)?;
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
            "{} restart(s) as soon as its current turn is done (the conversation resumes)",
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
    let on = front
        .as_ref()
        .is_some_and(|f| wanted(&db, &f.id).ok().flatten().is_some());
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
    }

    #[test]
    fn running_process_against_the_wish() {
        let l = |bridge: Option<&str>, started_at| Live {
            pid: 1,
            status: "idle".into(),
            bridge: bridge.map(str::to_string),
            started_at,
        };
        let want = ("x".to_string(), 100);
        assert!(
            running_matches(Some(&want), &l(None, 150)),
            "started with the flag"
        );
        assert!(
            !running_matches(Some(&want), &l(None, 50)),
            "started before the wish"
        );
        assert!(running_matches(Some(&want), &l(Some("b"), 50)));
        assert!(!running_matches(None, &l(Some("b"), 50)), "still connected");
        assert!(running_matches(None, &l(None, 50)));
    }
}
