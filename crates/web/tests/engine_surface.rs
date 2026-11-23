//! The phone as a surface, through its own engine (`rlord_e2e_client`,
//! driven by `rlord_e2e_device::Phone` as the phone page drives it: page
//! messages in, page events out). The page's part is played by the test
//! as `phoneApp.ts` plays it: a `ui` event comes in, the page answers
//! with `page.report`; at each hello it asks `page.pending`. The real
//! `rlord-web` under an in-memory daemon, the real relay in process on
//! 127.0.0.1 behind a `Tamper` that only watches here. The overlord's
//! tools are called over the daemon's socket as `rlord mcp --overlord`
//! calls them; no agent runs, no tmux session is started (tmux is on a
//! scratch socket), nothing leaves loopback.

mod common;
mod world;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use common::{next_json, Ws, LOCK};
use rlord_e2e_device::testing::Dir;
use rlord_e2e_device::Phone;
use rlord_relay_proto::{FromRelay, Role, ToRelay};
use world::*;

const LONG: Duration = Duration::from_secs(30);

/// The overlord's tool `name`: its answer.
async fn tool(name: &str, args: Value) -> Value {
    rpc(
        "mcp.call",
        json!({"name": name, "arguments": args, "overlord": true}),
    )
    .await
    .unwrap_or_else(|e| panic!("{name}: {e}"))
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

/// Whether the plugin lists device `id`'s link as live.
fn linked(s: &Value, id: &str) -> bool {
    s["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|d| d["id"] == id && d["linked"] == true)
}

/// The last `event` the engine sent the page.
fn last(phone: &mut Phone, event: &str) -> Value {
    phone.drain();
    phone
        .log
        .iter()
        .rev()
        .find(|v| v["event"] == event)
        .cloned()
        .unwrap_or_else(|| panic!("no {event}"))
}

/// What the `Tamper` saw of the relay's signal: the host asking for one,
/// each `ready` the device got (its `signal`), the device's acks.
#[derive(Default)]
struct Signals {
    asked: usize,
    ready: Vec<Option<u64>>,
    acked: usize,
}

fn watch_signals(w: &World) -> Arc<Mutex<Signals>> {
    let seen = Arc::new(Mutex::new(Signals::default()));
    let s2 = seen.clone();
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if let Message::Text(t) = &m {
            let mut s = s2.lock().unwrap();
            match (p.role, dir) {
                (Role::Host, Dir::Up) => {
                    if let Ok(ToRelay::Signal { .. }) = ToRelay::parse(t) {
                        s.asked += 1;
                    }
                }
                (Role::Device, Dir::Down) => {
                    if let Ok(FromRelay::Ready(r)) = FromRelay::parse(t) {
                        s.ready.push(r.signal);
                    }
                }
                (Role::Device, Dir::Up) => {
                    if let Ok(ToRelay::AckSignal { .. }) = ToRelay::parse(t) {
                        s.acked += 1;
                    }
                }
                _ => {}
            }
        }
        vec![m]
    }));
    seen
}

