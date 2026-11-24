//! Panes on a paired device, end to end: the real plugin under an
//! in-memory daemon, the real relay on 127.0.0.1, the low-level test
//! device (`rlord_e2e_device`), and a scratch tmux server (socket
//! `rlord-test-rpane`) whose "agent" is a small bash script. Gated:
//! `RLORD_TMUX_TESTS=1 cargo test`. Never touches `tmux -L rlord` or the
//! default server; no model is called.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::*;
use rlord_core::{paths, plugin, tmux};
use rlord_e2e_device::{Conn, Device, Live};

const SOCKET: &str = "rlord-test-rpane";
const WAIT: Duration = Duration::from_secs(10);

fn gated() -> bool {
    std::env::var("RLORD_TMUX_TESTS").as_deref() == Ok("1")
}

/// A tmux command on the scratch server; stdout, trimmed.
fn tx(args: &[&str]) -> String {
    let out = tmux::command().args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn views() -> Vec<String> {
    tx(&["list-sessions", "-F", "#{session_name}"])
        .lines()
        .filter(|s| tmux::is_view(s))
        .map(str::to_string)
        .collect()
}

fn window_size(t: &str) -> String {
    tx(&[
        "display-message",
        "-p",
        "-t",
        &format!("={t}:"),
        "#{window_width}x#{window_height}",
    ])
}

fn audit() -> String {
    std::fs::read_to_string(plugin::remote_log_path()).unwrap_or_default()
}

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

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let t = Instant::now();
    while !ok() {
        assert!(t.elapsed() < WAIT, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The "agent": says hello, echoes each line, prints its size on SIGWINCH,
/// floods on `flood N`.
const AGENT: &str = r#"exec bash -c 'trap "echo size:\$(stty size)" WINCH; echo agent-ready; while :; do IFS= read -r l || continue; case $l in flood*) yes "flood line 0123456789 abcdefghijklmnopqrstuvwxyz" | head -c ${l#flood } ; echo FLOOD-DONE;; *) echo "got:$l";; esac; done'"#;

struct World {
    _rig: Rig,
    relay: rlord_relay::Running,
    _relay_dir: tempfile::TempDir,
}

/// Sessions `alpha` and `beta` running the agent, `gamma` not running; a
/// daemon, its plugin, a relay, the host set up for it.
async fn world() -> World {
    env_once("remote-pane", SOCKET);
    tx(&["kill-server"]);
    for t in ["rp_alpha", "rp_beta"] {
        tmux::new_session(t, "/tmp", AGENT).unwrap();
    }
    let rig = tokio::task::spawn_blocking(|| {
        Rig::start(|db| {
            add(db, "alpha", Some("rp_alpha"));
            add(db, "beta", Some("rp_beta"));
            add(db, "gamma", Some("rp_gamma"));
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
    rpc(
        "relay.setup",
        json!({"address": relay.address().to_string(), "token": token, "reset": true}),
    )
    .await
    .unwrap();
    World {
        _rig: rig,
        relay,
        _relay_dir: relay_dir,
    }
}

async fn pair(name: &str) -> Device {
    let p = rpc("relay.pair", json!({})).await.unwrap();
    let qr = p["qr"].as_str().unwrap().to_string();
    status_until("connected for the pairing", |s| s["connected"] == true).await;
    let mut dev = Device::new(name).unwrap();
    let run = dev.start_pairing(&qr).await.unwrap();
    status_until("pending", |s| s["pending"].is_object()).await;
    rpc("relay.confirm", json!({})).await.unwrap();
    run.outcome(&mut dev, WAIT).await.unwrap();
    dev
}

async fn live(dev: &Device) -> (Conn, Live) {
    let mut c = dev.connect().await.unwrap();
    let l = dev.live(&mut c).await.unwrap();
    (c, l)
}

async fn open(c: &mut Conn, l: &mut Live, stream: u8, target: &str, cols: u16, rows: u16) -> Value {
    l.request(
        c,
        "pane.open",
        json!({"stream": stream, "target": target, "cols": cols, "rows": rows}),
    )
    .await
    .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Wait until the stream's unread bytes contain `needle` (acknowledging
/// as it reads).
async fn see(c: &mut Conn, l: &mut Live, s: u8, needle: &str) {
    let n = needle.to_string();
    l.pane_wait(c, s, true, WAIT, |b| text(b).contains(&n))
        .await
        .unwrap_or_else(|e| panic!("{e}: wanted {needle:?}, have {:?}", text(&l_data(l, s))));
}

fn l_data(l: &Live, s: u8) -> Vec<u8> {
    l.panes.get(&s).map(|p| p.data.clone()).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pane_opens_and_carries_bytes() {
    if !gated() {
        return;
    }
    let _g = LOCK.lock().await;
    let _w = world().await;
    let dev = pair("phone").await;
    let (mut c, mut l) = live(&dev).await;
    assert_eq!(
        l.hello["panes"],
        json!({"max": 2, "window": 65536, "frames": 48})
    );
    assert_eq!(l.hello["input"], true);
    let a = open(&mut c, &mut l, 1, "alpha", 54, 20).await;
    let ok = &a["ok"];
    assert_eq!(ok["stream"], 1, "{a}");
    assert_eq!(ok["id"], "alpha");
    assert_eq!(ok["control"], true, "a free pane goes to its opener: {a}");
    assert_eq!(ok["input"], true);
    assert_eq!(
        (ok["cols"].as_u64(), ok["rows"].as_u64()),
        (Some(54), Some(20))
    );
    assert!(ok.get("tmux_session").is_none(), "{a}");
    see(&mut c, &mut l, 1, "agent-ready").await;
    // It holds the pane: the window is laid out for it.
    wait_for("the window pinned to the device", || {
        window_size("rp_alpha") == "54x20"
    })
    .await;
    l.pane_send(&mut c, 1, b"hello from the phone\r")
        .await
        .unwrap();
    see(&mut c, &mut l, 1, "got:hello from the phone").await;
    // A second stream on the same link may not reuse an open id.
    let again = open(&mut c, &mut l, 1, "beta", 54, 20).await;
    assert_eq!(again["error"]["code"], "bad_request", "{again}");
    assert_eq!(views().len(), 1);
    assert!(audit().contains(&format!("{} pane open alpha 54x20 rw", dev.id())));
    assert!(audit().contains(&format!("{} pane input alpha", dev.id())));
}

#[tokio::test(flavor = "multi_thread")]
async fn opening_starts_nothing() {
    if !gated() {
        return;
    }
    let _g = LOCK.lock().await;
    let _w = world().await;
    let dev = pair("phone").await;
    let (mut c, mut l) = live(&dev).await;
    let before = tx(&["list-sessions", "-F", "#{session_name}"]);
    let a = open(&mut c, &mut l, 3, "gamma", 54, 20).await;
    assert_eq!(a["error"]["code"], "not_running", "{a}");
    let a = open(&mut c, &mut l, 3, "nosuch", 54, 20).await;
    assert_eq!(a["error"]["code"], "not_found", "{a}");
    for bad in [
        json!({"stream": 0, "target": "alpha", "cols": 54, "rows": 20}),
        json!({"stream": 251, "target": "alpha", "cols": 54, "rows": 20}),
        json!({"stream": 2, "target": "alpha", "cols": 1, "rows": 20}),
        json!({"stream": 2, "cols": 54, "rows": 20}),
    ] {
        let a = l.request(&mut c, "pane.open", bad.clone()).await.unwrap();
        assert_eq!(a["error"]["code"], "bad_request", "{bad}: {a}");
    }
    assert_eq!(tx(&["list-sessions", "-F", "#{session_name}"]), before);
    // The link is fine after all that.
    let s = l.request(&mut c, "snapshot", json!({})).await.unwrap();
    assert!(s["ok"]["rows"].is_array());
}

#[tokio::test(flavor = "multi_thread")]
async fn resize_reaches_the_agent_and_close_leaves_no_view() {
    if !gated() {
        return;
    }
    let _g = LOCK.lock().await;
    let _w = world().await;
    let dev = pair("phone").await;
    let (mut c, mut l) = live(&dev).await;
    open(&mut c, &mut l, 1, "alpha", 54, 20).await;
    see(&mut c, &mut l, 1, "agent-ready").await;
    l.pane_event(&mut c, 1, "resize", json!({"cols": 60, "rows": 30}))
        .await
        .unwrap();
    wait_for("pinned again", || window_size("rp_alpha") == "60x30").await;
    // The agent got SIGWINCH (bash runs the trap once its read returns).
    l.pane_send(&mut c, 1, b"after the resize\r").await.unwrap();
    see(&mut c, &mut l, 1, "size:30 60").await;
    l.pane_event(&mut c, 1, "close", json!({})).await.unwrap();
    let ev = l.event(&mut c, "pane", WAIT).await.unwrap();
    assert_eq!(ev["type"], "closed");
    assert_eq!(ev["reason"], "closed");
    wait_for("no view left", || views().is_empty()).await;
    // Its id may be used again once closed.
    let a = open(&mut c, &mut l, 1, "alpha", 54, 20).await;
    assert_eq!(a["ok"]["stream"], 1, "{a}");
    assert!(audit().contains(&format!("{} pane close alpha in=17 out=", dev.id())));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_link_leaves_no_view_and_a_reopen_draws_the_screen() {
    if !gated() {
        return;
    }
    let _g = LOCK.lock().await;
    let _w = world().await;
    let dev = pair("phone").await;
    let (mut c, mut l) = live(&dev).await;
    open(&mut c, &mut l, 1, "alpha", 54, 20).await;
    see(&mut c, &mut l, 1, "agent-ready").await;
    l.pane_send(&mut c, 1, b"before the drop\r").await.unwrap();
    see(&mut c, &mut l, 1, "got:before the drop").await;
    c.close().await;
    wait_for("the link's view gone", || views().is_empty()).await;
    // Reopen on a fresh session: tmux draws the whole screen first.
    let (mut c, mut l) = live(&dev).await;
    open(&mut c, &mut l, 1, "alpha", 54, 20).await;
    see(&mut c, &mut l, 1, "got:before the drop").await;
    let s = text(&l_data(&l, 1));
    assert!(s.contains("agent-ready"), "{s:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flood_never_drops_the_link_nor_delays_control() {
    if !gated() {
        return;
    }
    let _g = LOCK.lock().await;
    let w = world().await;
    let dev = pair("phone").await;
    let other = pair("tablet").await;
    let (mut c, mut l) = live(&dev).await;
    let (mut c2, mut l2) = live(&other).await;
    open(&mut c, &mut l, 1, "alpha", 54, 20).await;
    see(&mut c, &mut l, 1, "agent-ready").await;
    // 50 MB through the pane, and the device stops acknowledging.
    let base = l.panes.get(&1).map_or(0, |p| p.bytes);
    l.pane_send(&mut c, 1, b"flood 50000000\r").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Control is answered promptly on the flooded link ...
    let t = Instant::now();
    let s = l.request(&mut c, "snapshot", json!({})).await.unwrap();
    assert!(s["ok"]["rows"].is_array(), "{s}");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    // ... and on another device's.
    let t = Instant::now();
    let s = l2.request(&mut c2, "snapshot", json!({})).await.unwrap();
    assert!(s["ok"]["rows"].is_array(), "{s}");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    // Unacknowledged, the host stopped at the window.
    let got = l.panes.get(&1).map_or(0, |p| p.bytes) - base;
    assert!(got <= 64 * 1024, "{got}");
    // The tmux server stays small while the device sleeps (gate 1a).
    let pid = tx(&["display-message", "-p", "#{pid}"]);
    let rss = || {
        let o = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            .unwrap();
        text(&o.stdout).trim().parse::<u64>().unwrap_or(0)
    };
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(rss() < 64 * 1024, "tmux server rss {} KiB", rss());
    // Catching up: the flood ends in a whole screen, the link alive.
    l.pane_ack(&mut c, 1).await.unwrap();
    l.pane_wait(&mut c, 1, true, Duration::from_secs(60), |b| {
        text(b).contains("FLOOD-DONE")
    })
    .await
    .unwrap();
    let total = l.panes.get(&1).unwrap().bytes;
    assert!(
        total < 20_000_000,
        "the phone was sent {total} bytes of 50 MB"
    );
    let s = l.request(&mut c, "snapshot", json!({})).await.unwrap();
    assert!(s["ok"]["rows"].is_array());
    drop(w);
}

#[tokio::test(flavor = "multi_thread")]
async fn control_between_two_devices() {
    if !gated() {
        return;
    }
    let _g = LOCK.lock().await;
    let _w = world().await;
    let a = pair("phone").await;
    let b = pair("tablet").await;
    let (mut ca, mut la) = live(&a).await;
    let (mut cb, mut lb) = live(&b).await;
    let oa = open(&mut ca, &mut la, 1, "alpha", 54, 20).await;
    assert_eq!(oa["ok"]["control"], true, "{oa}");
    see(&mut ca, &mut la, 1, "agent-ready").await;
    // The second opens as a viewer, at the window's size.
    let ob = open(&mut cb, &mut lb, 1, "alpha", 80, 30).await;
    assert_eq!(ob["ok"]["control"], false, "{ob}");
    assert_eq!(
        (ob["ok"]["cols"].as_u64(), ob["ok"]["rows"].as_u64()),
        (Some(54), Some(20)),
        "{ob}"
    );
    assert_eq!(window_size("rp_alpha"), "54x20");
    see(&mut cb, &mut lb, 1, "agent-ready").await;
    // A viewer's input goes nowhere.
    lb.pane_send(&mut cb, 1, b"from the viewer\r")
        .await
        .unwrap();
    la.pane_send(&mut ca, 1, b"from the holder\r")
        .await
        .unwrap();
    see(&mut ca, &mut la, 1, "got:from the holder").await;
    assert!(!text(&l_data(&la, 1)).contains("got:from the viewer"));
    // Take over: resized to the taker, typing swaps.
    let t = lb
        .request(
            &mut cb,
            "pane.take",
            json!({"target": "alpha", "cols": 80, "rows": 30}),
        )
        .await
        .unwrap();
    assert_eq!(t["ok"]["control"], true, "{t}");
    wait_for("pinned to the taker", || window_size("rp_alpha") == "80x30").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    lb.pane_send(&mut cb, 1, b"after the take\r").await.unwrap();
    see(&mut cb, &mut lb, 1, "got:after the take").await;
    la.pane_send(&mut ca, 1, b"from the old holder\r")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    lb.pane_wait(&mut cb, 1, true, Duration::from_millis(300), |_| false)
        .await
        .ok();
    assert!(!text(&l_data(&lb, 1)).contains("got:from the old holder"));
    assert!(audit().contains(&format!("{} pane take alpha", b.id())));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_view_only_device_cannot_type_or_take() {
    if !gated() {
        return;
    }
    let _g = LOCK.lock().await;
    let _w = world().await;
    let dev = pair("phone").await;
    rpc("relay.input", json!({"device": dev.id(), "input": false}))
        .await
        .unwrap();
    let (mut c, mut l) = live(&dev).await;
    assert_eq!(l.hello["input"], false, "{}", l.hello);
    let a = open(&mut c, &mut l, 1, "alpha", 54, 20).await;
    assert_eq!(a["ok"]["input"], false, "{a}");
    assert_eq!(
        a["ok"]["control"], false,
        "a view-only device never takes: {a}"
    );
    see(&mut c, &mut l, 1, "agent-ready").await;
    l.pane_send(&mut c, 1, b"typed anyway\r").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t = l
        .request(
            &mut c,
            "pane.take",
            json!({"target": "alpha", "cols": 54, "rows": 20}),
        )
        .await
        .unwrap();
    assert_eq!(t["error"]["code"], "not_allowed", "{t}");
    l.pane_wait(&mut c, 1, true, Duration::from_millis(300), |_| false)
        .await
        .ok();
    assert!(!text(&l_data(&l, 1)).contains("got:typed anyway"));
    // Attached at the window's size (it doesn't hold the pane), read-only.
    let log = audit();
    let line = log
        .lines()
        .find(|l| l.contains(&format!("{} pane open alpha ", dev.id())))
        .unwrap_or_else(|| panic!("{log}"));
    assert!(line.ends_with(" ro"), "{line}");
}

#[tokio::test(flavor = "multi_thread")]
async fn too_many_panes_and_revoking_ends_them() {
    if !gated() {
        return;
    }
    let _g = LOCK.lock().await;
    let _w = world().await;
    let dev = pair("phone").await;
    let (mut c, mut l) = live(&dev).await;
    assert!(open(&mut c, &mut l, 1, "alpha", 54, 20).await["ok"].is_object());
    assert!(open(&mut c, &mut l, 2, "beta", 54, 20).await["ok"].is_object());
    let a = open(&mut c, &mut l, 3, "alpha", 54, 20).await;
    assert_eq!(a["error"]["code"], "too_many", "{a}");
    wait_for("two views", || views().len() == 2).await;
    rpc("relay.revoke", json!({"device": dev.id()}))
        .await
        .unwrap();
    wait_for("its views gone", || views().is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_restart_reopens_and_the_audit_log_names_no_bytes() {
    if !gated() {
        return;
    }
    let _g = LOCK.lock().await;
    let w = world().await;
    let dev = pair("phone").await;
    let (mut c, mut l) = live(&dev).await;
    open(&mut c, &mut l, 1, "alpha", 54, 20).await;
    see(&mut c, &mut l, 1, "agent-ready").await;
    l.pane_send(&mut c, 1, b"secret-words-here\r")
        .await
        .unwrap();
    see(&mut c, &mut l, 1, "got:secret-words-here").await;
    // The relay goes and comes back (same address and key).
    let dir = w._relay_dir.path().to_path_buf();
    let addr = w.relay.addr();
    w.relay.shutdown().await;
    wait_for("the link's view gone", || views().is_empty()).await;
    let mut cfg = rlord_relay::Config::new(&dir);
    cfg.listen = addr;
    let _relay = rlord_relay::start(cfg).await.unwrap();
    let t = Instant::now();
    let (mut c, mut l) = loop {
        if let Ok(mut c) = dev.connect().await {
            if let Ok(l) = dev.live(&mut c).await {
                break (c, l);
            }
        }
        assert!(t.elapsed() < Duration::from_secs(30), "never reconnected");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    open(&mut c, &mut l, 1, "alpha", 54, 20).await;
    see(&mut c, &mut l, 1, "got:secret-words-here").await;
    let log = audit();
    assert!(log.contains("pane open alpha"), "{log}");
    assert!(!log.contains("secret-words-here"), "{log}");
}
