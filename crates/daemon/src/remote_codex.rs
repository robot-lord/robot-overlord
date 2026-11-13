//! Remote Control for rlord's Codex sessions: continue one from the ChatGPT
//! app while it keeps running here (`rlord_core::agents::codex_remote`).
//!
//! Codex's is machine-wide, so it takes three things, and rlord asks for
//! each one separately:
//!
//! - **The machine switch**, on Codex's background server. It lets a paired
//!   phone reach every Codex thread on that server, not just one session's,
//!   so rlord turns it on only when the caller says the user agreed
//!   (`confirm_machine: true`); otherwise `remote.set` answers
//!   `needs_confirm` with `machine_wide`. Switching it never restarts the
//!   server. kv `codex_remote_enabled_by_rlord` remembers that rlord did it.
//! - **A paired phone.** `remote.pair` starts a pairing and has the TUI
//!   show the code (`remote.pair_code`, asked by the TUI or a terminal
//!   `rlord remote pair`). The code is kept in memory only: never in an
//!   answer to the overlord, a log line or an event.
//! - **The session in daemon mode** (`codex_host: "daemon"`): only threads on
//!   the server are reachable. Turning it on moves an in-pane session there
//!   (a restart; the conversation resumes) and marks the spec
//!   `codex_host_by_remote`; turning it off moves such a session back into
//!   its pane. A session in daemon mode by config stays there.
//!
//! The wish is the same spec key as Claude's (`remote_control`,
//! `remote_control_at`); Codex has no per-session flag, so it changes no
//! argument (`methods::spec_args`).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::{json, Value};

use rlord_core::agents::codex_remote::{self, Link, Machine, Pairing};
use rlord_core::agents::remote::Availability;
use rlord_core::agents::{codex, codex_server, install, Agent};
use rlord_core::db::Db;
use rlord_core::tmux;
use rlord_core::types::{Session, Status};

use crate::{overlord, Daemon};

/// How long the machine's state is trusted.
const MACHINE_TTL: Duration = Duration::from_secs(15);
/// Paired devices are asked of ChatGPT (a network call): less often.
const PAIRED_TTL: Duration = Duration::from_secs(60);
/// Whether Codex can have it at all (a login change shows up).
const AVAIL_TTL: Duration = Duration::from_secs(30 * 60);
/// A pairing code without an expiry is dropped after this.
const PAIRING_MAX: Duration = Duration::from_secs(10 * 60);
/// How often a shown code is checked for a phone that used it.
const CLAIM_POLL: Duration = Duration::from_secs(2);

pub(crate) const ENABLED_BY_RLORD: &str = "codex_remote_enabled_by_rlord";

/// What rlord last saw of the machine.
#[derive(Debug, Clone, Default)]
pub(crate) struct Seen {
    /// None: Codex's background server couldn't be asked (not running).
    pub machine: Option<Machine>,
    /// Paired devices (None: not known).
    pub paired: Option<usize>,
}

impl Seen {
    fn link(&self) -> Option<Link> {
        self.machine.as_ref().map(|m| m.link)
    }

    /// A phone can reach daemon-mode threads now.
    fn reachable(&self) -> bool {
        self.link() == Some(Link::Connected) && self.paired != Some(0)
    }
}

struct Pending {
    pairing: Pairing,
    at: Instant,
    claimed: bool,
    checked: Instant,
}

#[derive(Default)]
pub(crate) struct CodexRemote {
    avail: Mutex<Option<(Instant, Availability)>>,
    machine: Mutex<Option<(Instant, Option<Machine>)>>,
    paired: Mutex<Option<(Instant, Option<usize>)>>,
    /// Why enabling last failed (until it works or is switched off).
    last_error: Mutex<Option<String>>,
    pairing: Mutex<Option<Pending>>,
    /// Tests: the stand-in control socket, and a fixed availability.
    socket: Mutex<Option<PathBuf>>,
    fixed: Mutex<Option<Availability>>,
}

