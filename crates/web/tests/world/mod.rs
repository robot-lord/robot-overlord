//! The world the engine tests share: the real plugin under an in-memory
//! daemon (`common::Rig`), the real relay in process on 127.0.0.1, and a
//! `Tamper` in front of it holding the relay's key: the relay's origin is
//! the proxy's, so the host and every device reach the relay through it.
//! Devices are the phone's own engine (`rlord_e2e_device::Phone`).

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::common::*;
use rlord_core::{paths, plugin};
use rlord_e2e_client::Tuning;
use rlord_e2e_device::testing::Tamper;
use rlord_e2e_device::Phone;
use rlord_relay_proto::Identity;

pub const WAIT: Duration = Duration::from_secs(10);

/// A user command over the daemon's socket, as the CLI sends it.
pub async fn rpc(method: &str, params: Value) -> Result<Value, String> {
    let (method, sock) = (method.to_string(), paths::socket_path());
    tokio::task::spawn_blocking(move || {
        let mut s = std::os::unix::net::UnixStream::connect(&sock).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let line = json!({"id": 1, "method": method, "params": params}).to_string() + "\n";
        s.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        let mut answer = String::new();
        BufReader::new(s)
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&answer).map_err(|e| e.to_string())?;
        match v.get("error") {
            Some(e) => Err(e["message"].as_str().unwrap_or("?").to_string()),
            None => Ok(v["result"].clone()),
        }
    })
    .await
    .unwrap()
}

/// Waits until `relay.status` satisfies `pick`.
pub async fn status_until(what: &str, pick: impl Fn(&Value) -> bool) -> Value {
    let t = Instant::now();
    loop {
        if let Ok(s) = rpc("relay.status", json!({})).await {
            if pick(&s) {
                return s;
            }
            assert!(t.elapsed() < WAIT * 2, "{what}: {s}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn audit() -> String {
    std::fs::read_to_string(plugin::remote_log_path()).unwrap_or_default()
}

/// How often `what` is in the audit log.
pub fn count(what: &str) -> usize {
    audit().matches(what).count()
}

/// Waits until the audit log holds `what` at least `n` times.
pub async fn audit_until(what: &str, n: usize) {
    let t = Instant::now();
    while count(what) < n {
        assert!(t.elapsed() < WAIT * 2, "never {n} × {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub struct World {
    pub rig: Rig,
    pub relay: Option<rlord_relay::Running>,
    pub relay_dir: tempfile::TempDir,
    pub tamper: Tamper,
    pub key: Identity,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A daemon with `n` sessions, its plugin, a relay behind a `Tamper`; the
/// host set up for it (keys reset).
pub async fn world(n: usize) -> World {
    world_with(n, |_| {}).await
}

pub async fn world_with(n: usize, f: impl FnOnce(&mut rlord_relay::Config)) -> World {
    env_once("engine", "rlord-test-engine");
    let rig = tokio::task::spawn_blocking(move || {
        Rig::start(move |db| {
            for i in 0..n {
                add(db, &format!("session-{i:04}"), None);
            }
        })
    })
    .await
    .unwrap();
    let relay_dir = tempfile::tempdir().unwrap();
    // The relay's key, known to the test: the Tamper holds it too.
    let key = rlord_e2e_client::soft::new_identity().unwrap();
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(relay_dir.path().join("relay.key"))
            .unwrap();
        writeln!(f, "{}", hex(&key.to_bytes())).unwrap();
    }
    let tamper = Tamper::new(key.clone()).await;
    let relay = start_relay(&relay_dir, &tamper, None, f).await;
    let token = relay
        .admin()
        .token_new(1, Duration::from_secs(600), "local")
        .unwrap();
    let r = rpc(
        "relay.setup",
        json!({"address": relay.address().to_string(), "token": token, "reset": true}),
    )
    .await
    .unwrap();
    assert_eq!(r["relay"], tamper.origin);
    World {
        rig,
        relay: Some(relay),
        relay_dir,
        tamper,
        key,
    }
}

async fn start_relay(
    dir: &tempfile::TempDir,
    tamper: &Tamper,
    listen: Option<std::net::SocketAddr>,
    f: impl FnOnce(&mut rlord_relay::Config),
) -> rlord_relay::Running {
    let mut cfg = rlord_relay::Config::new(dir.path());
    cfg.origin = Some(tamper.origin.clone());
    if let Some(l) = listen {
        cfg.listen = l;
    }
    f(&mut cfg);
    let relay = rlord_relay::start(cfg).await.unwrap();
    assert_eq!(relay.origin(), tamper.origin);
    tamper.upstream(&format!("ws://{}/v1", relay.addr()));
    relay
}

impl World {
    /// Stops the relay and starts it again on the same port and data (a
    /// new `boot`; its queues and flags forgotten).
    pub async fn restart_relay(&mut self) {
        let relay = self.relay.take().unwrap();
        let addr = relay.addr();
        relay.shutdown().await;
        self.relay = Some(start_relay(&self.relay_dir, &self.tamper, Some(addr), |_| {}).await);
    }
}

/// A paired phone (the engine, software keys) in the foreground, live.
pub async fn pair(name: &str) -> (Phone, tempfile::TempDir) {
    pair_tuned(name, Tuning::default()).await
}

pub async fn pair_tuned(name: &str, tuning: Tuning) -> (Phone, tempfile::TempDir) {
    let p = rpc("relay.pair", json!({})).await.unwrap();
    let qr = p["qr"].as_str().unwrap().to_string();
    status_until("connected for the pairing", |s| s["connected"] == true).await;
    let dir = tempfile::tempdir().unwrap();
    let mut phone = Phone::open_tuned(dir.path(), tuning).unwrap();
    phone.engine.lifecycle(true);
    let code = phone.start_pairing(&qr, name).await.unwrap();
    let s = status_until("pending", |s| s["pending"].is_object()).await;
    assert_eq!(s["pending"]["name"], name);
    assert_eq!(s["pending"]["code"], code.as_str());
    rpc("relay.confirm", json!({})).await.unwrap();
    phone.pairing_outcome(WAIT).await.unwrap();
    phone.conn("live", WAIT).await.unwrap();
    (phone, dir)
}

/// The phone's device id.
pub async fn device_id(phone: &mut Phone) -> String {
    phone.status().await.unwrap()["device_id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Waits until the host's mailbox for this device is keyed `n` times.
pub async fn keyed(id: &str, n: usize) {
    audit_until(&format!("{id} mailbox keyed"), n).await;
}

/// Stops the plugin and waits until the phone sees its host offline.
pub async fn host_away(w: &World, phone: &mut Phone) {
    w.rig.daemon.web().stop();
    phone.conn("host_offline", WAIT * 2).await.unwrap();
}

/// Starts the plugin with `env` set for it (debug hooks).
pub fn start_plugin(w: &World, env: &[(&str, &str)]) {
    // Safety: under LOCK; the daemon passes its environment to the plugin.
    for (k, v) in env {
        std::env::set_var(k, v);
    }
    w.rig.daemon.web().start(false).unwrap();
    for (k, _) in env {
        std::env::remove_var(k);
    }
}
