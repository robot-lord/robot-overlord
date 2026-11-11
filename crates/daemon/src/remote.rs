//! Remote Control for rlord's sessions: continue any Claude Code session
//! from the Claude app or claude.ai while it keeps running here.
//!
//! - **The wish** is per session, in its launch spec (`remote_control`: the
//!   name it shows in the app). Every start, resume and fresh start passes
//!   `--remote-control <name>`, so it survives restarts and resets.
//! - **A running session** is connected by typing `/remote-control <name>`
//!   into it, only while Claude reports it idle and its input line is empty
//!   (never over the user's typing). Turning it off picks "Disconnect this
//!   session" in the same command's panel.
//! - **The keeper** (every few seconds) reconnects sessions that should be
//!   connected and aren't (a dropped connection needs the command again), and
//!   gives up with a reason after a few tries: an org that hasn't allowed
//!   Remote Control just never connects, and rlord says so instead of
//!   retrying forever.
//! - **Availability** is checked up front (`remote::check`, cached) so the
//!   overlord and the list say plainly when this machine can't use it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::remote::{self, Availability, Live};
use rlord_core::db::{Db, SessionQuery};
use rlord_core::tmux;
use rlord_core::types::Session;

use crate::{overlord, Daemon};

/// How often the keeper looks.
const TICK: Duration = Duration::from_secs(5);
/// Between two attempts to connect one session.
const RETRY: Duration = Duration::from_secs(40);
/// A process this young may still be connecting from its launch flag.
const SETTLE_MS: i64 = 30_000;
/// Attempts before rlord says it didn't connect.
const TRIES: u32 = 3;
/// After giving up, try again this much later (a network blip, a fixed org
/// setting).
const GIVE_UP_FOR: Duration = Duration::from_secs(30 * 60);
/// Availability is re-checked this often (a login or plan change).
const RECHECK: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub struct Remote {
    avail: Mutex<Option<(Instant, Availability)>>,
    track: Mutex<HashMap<String, Track>>,
    /// Last state per session, for the list (cheap to annotate).
    snapshot: Mutex<HashMap<String, Value>>,
    /// One keeper pass at a time (the keeper and a `remote.set`).
    tick: Mutex<()>,
}

