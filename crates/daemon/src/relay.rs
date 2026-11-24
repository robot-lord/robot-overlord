//! Remote access through one pinned relay: the daemon's side. (Not Claude
//! Code's Remote Control: that is `remote.rs`.)
//!
//! The daemon holds no keys and does no crypto: `rlord-web` (the plugin)
//! does all of that and alone dials out. What lives here:
//!
//! - The user's commands, `relay.status`, `relay.setup`, `relay.pair`,
//!   `relay.pair_cancel`, `relay.confirm`, `relay.reject`,
//!   `relay.devices`, `relay.revoke`, `relay.input`, `relay.disable`:
//!   called by the CLI
//!   (`rlord relay …`) and the TUI (`:relay`) over the socket. They are
//!   reached only from the socket's own dispatch (`user_call`), never
//!   through `Daemon::rpc`, so no MCP tool and no `web.call` op can reach
//!   them (`relay.` is also a forbidden family in `web::FORBIDDEN`).
//! - Forwarding: a command the plugin must answer goes on the
//!   `web.subscribe` stream as `{"notify":"remote","id","req"}` (never
//!   folded into `changed`), and the daemon waits up to 10 s for the
//!   plugin's `web.remote_reply {id, ok | error}`.
//! - The mirror: the plugin reports its state with `web.remote_state`
//!   (`normalize` has the shape) on every change. The daemon keeps it,
//!   answers `relay.status` and `relay.devices` from it, moves `rev`, and
//!   puts a notice on every surface when a device starts waiting for the
//!   user's confirmation.
//!
//! The local page's `device.*` ops (`web.rs`) call the same functions.
//! See docs/working/e2e-protocol.md, "Host and daemon".

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::config::Config;
use rlord_core::plugin;
use rlord_core::qr::Qr;

use crate::Daemon;

/// How long a forwarded command waits for the plugin's answer.
const REPLY_WAIT: Duration = Duration::from_secs(10);
/// How long a command waits for a plugin it started to subscribe, and to
/// report its state.
const START_WAIT: Duration = Duration::from_secs(5);
/// At most this many devices are kept from a report.
const MAX_DEVICES: usize = 64;

/// The daemon's relay state: the plugin's last report and the commands
/// waiting for its answer.
#[derive(Default)]
pub struct Relay {
    /// The plugin's last `web.remote_state` (normalized); None before its
    /// first, and after it went away.
    mirror: Mutex<Option<Value>>,
    /// Forwarded commands waiting for `web.remote_reply`, by id.
    waiting: Mutex<HashMap<String, mpsc::SyncSender<Result<Value, String>>>>,
    seq: AtomicU64,
}

impl Relay {
    fn mirror(&self) -> Option<Value> {
        self.mirror.lock().expect("relay mutex").clone()
    }

    /// The plugin's stream ended (it exited): what it reported is stale,
    /// and nothing will answer the commands still waiting.
    pub(crate) fn plugin_gone(&self) -> bool {
        self.waiting.lock().expect("relay mutex").clear();
        self.mirror.lock().expect("relay mutex").take().is_some()
    }
}

/// A user command (`relay.*`), from the socket's dispatch only. None: not
/// one of them.
pub fn user_call(d: &Daemon, method: &str, params: &Value) -> Option<Result<Value>> {
    Some(match method {
        "relay.status" => status(d),
        "relay.setup" => setup(d, params),
        "relay.pair" => pair(d),
        "relay.pair_cancel" => pair_cancel(d),
        "relay.confirm" => confirm(d),
        "relay.reject" => reject(d),
        "relay.devices" => devices(d),
        "relay.revoke" => revoke(d, params),
        "relay.input" => input(d, params),
        "relay.disable" => disable(d),
        _ => return None,
    })
}

/// `relay.status` (and the page's `device.list`): the config, the plugin,
/// and what it last reported. Never starts anything.
pub(crate) fn status(d: &Daemon) -> Result<Value> {
    let cfg = d.config();
    let m = d.relay.mirror();
    // A plugin that reports is running; otherwise ask the supervisor
    // (which hashes the plugin file: not on every poll).
    let web = if m.is_some() {
        json!({"installed": true, "state": "running", "problem": null})
    } else {
        d.web().status(cfg.web.enabled)
    };
    let field = |k: &str| m.as_ref().map(|m| m[k].clone()).unwrap_or(Value::Null);
    Ok(json!({
        "enabled": cfg.remote.enabled,
        "relay": cfg.remote.relay,
        "relay_key": cfg.remote.relay_key,
        "plugin": {
            "installed": web["installed"],
            "state": web["state"],
            "problem": web["problem"],
        },
        // Whether the rest is the plugin's word (false: it isn't running,
        // or hasn't reported yet).
        "reported": m.is_some(),
        "connected": field("connected") == true,
        "error": field("error"),
        "devices": m.as_ref().map(|m| m["devices"].clone()).unwrap_or(json!([])),
        "pairing": field("pairing"),
        "pending": field("pending"),
    }))
}