impl CodexRemote {
    fn socket(&self) -> PathBuf {
        self.socket
            .lock()
            .expect("codex remote mutex")
            .clone()
            .unwrap_or_else(codex_server::default_socket)
    }

    fn stand_in(&self) -> bool {
        self.socket.lock().expect("codex remote mutex").is_some()
    }

    #[cfg(test)]
    pub(crate) fn use_stand_in(&self, socket: PathBuf, avail: Availability) {
        *self.socket.lock().unwrap() = Some(socket);
        *self.fixed.lock().unwrap() = Some(avail);
    }

    /// Whether this machine's Codex can have Remote Control (cached).
    pub(crate) fn availability(&self, force: bool) -> Availability {
        if let Some(a) = self.fixed.lock().expect("codex remote mutex").clone() {
            return a;
        }
        if !force {
            if let Some((at, a)) = self.avail.lock().expect("codex remote mutex").as_ref() {
                let ttl = if a.transient {
                    Duration::from_secs(60)
                } else {
                    AVAIL_TTL
                };
                if at.elapsed() < ttl {
                    return a.clone();
                }
            }
        }
        let program = install::resolved(Agent::Codex.name());
        let a = codex_remote::check(program.as_deref());
        *self.avail.lock().expect("codex remote mutex") = Some((Instant::now(), a.clone()));
        a
    }

    /// The last availability check, without running one.
    pub(crate) fn cached_availability(&self) -> Option<Availability> {
        if let Some(a) = self.fixed.lock().expect("codex remote mutex").clone() {
            return Some(a);
        }
        self.avail
            .lock()
            .expect("codex remote mutex")
            .as_ref()
            .map(|(_, a)| a.clone())
    }

    /// The machine's state (cached; `force` asks again).
    pub(crate) fn seen(&self, force: bool) -> Seen {
        let machine = {
            let cached = self
                .machine
                .lock()
                .expect("codex remote mutex")
                .as_ref()
                .filter(|(at, _)| !force && at.elapsed() < MACHINE_TTL)
                .map(|(_, m)| m.clone());
            match cached {
                Some(m) => m,
                None => {
                    let m = codex_remote::status(&self.socket()).ok();
                    *self.machine.lock().expect("codex remote mutex") =
                        Some((Instant::now(), m.clone()));
                    m
                }
            }
        };
        let paired = match &machine {
            Some(m) if m.link == Link::Connected => {
                let cached = self
                    .paired
                    .lock()
                    .expect("codex remote mutex")
                    .as_ref()
                    .filter(|(at, _)| !force && at.elapsed() < PAIRED_TTL)
                    .map(|(_, n)| *n);
                match cached {
                    Some(n) => n,
                    None => {
                        let n = codex_remote::paired(&self.socket(), m);
                        *self.paired.lock().expect("codex remote mutex") =
                            Some((Instant::now(), n));
                        n
                    }
                }
            }
            _ => None,
        };
        Seen { machine, paired }
    }

    fn forget(&self) {
        *self.machine.lock().expect("codex remote mutex") = None;
        *self.paired.lock().expect("codex remote mutex") = None;
    }

    /// Why switching it on last failed.
    pub(crate) fn last_error(&self) -> Option<String> {
        self.last_error.lock().expect("codex remote mutex").clone()
    }

    /// Switch it on: Codex's background server first (started if none
    /// runs), then the switch. The error says why, in the user's terms.
    fn enable(&self) -> Result<Machine> {
        if !self.stand_in() {
            let Some(program) = install::resolved(Agent::Codex.name()) else {
                bail!("Codex isn't installed");
            };
            codex_server::ensure(&program)?;
        }
        let r = codex_remote::enable(&self.socket());
        self.forget();
        match &r {
            Ok(_) => *self.last_error.lock().expect("codex remote mutex") = None,
            Err(e) => *self.last_error.lock().expect("codex remote mutex") = Some(format!("{e:#}")),
        }
        r
    }