/// Waits until `pick` holds of what the `Tamper` saw.
async fn signals_until(seen: &Mutex<Signals>, what: &str, pick: impl Fn(&Signals) -> bool) {
    let t = Instant::now();
    while !pick(&seen.lock().unwrap()) {
        assert!(t.elapsed() < WAIT, "never {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Puts the phone away (the link closes) and waits until the host has
/// seen it go.
async fn put_away(phone: &mut Phone, id: &str) {
    phone.engine.lifecycle(false);
    phone.conn("connecting", WAIT).await.unwrap();
    status_until("its link gone", |s| !linked(s, id)).await;
}

/// Brings the phone back; the hello the page gets.
async fn bring_back(phone: &mut Phone) -> Value {
    phone.mark();
    phone.engine.lifecycle(true);
    phone
        .wait("a hello", LONG, |v| v["event"] == "hello")
        .await
        .unwrap()
}

/// The engine's look-ups are the daemon's idempotent remote ops: the
/// host stores no answer for those, so a resend by id would run one
/// again; and an op the host stores is one a lost answer must not lose.
#[test]
fn the_engines_lookups_are_the_daemons_idempotent_ops() {
    for op in rlord_daemon::web::OPS.iter().filter(|o| o.remote) {
        assert_eq!(
            rlord_e2e_client::outbox::is_lookup(op.op),
            op.idempotent,
            "{}",
            op.op
        );
    }
}

/// The hello names the remote API level and the device as its page. The
/// user unpairs on the phone: the engine says `device.leave`, the host
/// answers and removes the device as revoking it does, the engine
/// forgets the host, and the relay refuses that identity from then on.
#[tokio::test(flavor = "multi_thread")]
async fn a_phone_leaves_by_itself() {
    let _g = LOCK.lock().await;
    let _w = world(1).await;
    let (mut phone, dir) = pair("leaving").await;
    let id = device_id(&mut phone).await;
    keyed(&id, 1).await;
    let hello = last(&mut phone, "hello");
    assert_eq!(hello["api"], 1, "{hello}");
    assert_eq!(hello["page"], id.as_str(), "{hello}");
    assert_eq!(hello["device"], id.as_str(), "{hello}");
    // Its folder (keys and host record), copied before it leaves.
    let copy = tempfile::tempdir().unwrap();
    copy_dir(dir.path(), copy.path());
    phone.say(json!({"app": "unpair"}));
    phone.conn("unpaired", WAIT).await.unwrap();
    let c = last(&mut phone, "conn");
    assert!(
        c["detail"]
            .as_str()
            .is_some_and(|d| d.contains("has removed this phone")),
        "the host's answer came: {c}"
    );
    assert!(rlord_e2e_client::store::host_files(dir.path()).is_empty());
    status_until("gone", |s| !s["devices"].to_string().contains(&id)).await;
    assert!(
        audit().contains(&format!("{id} revoked (left")),
        "{}",
        audit()
    );
    // The same identity again: refused (4403), and that engine forgets
    // the host too.
    let mut twin = Phone::open(copy.path()).unwrap();
    twin.engine.lifecycle(true);
    twin.conn("revoked", LONG).await.unwrap();
    assert!(rlord_e2e_client::store::host_files(copy.path()).is_empty());
}

/// A UI command for the phone reaches its page live, as a `ui` event; the
/// page's report (a look-up: never in the outbox) says it got it, so the
/// next hello's `page.pending` doesn't hand it over again.
#[tokio::test(flavor = "multi_thread")]
async fn a_ui_command_reaches_the_phone_live() {
    let _g = LOCK.lock().await;
    let _w = world(2).await;
    let (mut phone, _dir) = pair("phone").await;
    let id = device_id(&mut phone).await;
    // The page reports what it shows after hello: a live phone, never
    // used yet, is a surface (nothing else is here).
    let a = phone
        .request(
            "page.report",
            json!({"route": {"kind": "list"}, "visible": true}),
            WAIT,
        )
        .await
        .unwrap();
    assert!(a["ok"].is_object(), "{a}");
    status_until("linked", |s| linked(s, &id)).await;
    target_until("phone").await;
    phone.mark();
    let t = tool("show_list", json!({})).await;
    assert_eq!(t["on"], "phone", "{t}");
    let ev = phone
        .wait("the ui command", WAIT, |v| v["event"] == "ui")
        .await
        .unwrap();
    assert_eq!(ev["cmd"], json!({"cmd": "show_list"}), "{ev}");
    // The page answers a command with a report (`phoneApp.ts`).
    let a = phone.request("page.report", json!({}), WAIT).await.unwrap();
    assert!(a["ok"].is_object(), "{a}");
    assert!(
        !phone
            .log
            .iter()
            .any(|v| v["event"] == "outbox" && v.to_string().contains("page.report")),
        "a report is a look-up"
    );
    // At the next hello: nothing parked.
    put_away(&mut phone, &id).await;
    bring_back(&mut phone).await;
    let a = phone
        .request("page.pending", json!({}), WAIT)
        .await
        .unwrap();
    assert_eq!(a["ok"], Value::Null, "{a}");
}

/// A command for a phone with no live session is kept for it: the host
/// asks the relay to signal it, the engine's next link is told so, and
/// the page's `page.pending` at the next hello takes it, once. The page's
/// first look-up after that clears the relay's flag.
#[tokio::test(flavor = "multi_thread")]
async fn a_parked_command_is_pulled_at_the_next_hello() {
    let _g = LOCK.lock().await;
    let w = world(2).await;
    let (mut phone, _dir) = pair("pocket").await;
    let id = device_id(&mut phone).await;
    // Used once (it tapped something), then put away.
    let a = phone
        .request(
            "page.report",
            json!({"route": {"kind": "list"}, "input": true}),
            WAIT,
        )
        .await
        .unwrap();
    assert!(a["ok"].is_object(), "{a}");
    let seen = watch_signals(&w);
    put_away(&mut phone, &id).await;
    // Nothing owed it yet.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(seen.lock().unwrap().asked, 0);
    // The overlord opens a form: it is the phone's, parked.
    target_until("phone").await;
    let t = tool(
        "confirm_new_session",
        json!({"description": "from the phone", "changed": [], "note": "n"}),
    )
    .await;
    assert_eq!(t["on"], "phone", "{t}");
    signals_until(&seen, "the host's signal", |s| s.asked >= 1).await;
    bring_back(&mut phone).await;
    assert_eq!(seen.lock().unwrap().ready.last(), Some(&Some(1)));
    let a = phone
        .request("page.pending", json!({}), WAIT)
        .await
        .unwrap();
    assert_eq!(a["ok"]["cmd"], "confirm_new_session", "{a}");
    assert_eq!(a["ok"]["prefill"]["description"], "from the phone", "{a}");
    let a = phone
        .request("page.pending", json!({}), WAIT)
        .await
        .unwrap();
    assert_eq!(a["ok"], Value::Null, "{a}");
    // Pulled, never pushed.
    assert!(!phone.log.iter().any(|v| v["event"] == "ui"));
    let a = phone.request("snapshot", json!({}), WAIT).await.unwrap();
    assert!(a["ok"].is_object(), "{a}");
    signals_until(&seen, "the device's ack", |s| s.acked >= 1).await;
}

/// A local page the user used is the target, until the phone sends a
/// form: then the overlord's answer opens on the phone (a `ui` event to
/// its page), and the local page gets a notice saying so.
#[tokio::test(flavor = "multi_thread")]
async fn the_phone_is_the_target_after_it_sent_a_form() {
    let _g = LOCK.lock().await;
    let w = world(2).await;
    let (mut phone, _dir) = pair("hand").await;
    // A page, used after the phone connected.
    let (mut page, hello) = w.rig.control().await;
    assert!(hello["page"].is_string(), "{hello}");
    page_req(
        &mut page,
        json!({"id": 1, "op": "session.pin",
               "args": {"ref": "session-0000", "pinned": true}}),
    )
    .await;
    target_until("web").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The describe form, sent from the phone (no overlord here: its own
    // error, but sent all the same).
    let a = phone
        .request(
            "overlord.new_session",
            json!({"description": "a thing, from the phone"}),
            WAIT,
        )
        .await
        .unwrap();
    assert_ne!(a["error"]["code"], "not_allowed", "{a}");
    target_until("phone").await;
    phone.mark();
    let t = tool(
        "confirm_new_session",
        json!({"description": "a thing, from the phone", "changed": [], "note": "n"}),
    )
    .await;
    assert_eq!(t["on"], "phone", "{t}");
    let ev = phone
        .wait("the form", WAIT, |v| v["event"] == "ui")
        .await
        .unwrap();
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
    page.send(Message::text(req.to_string())).await.unwrap();
    loop {
        let m = next_json(page).await;
        if m["id"] == req["id"] {
            return m;
        }
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let p = e.path();
        let dest = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &dest);
        } else {
            std::fs::copy(&p, &dest).unwrap();
        }
    }
}