/// A paired device, as the plugin last reported it.
pub(crate) struct Paired {
    pub id: String,
    pub name: String,
    /// It has a live session now.
    pub linked: bool,
    /// It may type into panes (false: view-only, set by the user).
    pub input: bool,
}

/// The paired devices, as the plugin last reported them (none when it
/// isn't running or hasn't reported).
pub(crate) fn paired(d: &Daemon) -> Vec<Paired> {
    let m = d.relay.mirror();
    m.as_ref()
        .and_then(|m| m["devices"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| {
            Some(Paired {
                id: v["id"].as_str()?.to_string(),
                name: v["name"].as_str().unwrap_or("").to_string(),
                linked: v["linked"] == true,
                input: v["input"] != false,
            })
        })
        .collect()
}

/// What a local page's `snapshot` carries: enough for the header (a count
/// and the device waiting for confirmation), never the list.
pub(crate) fn summary(d: &Daemon) -> Value {
    let m = d.relay.mirror();
    json!({
        "enabled": d.config().remote.enabled,
        "connected": m.as_ref().is_some_and(|m| m["connected"] == true),
        "devices": m.as_ref().and_then(|m| m["devices"].as_array().map(Vec::len)).unwrap_or(0),
        "pending": m.as_ref().map(|m| m["pending"].clone()).unwrap_or(Value::Null),
    })
}

/// `relay.setup {address, token?, reset?}`: the plugin parses the relay
/// address and makes the keys for it (`reset`: deleting every key and
/// device first); then `[remote] relay`, `relay_key` and `enabled = true`
/// are written from its answer. Starts the plugin if it isn't running.
fn setup(d: &Daemon, p: &Value) -> Result<Value> {
    let address = p["address"].as_str().map(str::trim).unwrap_or("");
    if address.is_empty() {
        bail!("setup needs the relay's address (`rlord-relay address` prints it)");
    }
    let mut req = json!({"cmd": "setup", "address": address, "reset": p["reset"] == true});
    if let Some(t) = p["token"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
        req["token"] = json!(t);
    }
    let ok = forward(d, req)?;
    let origin = ok["origin"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("rlord-web's setup answer has no origin")?
        .to_string();
    let fingerprint = ok["fingerprint"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("rlord-web's setup answer has no fingerprint")?
        .to_string();
    Config::set_value("remote", "relay", &origin)?;
    Config::set_value("remote", "relay_key", &fingerprint)?;
    Config::set_bool("remote", "enabled", true)?;
    d.update_config(|c| {
        c.remote.relay = origin.clone();
        c.remote.relay_key = fingerprint.clone();
        c.remote.enabled = true;
    });
    d.changed();
    Ok(json!({"relay": origin, "relay_key": fingerprint, "enabled": true}))
}

/// `relay.pair` (and `device.pair`): a pairing offer for one device, for
/// 300 s. The answer is the plugin's `{qr, expires}` with `qr_rows`, the
/// code's modules, for a page to draw.
pub(crate) fn pair(d: &Daemon) -> Result<Value> {
    let cfg = d.config();
    if cfg.remote.relay.is_empty() {
        bail!("no relay is set up yet: run `rlord relay setup <relay address>` first");
    }
    if !cfg.remote.enabled {
        bail!("remote access is off: `rlord relay setup <relay address>` turns it on again");
    }
    let mut ok = forward(d, json!({"cmd": "pair"}))?;
    let qr = ok["qr"]
        .as_str()
        .context("rlord-web's pairing answer has no qr")?;
    ok["qr_rows"] = json!(Qr::new(qr)?.rows);
    Ok(ok)
}

pub(crate) fn pair_cancel(d: &Daemon) -> Result<Value> {
    forward(d, json!({"cmd": "pair_cancel"}))
}

/// The device waiting for confirmation: paired. Its `{device: {id, name}}`.
pub(crate) fn confirm(d: &Daemon) -> Result<Value> {
    forward(d, json!({"cmd": "confirm"}))
}

pub(crate) fn reject(d: &Daemon) -> Result<Value> {
    forward(d, json!({"cmd": "reject"}))
}

/// `relay.devices`: the paired devices, as the plugin last reported them
/// (starting it, and waiting for its first report, if it isn't running).
fn devices(d: &Daemon) -> Result<Value> {
    if d.relay.mirror().is_none() {
        ensure_plugin(d)?;
        let deadline = Instant::now() + START_WAIT;
        while d.relay.mirror().is_none() {
            if Instant::now() >= deadline {
                bail!(
                    "rlord-web hasn't reported its devices (log: {})",
                    plugin::log_path().display()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let m = d.relay.mirror().unwrap_or_default();
    Ok(json!({"devices": m["devices"], "pending": m["pending"]}))
}

/// `relay.revoke {device}` (and `device.revoke`): unpair one device by id.
pub(crate) fn revoke(d: &Daemon, p: &Value) -> Result<Value> {
    let device = p["device"].as_str().map(str::trim).unwrap_or("");
    if device.is_empty() {
        bail!("revoke needs a device id (`rlord relay devices` lists them)");
    }
    forward(d, json!({"cmd": "revoke", "device": device}))
}

/// `relay.input {device, input}` (and the page's `device.input`): whether
/// a paired device may type into panes (false: view-only, it only ever
/// watches). The plugin keeps it on the device's record, as `revoke` is
/// passed on; the user's alone, never a device's or the overlord's.
pub(crate) fn input(d: &Daemon, p: &Value) -> Result<Value> {
    let device = p["device"].as_str().map(str::trim).unwrap_or("");
    if device.is_empty() {
        bail!("input needs a device id (`rlord relay devices` lists them)");
    }
    let Some(on) = p["input"].as_bool() else {
        bail!("input needs input: true (it may type) or false (view-only)");
    };
    forward(d, json!({"cmd": "input", "device": device, "input": on}))
}

/// `relay.disable`: the plugin closes its relay connection (if it runs),
/// then `[remote] enabled = false`. Devices stay paired.
fn disable(d: &Daemon) -> Result<Value> {
    if d.web_subscribers() > 0 {
        forward(d, json!({"cmd": "disable"}))?;
    }
    if d.config().remote.enabled {
        Config::set_bool("remote", "enabled", false)?;
        d.update_config(|c| c.remote.enabled = false);
        d.changed();
    }
    Ok(json!({"enabled": false}))
}

/// The plugin is running and subscribed: start it if it isn't (an error
/// naming `rlord web install` when there is none).
fn ensure_plugin(d: &Daemon) -> Result<()> {
    if d.web_subscribers() > 0 {
        return Ok(());
    }
    let cfg = d.config();
    let st = d.web().status(cfg.web.enabled);
    if st["installed"] != true {
        bail!(
            "remote access needs the web plugin: run `rlord web install` ({})",
            st["problem"].as_str().unwrap_or("it is not installed")
        );
    }
    d.web().start(cfg.web.enabled)?;
    let deadline = Instant::now() + START_WAIT;
    while d.web_subscribers() == 0 {
        if Instant::now() >= deadline {
            bail!(
                "rlord-web started but didn't subscribe (log: {})",
                plugin::log_path().display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// Send `req` to the plugin and wait for its answer (`web.remote_reply`).
pub(crate) fn forward(d: &Daemon, req: Value) -> Result<Value> {
    ensure_plugin(d)?;
    let id = format!("r{}", d.relay.seq.fetch_add(1, Ordering::SeqCst) + 1);
    let (tx, rx) = mpsc::sync_channel(1);
    d.relay
        .waiting
        .lock()
        .expect("relay mutex")
        .insert(id.clone(), tx);
    d.notify_line(json!({"notify": "remote", "id": id, "req": req}).to_string());
    let got = rx.recv_timeout(REPLY_WAIT);
    d.relay.waiting.lock().expect("relay mutex").remove(&id);
    match got {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(m)) => bail!("{m}"),
        Err(mpsc::RecvTimeoutError::Timeout) => bail!(
            "rlord-web didn't answer within {} s (log: {})",
            REPLY_WAIT.as_secs(),
            plugin::log_path().display()
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("rlord-web went away before it answered")
        }
    }
}

/// `web.remote_reply {id, ok | error}`: the plugin's answer to a
/// forwarded command.
pub fn remote_reply(d: &Daemon, p: &Value) -> Result<Value> {
    let id = p["id"]
        .as_str()
        .context("remote_reply needs the command's id")?;
    let answer = match (p.get("ok"), p.get("error")) {
        (_, Some(e)) if !e.is_null() => Err(clip(e.as_str().unwrap_or("rlord-web failed"), 500)),
        (Some(v), _) => Ok(v.clone()),
        _ => bail!("remote_reply needs ok or error"),
    };
    let Some(tx) = d.relay.waiting.lock().expect("relay mutex").remove(id) else {
        bail!("no command {id} is waiting (it timed out)");
    };
    let _ = tx.try_send(answer);
    Ok(json!({"ok": true}))
}

/// `web.remote_state {relay, connected, error, devices, pairing,
/// pending}`: the plugin's state, the whole of it each time. A device
/// that starts waiting for confirmation puts a notice on every surface.
pub fn remote_state(d: &Daemon, p: &Value) -> Result<Value> {
    if !p.is_object() {
        bail!("remote_state must be an object");
    }
    let new = normalize(p);
    let old = d
        .relay
        .mirror
        .lock()
        .expect("relay mutex")
        .replace(new.clone());
    // A device no longer paired is no surface any more.
    let paired: Vec<String> = paired(d).into_iter().map(|p| p.id).collect();
    d.web_ui().keep_devices(&paired);
    // (Nor does it hold a pane.)
    crate::control::keep_devices(d, &paired);
    let key = |m: &Value| -> Option<(String, String)> {
        let p = &m["pending"];
        p.is_object().then(|| {
            (
                p["name"].as_str().unwrap_or("").to_string(),
                p["code"].as_str().unwrap_or("").to_string(),
            )
        })
    };
    let was = old.as_ref().and_then(key);
    if let Some((name, code)) = key(&new).filter(|k| Some(k) != was.as_ref()) {
        d.notice_all(
            &format!(
                "a device wants to pair: \"{name}\", code {code}: check the code, then :relay"
            ),
            &format!(
                "a device wants to pair: \"{name}\", code {code}: check the code, then Devices"
            ),
        );
    }
    if old.as_ref() != Some(&new) {
        d.changed_relay();
    }
    Ok(json!({"ok": true}))
}

/// The plugin's report, with only the fields rlord shows, each clipped:
/// `{relay, connected, error, devices: [{id, name, paired_at, last_seen,
/// linked, input}], pairing: {expires} | null, pending: {name, code,
/// expires} | null}`. Times are Unix seconds; `input` is true unless the
/// plugin says the device is view-only.
fn normalize(p: &Value) -> Value {
    let s = |v: &Value, n: usize| json!(clip(v.as_str().unwrap_or(""), n));
    let num = |v: &Value| v.as_u64().map_or(Value::Null, |n| json!(n));
    let devices: Vec<Value> = p["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["id"].as_str().is_some_and(|i| !i.is_empty()))
        .take(MAX_DEVICES)
        .map(|v| {
            json!({
                "id": s(&v["id"], 40),
                "name": s(&v["name"], 40),
                "paired_at": num(&v["paired_at"]),
                "last_seen": num(&v["last_seen"]),
                "linked": v["linked"] == true,
                "input": v["input"] != false,
            })
        })
        .collect();
    let pairing = &p["pairing"];
    let pending = &p["pending"];
    json!({
        "relay": s(&p["relay"], 300),
        "connected": p["connected"] == true,
        "error": p["error"].as_str().map(|e| clip(e, 300)),
        "devices": devices,
        "pairing": if pairing.is_object() { json!({"expires": num(&pairing["expires"])}) } else { Value::Null },
        "pending": if pending.is_object() {
            json!({
                "name": s(&pending["name"], 40),
                "code": s(&pending["code"], 12),
                "expires": num(&pending["expires"]),
            })
        } else {
            Value::Null
        },
    })
}

/// `s` cut to at most `n` characters, control characters dropped (a
/// device's name is the device's to choose).
fn clip(s: &str, n: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_keeps_only_what_rlord_shows() {
        let got = normalize(&json!({
            "relay": "https://r.example", "connected": true, "error": null, "secret": "x",
            "devices": [
                {"id": "ab12", "name": "phone\u{7}\nname-that-goes-on-and-on-past-forty-characters",
                 "paired_at": 5, "last_seen": null, "linked": true, "key": "k"},
                {"name": "no id"},
            ],
            "pairing": {"expires": 9, "secret": "s"},
            "pending": {"name": "tab", "code": "123456", "expires": 9, "psk": "p"},
        }));
        assert_eq!(
            got,
            json!({
                "relay": "https://r.example", "connected": true, "error": null,
                "devices": [{"id": "ab12", "name": "phonename-that-goes-on-and-on-past-forty",
                             "paired_at": 5, "last_seen": null, "linked": true, "input": true}],
                "pairing": {"expires": 9},
                "pending": {"name": "tab", "code": "123456", "expires": 9},
            })
        );
        let empty = normalize(&json!({}));
        assert_eq!(empty["devices"], json!([]));
        assert!(empty["pending"].is_null() && empty["pairing"].is_null());
        assert_eq!(empty["connected"], false);
    }
}
