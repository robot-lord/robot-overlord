//! Remote access end to end: the real `rlord-web` under an in-memory
//! daemon, the real relay (`rlord_relay`, in process on 127.0.0.1), and
//! the test device (`rlord_e2e_device`). No tmux session is started; no
//! agent runs; nothing leaves loopback.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::*;
use rlord_core::{paths, plugin};
use rlord_e2e_device::{testing, ConnectError, Device, Opts};
use rlord_relay_proto::msg::{close, HostState};
use rlord_relay_proto::{FromRelay, Identity, ToRelay};

const WAIT: Duration = Duration::from_secs(10);

/// A user command over the daemon's socket, as the CLI sends it.
async fn rpc(method: &str, params: Value) -> Result<Value, String> {
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
async fn status_until(what: &str, pick: impl Fn(&Value) -> bool) -> Value {
    let t = Instant::now();
    loop {
        if let Ok(s) = rpc("relay.status", json!({})).await {
            if pick(&s) {
                return s;
            }
            assert!(t.elapsed() < WAIT, "{what}: {s}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn audit() -> String {
    std::fs::read_to_string(plugin::remote_log_path()).unwrap_or_default()
}

struct World {
    rig: Rig,
    relay: rlord_relay::Running,
    _relay_dir: tempfile::TempDir,
}

/// A daemon with `n` sessions, its plugin, and a relay; the host set up
/// for that relay (from scratch: earlier tests' keys are reset).
async fn world(n: usize) -> World {
    env_once("remote", "rlord-test-remote");
    let rig = tokio::task::spawn_blocking(move || {
        Rig::start(move |db| {
            for i in 0..n {
                add(db, &format!("session-{i:04}-{}", "x".repeat(40)), None);
            }
        })
    })
    .await
    .unwrap();
    let relay_dir = tempfile::tempdir().unwrap();
    let relay = rlord_relay::start(rlord_relay::Config::new(relay_dir.path()))
        .await
        .unwrap();
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
    assert_eq!(r["relay"], relay.origin());
    World {
        rig,
        relay,
        _relay_dir: relay_dir,
    }
}

/// Pair `name` through the CLI's commands, confirming the code.
async fn pair(name: &str) -> Device {
    let p = rpc("relay.pair", json!({})).await.unwrap();
    let qr = p["qr"].as_str().unwrap().to_string();
    assert!(qr.starts_with("RLORD1:"));
    status_until("connected for the pairing", |s| s["connected"] == true).await;
    let mut dev = Device::new(name).unwrap();
    let run = dev.start_pairing(&qr).await.unwrap();
    assert_eq!(run.code, run.host_code);
    let s = status_until("pending", |s| s["pending"].is_object()).await;
    assert_eq!(s["pending"]["name"], name);
    assert_eq!(s["pending"]["code"], run.code);
    // Not stored, not callable, until confirmed.
    assert!(s["devices"].as_array().unwrap().is_empty());
    let c = rpc("relay.confirm", json!({})).await.unwrap();
    assert_eq!(c["device"]["id"], dev.id());
    run.outcome(&mut dev, WAIT).await.unwrap();
    dev
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_confirm_then_use_it() {
    let _g = LOCK.lock().await;
    let w = world(2).await;
    let dev = pair("phone").await;
    let s = rpc("relay.devices", json!({})).await.unwrap();
    assert!(s.to_string().contains(&dev.id()), "{s}");

    // The cheap check: nothing waiting, no handshake.
    let c = dev.connect().await.unwrap();
    assert_eq!(c.ready.host, Some(HostState::Online));
    assert_eq!(c.ready.signal, Some(0));
    assert_eq!(c.ready.queued, Some(0));
    c.close().await;

    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    assert_eq!(live.hello["device"], dev.id());
    let snap = live.request(&mut c, "snapshot", json!({})).await.unwrap();
    assert!(snap["ok"]["rows"].is_array(), "{snap}");
    // A device's snapshot has no relay section (the page's has).
    assert!(snap["ok"].get("relay").is_none());

    // overlord.ask reaches the daemon (no overlord here: its own error).
    let ask = live
        .request(&mut c, "overlord.ask", json!({"text": "hello"}))
        .await
        .unwrap();
    assert!(
        ask["error"]["message"].is_string() || ask["ok"].is_object(),
        "{ask}"
    );
    assert_ne!(ask["error"]["code"], "not_allowed", "{ask}");

    // What a device may not do.
    for op in [
        "form.setup_save",
        "keys.list",
        "device.list",
        "device.confirm",
        "page.report",
    ] {
        let a = live.request(&mut c, op, json!({})).await.unwrap();
        assert_eq!(a["error"]["code"], "not_allowed", "{op}: {a}");
    }

    // A change reaches the live session.
    rpc(
        "session.rename",
        json!({"ref": format!("session-0000-{}", "x".repeat(40)), "name": "renamed"}),
    )
    .await
    .unwrap();
    let ev = live.event(&mut c, "changed", WAIT).await.unwrap();
    assert!(ev["rev"].is_string());
    assert!(audit().contains(&format!("{} op snapshot", dev.id())));
    drop(w);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signal_wakes_a_device_and_a_snapshot_spans_frames() {
    let _g = LOCK.lock().await;
    // Enough sessions that the snapshot is several 16 KiB frames.
    let _w = world(300).await;
    let dev = pair("tablet").await;
    // Nothing for it yet.
    let c = dev.connect().await.unwrap();
    assert_eq!(c.ready.signal, Some(0));
    let boot = c.ready.boot.clone();
    c.close().await;
    // A change while it has no live session: the host signals it.
    rpc(
        "session.rename",
        json!({"ref": format!("session-0001-{}", "x".repeat(40)), "name": "moved"}),
    )
    .await
    .unwrap();
    let t = Instant::now();
    let mut c = loop {
        let c = dev.connect().await.unwrap();
        if c.ready.signal == Some(1) {
            break c;
        }
        assert!(t.elapsed() < WAIT, "no signal");
        c.close().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(c.ready.boot, boot);
    let mut live = dev.live(&mut c).await.unwrap();
    let snap = live.request(&mut c, "snapshot", json!({})).await.unwrap();
    let rows = snap["ok"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 300);
    assert!(snap.to_string().len() > 2 * 16 * 1024);
    c.send(|n| ToRelay::AckSignal { n }).await.unwrap();
    c.wait_for(WAIT, |m| matches!(m, FromRelay::Ok { .. }))
        .await
        .unwrap();
    c.close().await;
    let c = dev.connect().await.unwrap();
    assert_eq!(c.ready.signal, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mailbox_request_waits_for_the_host() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let mut dev = pair("watch").await;
    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    dev.key_mailbox(&mut c, &mut live).await.unwrap();
    // The host saved it once the first mailbox frame proved it.
    let t = Instant::now();
    while !audit().contains(&format!("{} mailbox keyed", dev.id())) {
        assert!(t.elapsed() < WAIT, "mailbox never keyed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    c.close().await;

    // The plugin stops: the host is offline at the relay.
    w.rig.daemon.web().stop();
    let mut c = {
        let t = Instant::now();
        loop {
            let c = dev.connect().await.unwrap();
            if c.ready.host == Some(HostState::Offline) {
                break c;
            }
            assert!(t.elapsed() < WAIT);
            c.close().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let req = dev
        .mailbox_request("m1", "overlord.ask", json!({"text": "while you were away"}))
        .unwrap();
    c.send_mailbox(req).await.unwrap();
    c.wait_for(WAIT, |m| {
        matches!(m, FromRelay::Ok { .. } | FromRelay::Err { .. })
    })
    .await
    .ok();
    // The plugin comes back, fetches the queued request and answers it.
    w.rig.daemon.web().start(false).unwrap();
    let (seq, payload) = c.recv_mailbox(Duration::from_secs(20)).await.unwrap();
    let answer = dev.mailbox_open(&payload).unwrap();
    assert_eq!(answer["id"], "m1", "{answer}");
    assert_ne!(answer["error"]["code"], "not_allowed");
    c.send(|n| ToRelay::Ack {
        n,
        seq,
        device: None,
    })
    .await
    .unwrap();
    // The same request again is not acted on twice.
    let again = dev
        .mailbox_request("m1", "overlord.ask", json!({"text": "while you were away"}))
        .unwrap();
    c.send_mailbox(again).await.unwrap();
    let (_, payload) = c.recv_mailbox(WAIT).await.unwrap();
    assert_eq!(
        dev.mailbox_open(&payload).unwrap()["error"]["code"],
        "repeat"
    );
    // Only the two ops travel by mailbox.
    let snap = dev.mailbox_request("m2", "snapshot", json!({})).unwrap();
    c.send_mailbox(snap).await.unwrap();
    let (_, payload) = c.recv_mailbox(WAIT).await.unwrap();
    assert_eq!(
        dev.mailbox_open(&payload).unwrap()["error"]["code"],
        "not_allowed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_closes_the_door() {
    let _g = LOCK.lock().await;
    let _w = world(1).await;
    let dev = pair("old-phone").await;
    let mut c = dev.connect().await.unwrap();
    let _live = dev.live(&mut c).await.unwrap();
    rpc("relay.revoke", json!({"device": dev.id()}))
        .await
        .unwrap();
    // Its live link closes, and the relay refuses it from now on.
    let code = c.wait_closed(WAIT).await.unwrap();
    assert!(code == close::NOT_ALLOWED || code >= 1000, "{code}");
    let t = Instant::now();
    loop {
        match dev.try_connect().await {
            Err(ConnectError::Closed(code, _)) => {
                assert_eq!(code, close::NOT_ALLOWED);
                break;
            }
            Ok(c) => {
                assert!(t.elapsed() < WAIT, "still admitted");
                c.close().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert!(audit().contains(&format!("{} revoked", dev.id())));
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_needs_the_secret_the_confirmation_and_one_use() {
    let _g = LOCK.lock().await;
    let _w = world(1).await;
    // Rejected: nothing stored.
    let p = rpc("relay.pair", json!({})).await.unwrap();
    let qr = p["qr"].as_str().unwrap().to_string();
    status_until("connected", |s| s["connected"] == true).await;
    let mut dev = Device::new("intruder").unwrap();
    let run = dev.start_pairing(&qr).await.unwrap();
    status_until("pending", |s| s["pending"].is_object()).await;
    rpc("relay.reject", json!({})).await.unwrap();
    assert!(run.outcome(&mut dev, WAIT).await.is_err());
    let s = rpc("relay.status", json!({})).await.unwrap();
    assert!(s["devices"].as_array().unwrap().is_empty(), "{s}");
    // The rejected QR is spent: a second device can't use it.
    let mut other = Device::new("second").unwrap();
    assert!(other.start_pairing(&qr).await.is_err());

    // A wrong secret three times closes the rendezvous.
    let p = rpc("relay.pair", json!({})).await.unwrap();
    let good = p["qr"].as_str().unwrap().to_string();
    status_until("pairing open", |s| s["pairing"].is_object()).await;
    let mut qr = rlord_e2e::QrPayload::decode(&good).unwrap();
    qr.pairing_secret[0] ^= 1;
    let bad = qr.encode();
    for _ in 0..3 {
        let mut d = Device::new("guess").unwrap();
        assert!(d.start_pairing(&bad).await.is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    status_until("pairing closed", |s| s["pairing"].is_null()).await;
    let mut late = Device::new("late").unwrap();
    assert!(late.start_pairing(&good).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forwarder_under_another_identity_fails_on_both_ends() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let p = rpc("relay.pair", json!({})).await.unwrap();
    let qr = p["qr"].as_str().unwrap().to_string();
    status_until("connected", |s| s["connected"] == true).await;
    let own = Identity::from_seed([9; 32]).unwrap();
    let fwd = testing::forwarder(w.relay.origin(), own, w.relay.address().clone()).await;
    let mut dev = Device::new("through-a-forwarder").unwrap();
    // Pin check off and routed through the forwarder (a DNS hijack with a
    // valid certificate): the handshake still fails.
    dev.opts = Opts {
        skip_pin_check: true,
        url: Some(fwd.url()),
        ..Opts::default()
    };
    assert!(dev.start_pairing(&qr).await.is_err());
    assert_eq!(fwd.connections(), 1);
    let s = rpc("relay.status", json!({})).await.unwrap();
    assert!(
        s["pending"].is_null() && s["devices"].as_array().unwrap().is_empty(),
        "{s}"
    );
    // With the pin check on, the device stops at the challenge.
    dev.opts = Opts {
        url: Some(fwd.url()),
        ..Opts::default()
    };
    match dev.start_pairing(&qr).await {
        Err(e) => assert!(e.to_string().contains("WrongRelay"), "{e}"),
        Ok(_) => panic!("paired through a forwarder"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replayed_and_tampered_frames_get_nothing() {
    let _g = LOCK.lock().await;
    let _w = world(1).await;
    let dev = pair("replayer").await;
    // A live session; record its message 1 and first frame.
    let mut c = dev.connect().await.unwrap();
    let (ini, m1) = rlord_e2e::Initiator::reconnect(
        &dev.static_key,
        &dev.host.as_ref().unwrap().static_key,
        &c.relay,
        None,
    )
    .unwrap();
    c.send_live(m1.clone()).await.unwrap();
    let m2 = c.recv_live(WAIT).await.unwrap();
    let mut s = ini.finish(&m2).unwrap().into_session().unwrap();
    let hello = s
        .encrypt(&rlord_e2e::stream::fragment(0, br#"{"event":"hello"}"#).unwrap()[0])
        .unwrap();
    c.send_live(hello.clone()).await.unwrap();
    c.recv_live(WAIT).await.unwrap();
    let ops_before = audit().matches("op ").count();
    // A replayed frame ends the session: the host drops the link.
    c.send_live(hello.clone()).await.unwrap();
    c.wait_closed(WAIT).await.unwrap();
    // A replayed message 1 gets a message 2 and nothing else: the old
    // first frame doesn't decrypt under the new handshake.
    let mut c = dev.connect().await.unwrap();
    c.send_live(m1).await.unwrap();
    c.recv_live(WAIT).await.unwrap();
    c.send_live(hello).await.unwrap();
    c.wait_closed(WAIT).await.unwrap();
    // A flipped bit in message 1 gets nothing at all.
    let mut c = dev.connect().await.unwrap();
    let (_, mut m1) = rlord_e2e::Initiator::reconnect(
        &dev.static_key,
        &dev.host.as_ref().unwrap().static_key,
        &c.relay,
        None,
    )
    .unwrap();
    m1[40] ^= 1;
    c.send_live(m1).await.unwrap();
    c.wait_closed(WAIT).await.unwrap();
    assert_eq!(audit().matches("op ").count(), ops_before, "no op ran");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plugin_dials_out_only_when_asked() {
    let _g = LOCK.lock().await;
    env_once("remote", "rlord-test-remote");
    // A user of the local page only: no [remote], no keys.
    std::fs::write(paths::config_path(), "[web]\nport = 0\n").unwrap();
    let _ = std::fs::remove_dir_all(plugin::remote_dir());
    let relay_dir = tempfile::tempdir().unwrap();
    let relay = rlord_relay::start(rlord_relay::Config::new(relay_dir.path()))
        .await
        .unwrap();
    let rig = tokio::task::spawn_blocking(|| Rig::start(|_| {}))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let s = rpc("relay.status", json!({})).await.unwrap();
    assert_eq!(s["connected"], false, "{s}");
    assert_eq!(relay.connections(), 0);
    // Set up, but no device and no pairing: still nothing.
    let token = relay
        .admin()
        .token_new(1, Duration::from_secs(600), "local")
        .unwrap();
    rpc(
        "relay.setup",
        json!({"address": relay.address().to_string(), "token": token}),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(relay.connections(), 0);
    // A paired device keeps it connected...
    let _dev = pair("kept").await;
    assert!(relay.connections() >= 1);
    // ... until remote access is turned off, across a plugin restart.
    rpc("relay.disable", json!({})).await.unwrap();
    status_until("disconnected", |s| s["connected"] == false).await;
    rig.daemon.web().stop();
    rig.daemon.web().start(false).unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(relay.connections(), 0);
    let s = rpc("relay.status", json!({})).await.unwrap();
    assert_eq!(s["enabled"], false, "{s}");
    assert_eq!(s["devices"].as_array().map(Vec::len), Some(1), "{s}");
}

/// Starts the plugin with `RLORD_WEB_TEST_CRASH` set to `crash` (or
/// unset); its pid.
fn start_plugin(rig: &Rig, crash: Option<&str>) -> u64 {
    // Safety: under LOCK; the daemon passes its environment to the plugin.
    match crash {
        Some(c) => std::env::set_var("RLORD_WEB_TEST_CRASH", c),
        None => std::env::remove_var("RLORD_WEB_TEST_CRASH"),
    }
    let st = rig.daemon.web().start(false).unwrap();
    std::env::remove_var("RLORD_WEB_TEST_CRASH");
    st["pid"].as_u64().unwrap()
}

async fn gone(pid: u64) {
    let t = Instant::now();
    while alive(pid) {
        assert!(t.elapsed() < WAIT, "the plugin never crashed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_in_the_mailbox_never_repeats_an_op() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let mut dev = pair("crashy").await;
    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    dev.key_mailbox(&mut c, &mut live).await.unwrap();
    let t = Instant::now();
    while !audit().contains(&format!("{} mailbox keyed", dev.id())) {
        assert!(t.elapsed() < WAIT);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    c.close().await;
    let marker = format!("{} mailbox op", dev.id());
    let ops = || audit().matches(&marker).count();

    // Killed after decrypting, before saving: handled again, once.
    w.rig.daemon.web().stop();
    let mut c = dev.connect().await.unwrap();
    let req = dev
        .mailbox_request("c1", "overlord.ask", json!({"text": "one"}))
        .unwrap();
    c.send_mailbox(req).await.unwrap();
    let pid = start_plugin(&w.rig, Some("mailbox_decrypted"));
    gone(pid).await;
    assert_eq!(ops(), 0);
    start_plugin(&w.rig, None);
    let (seq, payload) = c.recv_mailbox(Duration::from_secs(20)).await.unwrap();
    assert_eq!(dev.mailbox_open(&payload).unwrap()["id"], "c1");
    c.send(|n| ToRelay::Ack {
        n,
        seq,
        device: None,
    })
    .await
    .unwrap();
    assert_eq!(ops(), 1);

    // Killed after saving, before acknowledging: the relay delivers it
    // again, the host knows it, the op never runs; a resend is a repeat.
    w.rig.daemon.web().stop();
    let req = dev
        .mailbox_request("c2", "overlord.ask", json!({"text": "two"}))
        .unwrap();
    c.send_mailbox(req).await.unwrap();
    let pid = start_plugin(&w.rig, Some("mailbox_saved"));
    gone(pid).await;
    start_plugin(&w.rig, None);
    let again = dev
        .mailbox_request("c2", "overlord.ask", json!({"text": "two"}))
        .unwrap();
    c.send_mailbox(again).await.unwrap();
    let (_, payload) = c.recv_mailbox(Duration::from_secs(20)).await.unwrap();
    let a = dev.mailbox_open(&payload).unwrap();
    assert_eq!(a["error"]["code"], "repeat", "{a}");
    assert_eq!(ops(), 1, "no op ran twice, and c2 not at all");
}