    fn disable(&self) -> Result<Machine> {
        let r = codex_remote::disable(&self.socket());
        self.forget();
        *self.last_error.lock().expect("codex remote mutex") = None;
        r
    }

    /// Start a pairing; its expiry (ms), if said.
    fn pair_start(&self) -> Result<Option<i64>> {
        let p = codex_remote::pair(&self.socket())?;
        let expires = p.expires_at;
        *self.pairing.lock().expect("codex remote mutex") = Some(Pending {
            pairing: p,
            at: Instant::now(),
            claimed: false,
            checked: Instant::now(),
        });
        Ok(expires)
    }

    /// The pairing on show: (code, expiry, claimed). Asks whether a phone
    /// used it at most every couple of seconds; an expired one is dropped.
    /// Returns whether it was just claimed, too.
    fn pair_peek(&self) -> (Option<(Pairing, bool)>, bool) {
        let mut g = self.pairing.lock().expect("codex remote mutex");
        let now_ms = rlord_core::types::now_ms();
        let expired = g.as_ref().is_some_and(|p| {
            p.pairing
                .expires_at
                .map_or(p.at.elapsed() > PAIRING_MAX, |e| e < now_ms)
                && !p.claimed
        });
        if expired {
            *g = None;
        }
        let Some(p) = g.as_mut() else {
            return (None, false);
        };
        let mut just = false;
        if !p.claimed && p.checked.elapsed() >= CLAIM_POLL {
            p.checked = Instant::now();
            if codex_remote::claimed(&self.socket(), &p.pairing) == Some(true) {
                p.claimed = true;
                just = true;
            }
        }
        let out = Some((p.pairing.clone(), p.claimed));
        drop(g);
        if just {
            // A phone was added: count again.
            *self.paired.lock().expect("codex remote mutex") = None;
        }
        (out, just)
    }

    fn pair_clear(&self) {
        *self.pairing.lock().expect("codex remote mutex") = None;
    }

    /// Whether a pairing is on show (the keeper watches it).
    pub(crate) fn pairing_open(&self) -> bool {
        self.pairing.lock().expect("codex remote mutex").is_some()
    }
}

/// The keeper's look at a shown pairing: the list is told when a phone
/// used it, so the TUI's dialog says so.
pub(crate) fn watch_pairing(d: &Daemon) {
    if !d.remote.codex.pairing_open() {
        return;
    }
    if d.remote.codex.pair_peek().1 {
        tracing::info!("Codex Remote Control: a phone paired");
        d.changed();
    }
}

fn spec_of(db: &Db, id: &str) -> Value {
    db.launch_spec(id)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({}))
}

/// Whether the session's pane is up (a restart would replace it).
fn running(s: &Session) -> bool {
    s.status != Status::Stopped && s.tmux_session.as_deref().is_some_and(tmux::has_session)
}

/// `{state, reason?, wanted, app}` for one Codex session. `want`: it has
/// the wish; `queued`: its restart onto the server waits for its work.
pub(crate) fn state_of(
    s: &Session,
    want: bool,
    seen: &Seen,
    avail: &Availability,
    last_error: Option<&str>,
    queued: bool,
) -> Value {
    let on_daemon = s.codex_daemon;
    let (state, reason): (&str, Option<String>) = if !want {
        // Not asked for, but a phone can reach a thread on the server.
        if on_daemon && s.status != Status::Stopped && seen.reachable() {
            (
                "on",
                Some("reachable: it runs on Codex's background server, which Remote Control is on for".into()),
            )
        } else {
            ("off", None)
        }
    } else if !avail.available {
        ("unavailable", avail.reason.clone())
    } else if s.status == Status::Stopped {
        ("stopped", None)
    } else if queued {
        (
            "pending",
            Some("moves to Codex's background server when its work is done".into()),
        )
    } else {
        match seen.link() {
            None => (
                "failed",
                Some("Codex's background server isn't running".into()),
            ),
            Some(Link::Disabled) => (
                "failed",
                Some("Remote Control is switched off for this machine".into()),
            ),
            Some(Link::Connecting) => ("connecting", None),
            Some(Link::Errored) => (
                "failed",
                Some(last_error.map(str::to_string).unwrap_or_else(|| {
                    "this machine isn't connected to ChatGPT (offline, or ChatGPT refused it)"
                        .into()
                })),
            ),
            Some(Link::Connected) if seen.paired == Some(0) => (
                "unpaired",
                Some("no phone is paired with this machine yet".into()),
            ),
            Some(Link::Connected) => ("on", None),
        }
    };
    json!({"state": state, "reason": reason, "wanted": want,
           "app": Agent::Codex.remote_app()})
}

