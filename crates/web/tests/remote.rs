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
    world_with(n, |_| {}).await
}

/// `world`, with the relay's config adjusted by `f`.
async fn world_with(n: usize, f: impl FnOnce(&mut rlord_relay::Config)) -> World {
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
    let mut cfg = rlord_relay::Config::new(relay_dir.path());
    f(&mut cfg);
    let relay = rlord_relay::start(cfg).await.unwrap();
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
    assert!(!s["devices"].to_string().contains(&dev.id()), "{s}");
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
        "device.revoke",
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
    // Nothing for it yet. The pairing moved the daemon's rev (its relay
    // status lists the device now), and so does this device's own live
    // session coming and going, but no device sees that: no signal, given
    // time for the plugin to hear of each.
    status_until("the device listed", |s| {
        s["devices"].to_string().contains(&dev.id()) && s["pending"].is_null()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut c = dev.connect().await.unwrap();
    assert_eq!(c.ready.signal, Some(0));
    let boot = c.ready.boot.clone();
    dev.live(&mut c).await.unwrap();
    status_until("its link listed", |s| {
        s["devices"].to_string().contains(r#""linked":true"#)
    })
    .await;
    c.close().await;
    status_until("its link gone", |s| {
        !s["devices"].to_string().contains(r#""linked":true"#)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let c = dev.connect().await.unwrap();
    assert_eq!(c.ready.signal, Some(0));
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
    // The relay says it is queued.
    assert_eq!(c.queue_mailbox(req).await.unwrap(), 1);
    // The plugin comes back, fetches the queued request and answers it.
    w.rig.daemon.web().start(false).unwrap();
    let (seq, payload) = c.recv_mailbox(Duration::from_secs(20)).await.unwrap();
    let boot = c.ready.boot.clone();
    let answer = dev.mailbox_receive(&boot, seq, &payload).unwrap().unwrap();
    assert_eq!(answer["id"], "m1", "{answer}");
    assert_ne!(answer["error"]["code"], "not_allowed");
    // A frame delivered again is recognised, not opened twice.
    assert_eq!(dev.mailbox_receive(&boot, seq, &payload).unwrap(), None);
    c.send(|n| ToRelay::Ack {
        n,
        seq,
        device: None,
    })
    .await
    .unwrap();
    // The same request again is not acted on twice: it gets the stored
    // answer.
    let again = dev
        .mailbox_request("m1", "overlord.ask", json!({"text": "while you were away"}))
        .unwrap();
    c.send_mailbox(again).await.unwrap();
    let (_, payload) = c.recv_mailbox(WAIT).await.unwrap();
    assert_eq!(dev.mailbox_open(&payload).unwrap(), answer);
    let marker = format!("{} mailbox op overlord.ask", dev.id());
    assert_eq!(audit().matches(&marker).count(), 1);
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
    // again, the host knows it, the op never runs; a resend is told it was
    // interrupted (the host can't know whether it happened).
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
    assert_eq!(a["error"]["code"], "interrupted", "{a}");
    assert_eq!(ops(), 1, "no op ran twice, and c2 not at all");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mailbox_frame_that_cant_be_saved_is_handled_when_it_comes_again() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let mut dev = pair("unsaved").await;
    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    dev.key_mailbox(&mut c, &mut live).await.unwrap();
    keyed(&dev).await;
    c.close().await;
    let marker = format!("{} mailbox op", dev.id());
    let ops = || audit().matches(&marker).count();

    // Queued while the plugin is off; the plugin's first save of it fails,
    // so it is left unacknowledged at the relay.
    w.rig.daemon.web().stop();
    let mut c = dev.connect().await.unwrap();
    let req = dev
        .mailbox_request("u1", "overlord.ask", json!({"text": "one"}))
        .unwrap();
    c.send_mailbox(req).await.unwrap();
    // Safety: under LOCK; the daemon passes its environment to the plugin.
    std::env::set_var("RLORD_WEB_TEST_FAIL", "mailbox_frame_save");
    w.rig.daemon.web().start(false).unwrap();
    std::env::remove_var("RLORD_WEB_TEST_FAIL");
    let t = Instant::now();
    while !audit().contains(&format!("{} mailbox frame not saved", dev.id())) {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "the save never failed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(ops(), 0);

    // The same plugin reconnects: the relay delivers the frame again, and
    // it is handled as new (not taken for done and dropped), once.
    rpc("relay.disable", json!({})).await.unwrap();
    status_until("disconnected", |s| s["connected"] == false).await;
    rpc(
        "relay.setup",
        json!({"address": w.relay.address().to_string()}),
    )
    .await
    .unwrap();
    let a = mailbox_answer(&mut dev, &mut c).await;
    assert_eq!(a["id"], "u1", "{a}");
    assert_ne!(a["error"]["code"], "interrupted", "{a}");
    assert_eq!(ops(), 1);
}

/// Waits until the device's mailbox is keyed at the host.
async fn keyed(dev: &Device) {
    let t = Instant::now();
    while !audit().contains(&format!("{} mailbox keyed", dev.id())) {
        assert!(t.elapsed() < WAIT, "mailbox never keyed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The next mailbox answer that isn't `in_progress`.
async fn mailbox_answer(dev: &mut Device, c: &mut rlord_e2e_device::Conn) -> Value {
    loop {
        let (seq, payload) = c.recv_mailbox(Duration::from_secs(20)).await.unwrap();
        let boot = c.ready.boot.clone();
        let Some(a) = dev.mailbox_receive(&boot, seq, &payload).unwrap() else {
            continue;
        };
        c.send(|n| ToRelay::Ack {
            n,
            seq,
            device: None,
        })
        .await
        .unwrap();
        if a["error"]["code"] != "in_progress" {
            return a;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_resent_by_mailbox_after_its_link_died_runs_once() {
    let _g = LOCK.lock().await;
    let _w = world(1).await;
    let mut dev = pair("commuter").await;
    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    dev.key_mailbox(&mut c, &mut live).await.unwrap();
    keyed(&dev).await;
    let live_ops = format!("{} op overlord.ask", dev.id());
    let mailbox_ops = format!("{} mailbox op overlord.ask", dev.id());
    let count = |m: &str| audit().matches(m).count();

    // Asked live, and the link dies before the answer is read.
    live.send_json(
        &mut c,
        &json!({"id": "x1", "op": "overlord.ask", "args": {"text": "on the train"}}),
    )
    .await
    .unwrap();
    let t = Instant::now();
    while count(&live_ops) == 0 {
        assert!(t.elapsed() < WAIT, "the op never ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    c.close().await;

    // Resent by mailbox with the same id: the host's store answers it.
    let mut c = dev.connect().await.unwrap();
    let req = dev
        .mailbox_request("x1", "overlord.ask", json!({"text": "on the train"}))
        .unwrap();
    c.queue_mailbox(req).await.unwrap();
    let a = mailbox_answer(&mut dev, &mut c).await;
    assert_eq!(a["id"], "x1", "{a}");
    assert!(
        a.get("ok").is_some() || a["error"]["code"] != "interrupted",
        "{a}"
    );

    // And live again, same id: the same answer, still one run.
    let mut live = dev.live(&mut c).await.unwrap();
    live.send_json(
        &mut c,
        &json!({"id": "x1", "op": "overlord.ask", "args": {"text": "on the train"}}),
    )
    .await
    .unwrap();
    let again = live.answer(&mut c, "x1", WAIT).await.unwrap();
    assert_eq!(again, a);
    // Without an id, an op that changes something is refused, not run.
    live.send_json(
        &mut c,
        &json!({"op": "overlord.ask", "args": {"text": "no id"}}),
    )
    .await
    .unwrap();
    let v = loop {
        let v = live.recv_json(&mut c, WAIT).await.unwrap();
        if v.get("error").is_some() {
            break v;
        }
    };
    assert_eq!(v["error"]["code"], "bad_request", "{v}");
    assert_eq!(count(&live_ops), 1, "ran once");
    assert_eq!(count(&mailbox_ops), 0, "never by mailbox");
    assert!(audit().contains(&format!("{} repeat overlord.ask", dev.id())));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_restart_loses_the_queue_and_a_resend_still_works() {
    let _g = LOCK.lock().await;
    let World {
        rig,
        relay,
        _relay_dir: relay_dir,
    } = world(1).await;
    let mut dev = pair("restart").await;
    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    dev.key_mailbox(&mut c, &mut live).await.unwrap();
    keyed(&dev).await;
    c.close().await;

    // The host is away; a request waits at the relay...
    rig.daemon.web().stop();
    let mut c = dev.connect().await.unwrap();
    let req = dev
        .mailbox_request("r1", "overlord.ask", json!({"text": "lost"}))
        .unwrap();
    c.queue_mailbox(req).await.unwrap();
    let boot = c.ready.boot.clone();
    // ... and the relay restarts (same port, same key), forgetting it.
    let addr = relay.addr();
    relay.shutdown().await;
    let mut cfg = rlord_relay::Config::new(relay_dir.path());
    cfg.listen = addr;
    let relay = rlord_relay::start(cfg).await.unwrap();
    let mut c = dev.connect().await.unwrap();
    assert_ne!(c.ready.boot, boot);
    assert_eq!(c.ready.queued, Some(0));
    // The device sees the new boot with r1 unanswered: it resends r1 (a
    // newer counter; the host skips the lost one).
    let req = dev
        .mailbox_request("r1", "overlord.ask", json!({"text": "lost"}))
        .unwrap();
    c.queue_mailbox(req).await.unwrap();
    rig.daemon.web().start(false).unwrap();
    let a = mailbox_answer(&mut dev, &mut c).await;
    assert_eq!(a["id"], "r1", "{a}");
    assert_eq!(
        audit()
            .matches(&format!("{} mailbox op overlord.ask", dev.id()))
            .count(),
        1
    );
    assert!(!audit().contains(&format!("{} mailbox discarded", dev.id())));
    drop(relay);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_flooding_its_link_never_breaks_anothers() {
    let _g = LOCK.lock().await;
    // Snapshots of several frames each.
    let w = world(300).await;
    let flood = pair("flood").await;
    let calm = pair("calm").await;
    let mut cf = flood.connect().await.unwrap();
    let mut lf = flood.live(&mut cf).await.unwrap();
    let mut cc = calm.connect().await.unwrap();
    let mut lc = calm.live(&mut cc).await.unwrap();

    // One device asks for far more than it reads (it reads nothing).
    let spam = tokio::spawn(async move {
        for i in 0..200 {
            let r = json!({"id": format!("s{i}"), "op": "snapshot", "args": {}});
            if lf.send_json(&mut cf, &r).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cf
    });
    // Meanwhile the other's session keeps working, promptly.
    for _ in 0..5 {
        let t = Instant::now();
        let snap = lc.request(&mut cc, "snapshot", json!({})).await.unwrap();
        assert_eq!(snap["ok"]["rows"].as_array().map(Vec::len), Some(300));
        assert!(t.elapsed() < WAIT, "{:?}", t.elapsed());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The flooding device's link is the one that ends: told to reconnect
    // (4205), or, since it read nothing, cut off before that close frame
    // could reach it (1006).
    let mut cf = spam.await.unwrap();
    let code = cf.wait_closed(Duration::from_secs(30)).await.unwrap();
    assert!(code == close::RESET || code == 1006, "{code}");
    // The host's relay connection never dropped: the calm session is the
    // same one, still answering and still told of changes.
    let snap = lc.request(&mut cc, "snapshot", json!({})).await.unwrap();
    assert!(snap["ok"]["rows"].is_array(), "{snap}");
    lc.inbox.clear();
    rpc(
        "session.rename",
        json!({"ref": format!("session-0002-{}", "x".repeat(40)), "name": "still"}),
    )
    .await
    .unwrap();
    lc.event(&mut cc, "changed", WAIT).await.unwrap();
    drop(w);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_refused_by_a_full_queue_is_sent_again() {
    let _g = LOCK.lock().await;
    // Queues of one message: the second answer finds the device's full.
    let _w = world_with(1, |c| c.limits.mailbox_messages = 1).await;
    let mut dev = pair("full").await;
    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    dev.key_mailbox(&mut c, &mut live).await.unwrap();
    keyed(&dev).await;
    c.close().await;

    let mut c = dev.connect().await.unwrap();
    let boot = c.ready.boot.clone();
    let req = dev
        .mailbox_request("f1", "overlord.ask", json!({"text": "one"}))
        .unwrap();
    c.queue_mailbox(req).await.unwrap();
    // f1's answer arrives; the device doesn't acknowledge it yet, so it
    // fills its queue at the relay.
    let (s1, p1) = c.recv_mailbox(WAIT).await.unwrap();
    assert_eq!(
        dev.mailbox_receive(&boot, s1, &p1).unwrap().unwrap()["id"],
        "f1"
    );
    let req = dev
        .mailbox_request("f2", "overlord.ask", json!({"text": "two"}))
        .unwrap();
    c.queue_mailbox(req).await.unwrap();
    // f2 ran; its answer was refused (`full`) and is kept.
    let t = Instant::now();
    let ran = || {
        audit()
            .matches(&format!("{} mailbox op overlord.ask", dev.id()))
            .count()
    };
    while ran() < 2 {
        assert!(t.elapsed() < WAIT, "f2 never ran");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        c.quiet(Duration::from_millis(500)).await,
        "f2's answer got through a full queue"
    );
    // The device acknowledges f1 and comes back: the host offers f2's
    // answer again when it links, the same bytes.
    c.send(|n| ToRelay::Ack {
        n,
        seq: s1,
        device: None,
    })
    .await
    .unwrap();
    c.wait_for(WAIT, |m| matches!(m, FromRelay::Ok { .. }))
        .await
        .unwrap();
    c.close().await;
    let mut c = dev.connect().await.unwrap();
    let a = mailbox_answer(&mut dev, &mut c).await;
    assert_eq!(a["id"], "f2", "{a}");
    assert!(!audit().contains("not delivered"));
}
