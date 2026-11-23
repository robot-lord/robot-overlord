//! The phone as a surface, end to end: the real `rlord-web` under an
//! in-memory daemon, the real relay (`rlord_relay`, in process on
//! 127.0.0.1), and the test device (`rlord_e2e_device`). The overlord's
//! tools are called over the daemon's socket as `rlord mcp --overlord`
//! calls them; no agent runs, no tmux session is started (tmux is on a
//! scratch socket), nothing leaves loopback.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::*;
use rlord_core::{paths, plugin};
use rlord_e2e_device::{client::Conn, ConnectError, Device, Live};
use rlord_relay_proto::msg::close;

const WAIT: Duration = Duration::from_secs(10);

/// A call over the daemon's socket, as the CLI (or `rlord mcp`) makes it.
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

/// The overlord's tool `name`: its answer.
async fn tool(name: &str, args: Value) -> Value {
    rpc(
        "mcp.call",
        json!({"name": name, "arguments": args, "overlord": true}),
    )
    .await
    .unwrap_or_else(|e| panic!("{name}: {e}"))
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

/// Waits until the overlord's UI commands land on `on`.
async fn target_until(on: &str) {
    let t = Instant::now();
    loop {
        let s = rpc("ui.state", json!({})).await.unwrap();
        if s["target"] == on {
            return;
        }
        assert!(t.elapsed() < WAIT, "target never {on}: {s}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn audit() -> String {
    std::fs::read_to_string(plugin::remote_log_path()).unwrap_or_default()
}

/// A request with an id of the test's choosing: a device's ids are unique
/// to it across sessions (the host answers a repeat from its store).
async fn req(live: &mut Live, c: &mut Conn, id: &str, op: &str, args: Value) -> Value {
    live.send_json(c, &json!({"id": id, "op": op, "args": args}))
        .await
        .unwrap();
    live.answer(c, id, Duration::from_secs(30)).await.unwrap()
}

struct World {
    rig: Rig,
    _relay: rlord_relay::Running,
    _relay_dir: tempfile::TempDir,
}

/// A daemon with two sessions, its plugin, and a relay; the host set up
/// for that relay (from scratch: earlier tests' keys are reset).
async fn world() -> World {
    env_once("remote-surface", "rlord-test-remote-surface");
    let rig = tokio::task::spawn_blocking(|| {
        Rig::start(|db| {
            add(db, "alpha", None);
            add(db, "beta", None);
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
        rig,
        _relay: relay,
        _relay_dir: relay_dir,
    }
}

/// Pair `name`, confirming on the host.
async fn pair(name: &str) -> Device {
    let p = rpc("relay.pair", json!({})).await.unwrap();
    let qr = p["qr"].as_str().unwrap().to_string();
    status_until("connected for the pairing", |s| s["connected"] == true).await;
    let mut dev = Device::new(name).unwrap();
    let run = dev.start_pairing(&qr).await.unwrap();
    status_until("pending", |s| s["pending"].is_object()).await;
    rpc("relay.confirm", json!({})).await.unwrap();
    run.outcome(&mut dev, WAIT).await.unwrap();
    status_until("paired", |s| {
        s["devices"].to_string().contains(&dev.id()) && s["pending"].is_null()
    })
    .await;
    dev
}

/// Whether the plugin lists `dev`'s link as live.
fn linked(s: &Value, dev: &Device) -> bool {
    s["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| d["id"] == dev.id() && d["linked"] == true)
}

/// A device's hello carries the remote API level and its id as its page.
/// `device.leave` answers, then removes the device as revoking it does:
/// its link closes, and the relay refuses it from then on.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_leaves_by_itself() {
    let _g = LOCK.lock().await;
    let _w = world().await;
    let dev = pair("leaving").await;
    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    assert_eq!(live.hello["api"], 1, "{}", live.hello);
    assert_eq!(live.hello["page"], dev.id());
    assert_eq!(live.hello["device"], dev.id());
    // Only the caller: a device named in the arguments is ignored.
    let a = req(
        &mut live,
        &mut c,
        "leave-1",
        "device.leave",
        json!({"device": "0123456789abcdef"}),
    )
    .await;
    assert_eq!(a["ok"], json!({"left": dev.id()}), "{a}");
    let code = c.wait_closed(WAIT).await.unwrap();
    assert!(code == close::NOT_ALLOWED || code >= 1000, "{code}");
    status_until("gone", |s| !s["devices"].to_string().contains(&dev.id())).await;
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
    assert!(audit().contains(&format!("{} revoked (left", dev.id())));
}

/// A UI command for the phone goes out on its live session; once it has,
/// the next hello's `page.pending` doesn't hand it over again.
#[tokio::test(flavor = "multi_thread")]
async fn a_ui_command_reaches_the_phone_live() {
    let _g = LOCK.lock().await;
    let _w = world().await;
    let dev = pair("phone").await;
    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    // The page reports what it shows after hello: a live phone, never
    // used yet, is a surface (nothing else is here).
    let a = req(
        &mut live,
        &mut c,
        "rep-0",
        "page.report",
        json!({"route": {"kind": "list"}, "visible": true}),
    )
    .await;
    assert!(a["ok"].is_object(), "{a}");
    status_until("linked", |s| linked(s, &dev)).await;
    target_until("phone").await;
    let t = tool("show_list", json!({})).await;
    assert_eq!(t["on"], "phone", "{t}");
    let ev = live.event(&mut c, "ui", WAIT).await.unwrap();
    assert_eq!(ev["cmd"], json!({"cmd": "show_list"}), "{ev}");
    // The page answers a command with a report, as a local page does.
    let a = req(&mut live, &mut c, "rep-1", "page.report", json!({})).await;
    assert!(a["ok"].is_object(), "{a}");
    c.close().await;
    // At the next hello: nothing parked.
    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    let a = req(&mut live, &mut c, "pending-1", "page.pending", json!({})).await;
    assert_eq!(a["ok"], Value::Null, "{a}");
}

/// A command for a phone with no live session is kept for it: the relay
/// is asked to signal it, and `page.pending` hands it over at its next
/// hello, once.
#[tokio::test(flavor = "multi_thread")]
async fn a_parked_command_is_pulled_at_the_next_hello() {
    let _g = LOCK.lock().await;
    let _w = world().await;
    let dev = pair("pocket").await;
    // Used once (it tapped something), then put away.
    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    let a = req(
        &mut live,
        &mut c,
        "report-1",
        "page.report",
        json!({"route": {"kind": "list"}, "input": true}),
    )
    .await;
    assert!(a["ok"].is_object(), "{a}");
    c.close().await;
    status_until("its link gone", |s| !linked(s, &dev)).await;
    // Nothing owed it yet.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let c = dev.connect().await.unwrap();
    assert_eq!(c.ready.signal, Some(0));
    c.close().await;
    // The overlord opens a form: it is the phone's, parked.
    target_until("phone").await;
    let t = tool(
        "confirm_new_session",
        json!({"description": "from the phone", "changed": [], "note": "n"}),
    )
    .await;
    assert_eq!(t["on"], "phone", "{t}");
    let t0 = Instant::now();
    let mut c = loop {
        let c = dev.connect().await.unwrap();
        if c.ready.signal.is_some_and(|s| s > 0) {
            break c;
        }
        assert!(t0.elapsed() < WAIT, "no signal");
        c.close().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let mut live = dev.live(&mut c).await.unwrap();
    let a = req(&mut live, &mut c, "pending-1", "page.pending", json!({})).await;
    assert_eq!(a["ok"]["cmd"], "confirm_new_session", "{a}");
    assert_eq!(a["ok"]["prefill"]["description"], "from the phone");
    let a = req(&mut live, &mut c, "pending-2", "page.pending", json!({})).await;
    assert_eq!(a["ok"], Value::Null, "{a}");
}

/// A local page the user used is the target, until the phone sends a
/// form: then the overlord's answer opens on the phone, and the page
/// gets a notice saying so.
#[tokio::test(flavor = "multi_thread")]
async fn the_phone_is_the_target_after_it_sent_a_form() {
    let _g = LOCK.lock().await;
    let w = world().await;
    let dev = pair("hand").await;
    let mut c = dev.connect().await.unwrap();
    let mut live = dev.live(&mut c).await.unwrap();
    // A page, used after the phone connected.
    let (mut page, hello) = w.rig.control().await;
    assert!(hello["page"].is_string(), "{hello}");
    page_req(
        &mut page,
        json!({"id": 1, "op": "session.pin",
                               "args": {"ref": "alpha", "pinned": true}}),
    )
    .await;
    target_until("web").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The describe form, sent from the phone (no overlord here: its own
    // error, but sent all the same).
    let a = req(
        &mut live,
        &mut c,
        "form-1",
        "overlord.new_session",
        json!({"description": "a thing, from the phone"}),
    )
    .await;
    assert_ne!(a["error"]["code"], "not_allowed", "{a}");
    target_until("phone").await;
    let t = tool(
        "confirm_new_session",
        json!({"description": "a thing, from the phone", "changed": [], "note": "n"}),
    )
    .await;
    assert_eq!(t["on"], "phone", "{t}");
    let ev = live.event(&mut c, "ui", WAIT).await.unwrap();
    assert_eq!(ev["cmd"]["cmd"], "confirm_new_session", "{ev}");
    // The page hears where it went.
    let t0 = Instant::now();
    loop {
        let m = next_json(&mut page).await;
        if m["event"] == "ui" {
            assert_eq!(m["cmd"]["cmd"], "notice", "{m}");
            assert!(m["cmd"]["text"].as_str().unwrap().contains("phone"), "{m}");
            break;
        }
        assert!(t0.elapsed() < WAIT, "no notice");
    }
    let s = rpc("ui.state", json!({})).await.unwrap();
    assert_eq!(s["phone"]["open"], true, "{s}");
}

/// A local page's request; its answer.
async fn page_req(page: &mut Ws, req: Value) -> Value {
    use futures_util::SinkExt;
    page.send(tokio_tungstenite::tungstenite::Message::text(
        req.to_string(),
    ))
    .await
    .unwrap();
    loop {
        let m = next_json(page).await;
        if m["id"] == req["id"] {
            return m;
        }
    }
}