/// This machine's switch, for `remote.status` and `rlord remote`.
pub(crate) fn machine_json(d: &Daemon, seen: &Seen, avail: &Availability) -> Value {
    let by_rlord = d
        .db()
        .kv_get(ENABLED_BY_RLORD)
        .ok()
        .flatten()
        .is_some_and(|v| v == "1");
    json!({
        "available": avail.available, "reason": avail.reason, "note": avail.note,
        "machine": seen.link().map(Link::name).unwrap_or("server not running"),
        "paired": seen.paired,
        "error": seen.link().filter(|l| *l == Link::Errored).and(d.remote.codex.last_error()),
        "enabled_by_rlord": by_rlord,
    })
}

/// The note that asks the user about the machine switch.
const MACHINE_ASK: &str = "Codex's Remote Control is machine-wide: once it's on, a phone paired with this machine can open and drive every Codex thread on Codex's background server, not only this session. Tell the user that in one line and ask; only on their explicit yes call again with confirm_machine: true. Never confirm it for them.";

/// `remote.set` for a Codex session (see the module doc).
pub(crate) fn set(
    d: &Daemon,
    s: &Session,
    on: bool,
    mode: Option<&str>,
    params: &Value,
) -> Result<Value> {
    if on && overlord::is_overlord_role(&s.role) {
        bail!("Codex's Remote Control is for sessions only, not the overlord: the overlord's settings make Codex run it in its pane, out of the phone's reach");
    }
    let codex = &d.remote.codex;
    if on {
        let a = codex.availability(true);
        if !a.available {
            bail!(
                "Remote Control isn't available for Codex here: {}",
                a.reason.unwrap_or_default()
            );
        }
    }
    let spec = spec_of(&d.db(), &s.id);
    let on_daemon = codex::on_daemon(&spec);
    let moved = spec["codex_host_by_remote"] == true;
    let wanted_before = spec["remote_control"]
        .as_str()
        .is_some_and(|n| !n.is_empty());
    let up = running(s);
    // A restart moves it onto the server (on) or back into its pane (off).
    let restart = up && if on { !on_daemon } else { moved && on_daemon };
    let going = if restart {
        crate::remote::work_going(s, None)
    } else {
        Vec::new()
    };
    let seen = codex.seen(true);
    let machine_off = on && !seen.link().is_some_and(Link::enabled);
    let confirmed = params["confirm_machine"] == true;
    if (machine_off && !confirmed) || (!going.is_empty() && mode.is_none()) {
        let mut note = Vec::new();
        if machine_off && !confirmed {
            note.push(MACHINE_ASK.to_string());
        }
        if !going.is_empty() && mode.is_none() {
            note.push(format!(
                "Turning it {} restarts {} (the conversation resumes), which would interrupt: {}. Ask the user: restart now (restart: \"now\"), or when its work is done (restart: \"when_idle\")?",
                if on { "on" } else { "off" },
                s.name,
                going.join("; ")
            ));
        }
        return Ok(json!({
            "session": s.id, "name": s.name, "needs_confirm": true,
            "machine_wide": machine_off && !confirmed,
            "work_going": going,
            "note": note.join(" "),
        }));
    }
    if machine_off {
        codex.enable()?;
        d.db().kv_set(ENABLED_BY_RLORD, "1")?;
        tracing::info!("Codex Remote Control switched on for this machine (the user confirmed)");
    }
    // The wish, and where the thread runs.
    let mut spec = spec;
    if let Some(o) = spec.as_object_mut() {
        if on {
            o.insert(
                "remote_control".into(),
                json!(crate::remote::remote_name(&s.name, &s.role)),
            );
            o.insert(
                "remote_control_at".into(),
                json!(rlord_core::types::now_ms()),
            );
            if !on_daemon {
                o.insert("codex_host".into(), json!(codex::HOST_DAEMON));
                o.insert("codex_host_by_remote".into(), json!(true));
            }
        } else {
            o.remove("remote_control");
            o.remove("remote_control_at");
            if moved {
                o.remove("codex_host");
                o.remove("codex_host_by_remote");
            }
        }
    }
    d.db().set_launch_spec(&s.id, &spec.to_string())?;
    crate::codex_daemon::sync_gate(&d.db());
    let mut restarted = false;
    let mut deferred = false;
    if restart {
        if mode == Some("when_idle") {
            crate::remote::enqueue(d, &s.id);
            deferred = true;
        } else {
            crate::remote::restart(d, &s.id, false)?;
            restarted = true;
        }
    }
    d.changed();
    let seen = codex.seen(machine_off);
    let s = d.db().resolve_ref(&s.id)?;
    let avail = codex.cached_availability().unwrap_or_default();
    let st = state_of(
        &s,
        on,
        &seen,
        &avail,
        codex.last_error().as_deref(),
        deferred,
    );
    let mut out = json!({"session": s.id, "name": s.name, "remote": st,
                         "restarted": restarted, "restarts_when_idle": deferred});
    let machine_on = seen.link().is_some_and(Link::enabled);
    out["note"] = json!(if on {
        match st["state"].as_str() {
            Some("on") => "on: the user opens it in the ChatGPT app (Codex), under the conversation's title".to_string(),
            Some("unpaired") => {
                out["needs_pairing"] = json!(true);
                "Remote Control is on for this machine, but no phone is paired yet: call remote_pair to show the user a pairing code on their screen".into()
            }
            Some("pending") => "it moves to Codex's background server as soon as its work is done (the conversation resumes)".into(),
            Some("stopped") => "it runs on Codex's background server from its next start".into(),
            Some("connecting") => "this machine is connecting to ChatGPT".into(),
            _ => format!(
                "not reachable yet: {}",
                st["reason"].as_str().unwrap_or("unknown")
            ),
        }
    } else if !wanted_before && !moved {
        "it was off".to_string()
    } else if on_daemon && !moved {
        "off; it stays on Codex's background server (your codex_daemon setting), so a paired phone can still reach it".into()
    } else if machine_on {
        out["machine_on"] = json!(true);
        format!(
            "{}; Remote Control is still on for this machine, so a paired phone may still open this conversation. Offer to switch it off for the whole machine (remote_control with machine: true, on: false)",
            if deferred {
                "it moves back into its own pane when its work is done"
            } else {
                "back in its own pane"
            }
        )
    } else {
        "off: back in its own pane".into()
    });
    Ok(out)
}