#[derive(Default)]
struct Track {
    tries: u32,
    last_try: Option<Instant>,
    failed: Option<(Instant, String)>,
    /// rlord turned it off: disconnect when it's next idle.
    disconnect: bool,
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

fn wanted(db: &Db, id: &str) -> Result<Option<String>> {
    let spec: Value = serde_json::from_str(&db.launch_spec(id)?).unwrap_or(json!({}));
    Ok(spec["remote_control"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_string))
}

fn set_wanted(db: &Db, s: &Session, on: bool) -> Result<()> {
    let mut spec: Value = serde_json::from_str(&db.launch_spec(&s.id)?).unwrap_or(json!({}));
    if on {
        spec["remote_control"] = json!(remote_name(&s.name, &s.role));
    } else if let Some(o) = spec.as_object_mut() {
        o.remove("remote_control");
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
    remote::live_sessions()
        .into_iter()
        .filter(|(_, l)| pid_alive(l.pid))
        .collect()
}

/// `{state, url?, reason?, wanted}` for one session.
fn state_of(
    s: &Session,
    want: Option<&str>,
    live: Option<&Live>,
    track: Option<&Track>,
    avail: &Availability,
) -> Value {
    let url = live.and_then(Live::url);
    let (state, reason): (&str, Option<String>) = if url.is_some() {
        ("on", None)
    } else if want.is_none() {
        ("off", None)
    } else if s.agent != "claude" {
        (
            "unavailable",
            Some(format!("{} has no Remote Control", s.agent)),
        )
    } else if !avail.available {
        ("unavailable", avail.reason.clone())
    } else if let Some((_, why)) = track.and_then(|t| t.failed.as_ref()) {
        ("failed", Some(why.clone()))
    } else if live.is_none() && s.status == rlord_core::types::Status::Stopped {
        // Not running: it connects when it starts.
        ("stopped", None)
    } else {
        ("connecting", None)
    };
    json!({"state": state, "url": url, "reason": reason, "wanted": want.is_some(),
           "name": want})
}

/// Whether the agent's input line is empty (nothing the user is typing):
/// the last line that starts with the prompt mark, minus the placeholder.
fn input_empty(screen: &str) -> bool {
    // Vim mode's NORMAL: typed text would run as commands.
    if screen
        .lines()
        .rev()
        .take(4)
        .any(|l| l.contains("-- NORMAL --"))
    {
        return false;
    }
    let Some(line) = screen.lines().rev().find(|l| l.starts_with('❯')) else {
        return false;
    };
    let rest = line.trim_start_matches('❯').trim();
    rest.is_empty() || rest.starts_with("Try \"")
}

/// The text on the agent's input line (after the prompt mark).
fn input_line(screen: &str) -> Option<String> {
    screen
        .lines()
        .rev()
        .find(|l| l.starts_with('❯'))
        .map(|l| l.trim_start_matches('❯').trim().to_string())
}

/// Type a slash command into an idle session, and submit it only if the
/// input line then holds exactly that command: if the user typed meanwhile,
/// take ours back out and leave theirs alone.
fn type_command(pane: &str, cmd: &str) -> Result<()> {
    tmux::send_text(pane, cmd)?;
    // Let the command menu settle before submitting.
    std::thread::sleep(Duration::from_millis(400));
    let line = input_line(&tmux::capture_pane(pane)?).unwrap_or_default();
    if line != cmd {
        // Ours is last (theirs came first): backspace it away. Otherwise
        // theirs follows ours; leave the line for them to fix.
        if line.ends_with(cmd) {
            let n = cmd.chars().count().to_string();
            tmux::send_keys(pane, &["-N", &n, "BSpace"])?;
        }
        bail!("the input line changed while typing; not submitted");
    }
    tmux::send_keys(pane, &["Enter"])
}

/// Open `/remote-control`'s panel and pick "Disconnect this session".
fn disconnect(pane: &str) -> Result<()> {
    type_command(pane, "/remote-control")?;
    let item = "Disconnect this session";
    let deadline = Instant::now() + Duration::from_secs(4);
    let screen = loop {
        let sc = tmux::capture_pane(pane)?;
        if sc.contains(item) {
            break sc;
        }
        if Instant::now() > deadline {
            // Opened late after all? Close it rather than leave it to
            // catch the user's keys.
            if tmux::capture_pane(pane).is_ok_and(|sc| sc.contains(item)) {
                tmux::send_keys(pane, &["Escape"])?;
            }
            bail!("the Remote Control panel didn't open");
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    // Menu rows: the selected one carries the mark. Move up to the item.
    let rows: Vec<&str> = screen.lines().collect();
    let at = |pred: &dyn Fn(&str) -> bool| rows.iter().rposition(|l| pred(l));
    let target = at(&|l| l.contains(item)).context("no disconnect item")?;
    let selected = at(&|l| l.trim_start().starts_with('❯') && l.starts_with(' '))
        .context("no selection in the panel")?;
    // Only option rows sit between them (blank lines too), count those.
    let steps = rows[target.min(selected)..target.max(selected)]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .count();
    let key = if selected > target { "Up" } else { "Down" };
    for _ in 0..steps {
        tmux::send_keys(pane, &[key])?;
    }
    std::thread::sleep(Duration::from_millis(200));
    let check = tmux::capture_pane(pane)?;
    if !check
        .lines()
        .any(|l| l.contains(item) && l.trim_start().starts_with('❯'))
    {
        tmux::send_keys(pane, &["Escape"])?;
        bail!("couldn't select \"{item}\"");
    }
    tmux::send_keys(pane, &["Enter"])
}

/// One keeper pass: connect what should be, disconnect what rlord turned
/// off, refresh the list's view. Returns each session's state.
fn pass(d: &Daemon) -> Result<HashMap<String, Value>> {
    let _one = d.remote.tick.lock().expect("remote tick");
    let sessions = d.db().sessions(&SessionQuery::default())?;
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
    let any_wanted = !wants.is_empty();
    // Checked when someone wants it (cached; the keeper's first pass warms
    // it for the setup dialog and the offer).
    let avail = if any_wanted || d.remote.cached().is_none() {
        d.remote.availability(false)
    } else {
        d.remote.cached().expect("cached")
    };
    let mut out = HashMap::new();
    let mut track = d.remote.track.lock().expect("remote mutex");
    for s in &sessions {
        let want = wants.get(&s.id).map(String::as_str);
        let l = s.agent_session.as_deref().and_then(|a| live.get(a));
        let connected = l.is_some_and(|l| l.bridge.is_some());
        let t = track.entry(s.id.clone()).or_default();
        if connected && want.is_some() {
            *t = Track::default();
        }
        if let Some((at, _)) = &t.failed {
            if at.elapsed() > GIVE_UP_FOR {
                *t = Track::default();
            }
        }
        // Only an idle agent with an empty input line takes a command.
        let now = rlord_core::types::now_ms();
        let ready = |l: &Live| {
            l.status == "idle"
                // Just started (maybe with the flag): give it time to connect.
                && now - l.started_at > SETTLE_MS
                && s.tmux_session
                    .as_deref()
                    .and_then(|p| tmux::capture_pane(p).ok())
                    .is_some_and(|sc| input_empty(&sc))
        };
        let pane = s.tmux_session.clone();
        match (want, l, pane) {
            (Some(name), Some(l), Some(pane))
                if !connected
                    && s.agent == "claude"
                    && avail.available
                    && t.failed.is_none()
                    && t.last_try.is_none_or(|at| at.elapsed() > RETRY) =>
            {
                if t.tries >= TRIES {
                    t.failed = Some((
                        Instant::now(),
                        if avail.note.is_some() {
                            "didn't connect: your organization may not allow Remote Control (an admin turns it on at claude.ai/admin-settings/claude-code)".into()
                        } else {
                            "didn't connect after a few tries (offline, or Claude's servers unreachable); rlord tries again later".into()
                        },
                    ));
                } else if ready(l) {
                    t.tries += 1;
                    t.last_try = Some(Instant::now());
                    if let Err(e) = type_command(&pane, &format!("/remote-control {name}")) {
                        tracing::debug!(session = %s.id, error = %e, "remote control: connect");
                    }
                }
            }
            (None, Some(l), Some(pane)) if t.disconnect => {
                if !connected {
                    t.disconnect = false;
                } else if ready(l) && t.last_try.is_none_or(|at| at.elapsed() > RETRY) {
                    t.last_try = Some(Instant::now());
                    t.tries += 1;
                    if let Err(e) = disconnect(&pane) {
                        tracing::debug!(session = %s.id, error = %e, "remote control: disconnect");
                    }
                    if t.tries >= TRIES {
                        t.disconnect = false;
                    }
                }
            }
            _ => {}
        }
        let st = state_of(s, want, l, Some(t), &avail);
        if st["state"] != "off" {
            out.insert(s.id.clone(), st);
        }
    }
    // Forget sessions that are gone.
    track.retain(|id, _| sessions.iter().any(|s| &s.id == id));
    drop(track);
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

/// `remote.status {ref?}`: one session's state, or every session that has
/// Remote Control on or wanted, plus this machine's availability.
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
        .sessions(&SessionQuery::default())?
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect();
    let list: Vec<Value> = states
        .into_iter()
        .map(|(id, st)| json!({"session": id, "name": names.get(&id), "remote": st}))
        .collect();
    Ok(json!({"availability": avail_json(&avail), "sessions": list}))
}

/// `remote.set {ref, on}`: turn Remote Control on or off for a session. On:
/// remembered for every future start, and connected now if it's running
/// (waits briefly for the link). The overlord's two seats go together.
pub fn set(d: &Daemon, params: &Value) -> Result<Value> {
    let on = params["on"]
        .as_bool()
        .context("missing 'on' (true or false)")?;
    let s = d
        .db()
        .resolve_ref(params["ref"].as_str().context("missing 'ref'")?)?;
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
    let mut targets = vec![s.clone()];
    if overlord::is_overlord_role(&s.role) {
        for seat in [&overlord::FRONT, &overlord::MASTERMIND] {
            if let Some(o) = seat.session(&d.db())? {
                if o.id != s.id {
                    targets.push(o);
                }
            }
        }
    }
    // The user said no for the overlord: don't offer it again.
    if !on && overlord::is_overlord_role(&s.role) {
        d.db().kv_set("remote_offer_declined", "1")?;
    }
    for t in &targets {
        set_wanted(&d.db(), t, on)?;
        let mut tr = d.remote.track.lock().expect("remote mutex");
        let e = tr.entry(t.id.clone()).or_default();
        *e = Track {
            disconnect: !on,
            ..Track::default()
        };
    }
    d.changed();
    // Give a running session a moment to connect (or let go), so the answer
    // can carry the link.
    let wait = if params["wait"] == false {
        0
    } else if on {
        12
    } else {
        6
    };
    let deadline = Instant::now() + Duration::from_secs(wait);
    let st = loop {
        let states = pass(d)?;
        let st = states
            .get(&s.id)
            .cloned()
            .unwrap_or_else(|| json!({"state": "off", "wanted": false}));
        let settled = match st["state"].as_str() {
            Some("connecting") => false,
            Some("on") => on,
            _ => true,
        };
        if settled || Instant::now() > deadline {
            break st;
        }
        std::thread::sleep(Duration::from_millis(700));
    };
    let note = match (on, st["state"].as_str()) {
        (true, Some("on")) => "connected: open it in the Claude app (Code) or at the url",
        (true, Some("stopped")) => "it connects when the session next starts",
        (true, Some("connecting")) => {
            "it connects as soon as the session is idle with an empty input line"
        }
        (false, Some("on")) => "it disconnects as soon as the session is idle",
        (false, _) => "off; future starts won't connect it",
        _ => "",
    };
    let avail = d.remote.availability(false);
    Ok(
        json!({"session": s.id, "name": s.name, "remote": st, "note": note,
              "caveat": avail.note}),
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
        assert!(input_empty("hello\n❯ \n──\n"));
        assert!(input_empty("❯ Try \"write a test\"\n"));
        assert!(!input_empty("❯ half a messa\n"));
        // The last prompt line counts (earlier ones are history).
        assert!(input_empty("❯ /remote-control\n  done\n❯ \n"));
        assert!(!input_empty("no prompt here"));
        assert!(!input_empty("❯ \n──\n  -- NORMAL -- ⏵⏵ auto mode\n"));
        assert!(input_empty("❯ \n──\n  -- INSERT -- ⏵⏵ auto mode\n"));
    }
}