/// `remote.machine {agent: "codex", on, confirm_machine?}`: the machine
/// switch itself. On needs `confirm_machine` (the user's yes); off never
/// unpairs a phone and never touches a session.
pub(crate) fn machine(d: &Daemon, params: &Value) -> Result<Value> {
    if params["agent"].as_str().is_some_and(|a| a != "codex") {
        bail!("only Codex has a machine-wide Remote Control switch");
    }
    let on = params["on"]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("missing 'on' (true or false)"))?;
    let codex = &d.remote.codex;
    if on {
        let a = codex.availability(true);
        if !a.available {
            bail!(
                "Remote Control isn't available for Codex here: {}",
                a.reason.unwrap_or_default()
            );
        }
        if params["confirm_machine"] != true {
            return Ok(json!({"needs_confirm": true, "machine_wide": true, "note": MACHINE_ASK}));
        }
        let m = codex.enable()?;
        d.db().kv_set(ENABLED_BY_RLORD, "1")?;
        tracing::info!("Codex Remote Control switched on for this machine (the user confirmed)");
        d.changed();
        let seen = codex.seen(true);
        let mut out = json!({"machine": m.link.name(), "paired": seen.paired});
        if seen.paired == Some(0) || m.link != Link::Connected {
            out["needs_pairing"] = json!(seen.paired == Some(0));
        }
        return Ok(out);
    }
    let m = codex.disable()?;
    d.db().kv_delete(ENABLED_BY_RLORD)?;
    codex.pair_clear();
    tracing::info!("Codex Remote Control switched off for this machine");
    d.changed();
    Ok(json!({"machine": m.link.name(),
              "note": "off for this machine: no phone can reach Codex threads here; paired phones stay paired for next time"}))
}

/// `remote.pair {ui?}`: start pairing a phone. The code goes to the TUI
/// (pushed to open its dialog, unless `ui: false`) and to a terminal
/// `rlord remote pair` through `remote.pair_code`; the answer has only
/// that it's shown and when it expires.
pub(crate) fn pair(d: &Daemon, params: &Value) -> Result<Value> {
    if params["agent"].as_str().is_some_and(|a| a != "codex") {
        bail!("pairing a phone is for Codex's Remote Control (Claude Code's needs none)");
    }
    let codex = &d.remote.codex;
    let seen = codex.seen(true);
    match seen.link() {
        None => bail!("Codex's background server isn't running"),
        Some(Link::Disabled) => {
            bail!("Remote Control is switched off for this machine: turn it on first (the user must agree: it's machine-wide)")
        }
        Some(Link::Errored) => bail!(
            "this machine isn't connected to ChatGPT: {}",
            codex
                .last_error()
                .unwrap_or_else(|| "offline, or ChatGPT refused it".into())
        ),
        _ => {}
    }
    let expires = codex.pair_start()?;
    tracing::info!("Codex Remote Control: pairing started");
    let ui = params["ui"] != false;
    if ui {
        d.ui_cmd(json!({"cmd": "remote_pair"}));
        crate::ui::show_list(d)?;
    }
    Ok(json!({
        "shown": ui,
        "expires_at": expires,
        "note": "the pairing code is on the user's screen in rlord's list; they enter it in the ChatGPT app (Codex). Don't ask for the code or repeat it.",
    }))
}

/// `remote.pair_code`: the code on show, for the user's own terminal (the
/// TUI's dialog, `rlord remote pair`). Never forwarded to the overlord.
pub(crate) fn pair_code(d: &Daemon, params: &Value) -> Result<Value> {
    if params["close"] == true {
        d.remote.codex.pair_clear();
        return Ok(json!({"closed": true}));
    }
    let (p, _) = d.remote.codex.pair_peek();
    Ok(match p {
        None => json!({"open": false}),
        Some((p, claimed)) => json!({
            "open": true,
            "code": p.code.reveal(),
            "expires_at": p.expires_at,
            "claimed": claimed,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sess(daemon: bool, status: Status) -> Session {
        serde_json::from_value(json!({
            "id": "01s", "name": "web", "name_locked": false, "agent": "codex",
            "agent_session": "t-1", "agent_session_trust": "hook", "project_path": "/tmp",
            "tmux_session": null, "status": status, "group_path": "", "pinned": false,
            "created_at": 0, "archived_at": null, "last_user_msg_at": null,
            "last_agent_msg_at": null, "last_state_change_at": null, "repo_path": null,
            "workspace_path": null, "branch": null, "model": null, "summary_at": null,
            "codex_daemon": daemon,
        }))
        .unwrap()
    }

    fn seen(link: Link, paired: Option<usize>) -> Seen {
        let m = Machine::parse(&json!({"status": link.name(), "environmentId": "e"})).unwrap();
        Seen {
            machine: Some(m),
            paired,
        }
    }

    #[test]
    fn every_state() {
        let ok = Availability {
            available: true,
            ..Default::default()
        };
        let st =
            |s: &Session, want, sn: &Seen, q| state_of(s, want, sn, &ok, None, q)["state"].clone();
        let idle = sess(true, Status::Idle);
        assert_eq!(
            st(&idle, true, &seen(Link::Connected, Some(1)), false),
            "on"
        );
        assert_eq!(st(&idle, true, &seen(Link::Connected, None), false), "on");
        assert_eq!(
            st(&idle, true, &seen(Link::Connected, Some(0)), false),
            "unpaired"
        );
        assert_eq!(
            st(&idle, true, &seen(Link::Connecting, None), false),
            "connecting"
        );
        assert_eq!(st(&idle, true, &seen(Link::Errored, None), false), "failed");
        assert_eq!(
            st(&idle, true, &seen(Link::Disabled, None), false),
            "failed"
        );
        assert_eq!(st(&idle, true, &Seen::default(), false), "failed");
        assert_eq!(
            st(&idle, true, &seen(Link::Connected, Some(1)), true),
            "pending"
        );
        assert_eq!(
            st(
                &sess(true, Status::Stopped),
                true,
                &seen(Link::Connected, Some(1)),
                false
            ),
            "stopped"
        );
        // Not wanted: on only while a phone can reach it on the server.
        assert_eq!(
            st(&idle, false, &seen(Link::Connected, Some(1)), false),
            "on"
        );
        assert_eq!(
            st(&idle, false, &seen(Link::Connected, Some(0)), false),
            "off"
        );
        assert_eq!(
            st(
                &sess(false, Status::Idle),
                false,
                &seen(Link::Connected, Some(1)),
                false
            ),
            "off"
        );
        let no = Availability {
            available: false,
            reason: Some("Codex isn't logged in".into()),
            ..Default::default()
        };
        let v = state_of(&idle, true, &Seen::default(), &no, None, false);
        assert_eq!(v["state"], "unavailable");
        assert_eq!(v["app"], "the ChatGPT app");
        // The enrollment error is the reason for an errored machine.
        let v = state_of(
            &idle,
            true,
            &seen(Link::Errored, None),
            &ok,
            Some("needs MFA"),
            false,
        );
        assert_eq!(v["reason"], "needs MFA");
    }

    /// The whole path against a stand-in control socket: the machine switch
    /// waits for the user's yes, a refusal changes nothing, pairing never
    /// puts the code in an answer, and off moves the session back.
    #[test]
    fn switching_pairing_and_back() {
        use rlord_core::agents::codex_server::fake::Fake;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("c.sock");
        let fake = Fake::start(&sock);
        let d = Daemon::new(
            Db::open_in_memory().unwrap(),
            rlord_core::config::Config::default(),
        );
        d.remote.codex.use_stand_in(
            sock,
            Availability {
                available: true,
                ..Default::default()
            },
        );
        d.db()
            .insert_session(&rlord_core::db::NewSession {
                id: "01cx",
                name: "web",
                agent: "codex",
                agent_session: Some("t-1"),
                project_path: "/tmp",
                tmux_session: None,
                launch_spec: r#"{"agent":"codex"}"#,
                created_at: 1,
                repo_path: None,
                workspace_path: None,
                branch: None,
                role: "",
            })
            .unwrap();
        d.db().set_status("01cx", Status::Idle, 1).unwrap();
        let set = |p: Value| crate::remote::set(&d, &p);
        let spec = || spec_of(&d.db(), "01cx");

        // Machine off: ask first, change nothing.
        let r = set(json!({"ref": "01cx", "on": true})).unwrap();
        assert_eq!(r["needs_confirm"], true);
        assert_eq!(r["machine_wide"], true);
        assert!(r["note"].as_str().unwrap().contains("confirm_machine"));
        assert!(!fake.methods().iter().any(|m| m == "remoteControl/enable"));
        assert!(!codex::on_daemon(&spec()));

        // ChatGPT refuses the machine: said plainly, nothing changes.
        fake.refuse_enable("enrollment failed at `https://x/enroll`: HTTP 403 Forbidden, request-id: 1, body: {\"detail\":\"Multi-factor authentication required\"}");
        let e = set(json!({"ref": "01cx", "on": true, "confirm_machine": true})).unwrap_err();
        assert!(format!("{e:#}").contains("multi-factor"), "{e:#}");
        assert!(!codex::on_daemon(&spec()));
        assert_eq!(d.db().kv_get(ENABLED_BY_RLORD).unwrap(), None);

        // The user's yes: on, onto the server, and a phone to pair.
        fake.refuse_enable("");
        let r = set(json!({"ref": "01cx", "on": true, "confirm_machine": true})).unwrap();
        assert_eq!(r["remote"]["state"], "unpaired", "{r}");
        assert_eq!(r["needs_pairing"], true);
        let sp = spec();
        assert!(codex::on_daemon(&sp));
        assert_eq!(sp["codex_host_by_remote"], true);
        assert_eq!(
            d.db().kv_get(ENABLED_BY_RLORD).unwrap().as_deref(),
            Some("1")
        );

        // Pairing: the answer has no code; only pair_code (the terminal's) does.
        let r = pair(&d, &json!({"ui": false})).unwrap();
        assert!(!r.to_string().contains("STAND-IN"), "{r}");
        let c = pair_code(&d, &json!({})).unwrap();
        assert_eq!(c["code"], "STAND-IN-CODE");
        assert_eq!(c["claimed"], false);
        fake.claim();
        std::thread::sleep(CLAIM_POLL);
        watch_pairing(&d);
        assert_eq!(pair_code(&d, &json!({})).unwrap()["claimed"], true);
        let r = crate::remote::status(&d, &json!({"ref": "01cx", "force": true})).unwrap();
        assert_eq!(r["remote"]["state"], "on", "{r}");

        // Off: back into its pane; the machine switch stays, and is offered.
        let r = set(json!({"ref": "01cx", "on": false})).unwrap();
        assert_eq!(r["machine_on"], true, "{r}");
        assert!(r["note"].as_str().unwrap().contains("machine: true"));
        let sp = spec();
        assert!(!codex::on_daemon(&sp) && sp.get("codex_host_by_remote").is_none());
        assert!(sp.get("remote_control").is_none());

        // The machine switch itself: on asks, off just goes.
        let r = machine(&d, &json!({"on": true})).unwrap();
        assert_eq!(r["needs_confirm"], true);
        let r = machine(&d, &json!({"on": false})).unwrap();
        assert_eq!(r["machine"], "disabled");
        assert_eq!(d.db().kv_get(ENABLED_BY_RLORD).unwrap(), None);
        assert!(pair(&d, &json!({"ui": false})).is_err());
    }

    #[test]
    fn not_for_a_codex_overlord() {
        let d = Daemon::new(
            Db::open_in_memory().unwrap(),
            rlord_core::config::Config::default(),
        );
        let mut s = sess(false, Status::Idle);
        s.role = "overlord".into();
        let e = set(&d, &s, true, None, &json!({})).unwrap_err();
        assert!(e.to_string().contains("sessions only"));
    }
}
