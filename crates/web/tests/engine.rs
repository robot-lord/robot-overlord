//! The phone's device engine (`rlord_e2e_client`, driven through
//! `rlord_e2e_device::Phone` as the page drives it) end to end: the real
//! `rlord-web` under an in-memory daemon, the real relay in process on
//! 127.0.0.1, and a `Flaky` proxy in front of it for cuts, stalls and
//! outages. No tmux session is started; no agent runs; nothing leaves
//! loopback.

mod common;
mod world;

use std::time::{Duration, Instant};

use serde_json::json;

use common::LOCK;
use rlord_e2e_client::{BackgroundOutcome, Tuning};
use rlord_e2e_device::Phone;
use rlord_relay_proto::Role;
use world::*;

const LONG: Duration = Duration::from_secs(30);

/// A request that changes something reached the daemon this many times
/// (live or by mailbox).
fn asks(id: &str) -> usize {
    count(&format!("{id} op overlord.ask")) + count(&format!("{id} mailbox op overlord.ask"))
}

#[tokio::test(flavor = "multi_thread")]
async fn pairs_and_talks_through_the_engine() {
    let _g = LOCK.lock().await;
    let w = world(2).await;
    let (mut phone, _dir) = pair("phone").await;
    let id = device_id(&mut phone).await;
    let s = rpc("relay.devices", json!({})).await.unwrap();
    assert!(s.to_string().contains(&id), "{s}");
    keyed(&id, 1).await;
    let hello = phone
        .log
        .iter()
        .rev()
        .find(|v| v["event"] == "hello")
        .cloned()
        .unwrap();
    assert_eq!(hello["page"], id.as_str());
    let snap = phone.request("snapshot", json!({}), WAIT).await.unwrap();
    assert_eq!(
        snap["ok"]["rows"].as_array().map(Vec::len),
        Some(2),
        "{snap}"
    );
    let ask = phone
        .request("overlord.ask", json!({"text": "hello"}), WAIT)
        .await
        .unwrap();
    assert!(
        ask.get("ok").is_some() || ask["error"]["code"] != "not_allowed",
        "{ask}"
    );
    assert_eq!(asks(&id), 1);
    assert!(phone.outbox().is_empty(), "answered: gone");
    // A change on the host reaches the page.
    phone.mark();
    rpc(
        "session.rename",
        json!({"ref": "session-0000", "name": "moved"}),
    )
    .await
    .unwrap();
    phone
        .wait("changed", WAIT, |v| v["event"] == "changed")
        .await
        .unwrap();
    let st = phone.status().await.unwrap();
    assert_eq!(st["state"], "live");
    assert_eq!(st["keys"], "software");
    assert_eq!(st["relay"], w.tamper.origin.as_str());
    // In the background the link closes; the background check finds
    // nothing new without a handshake.
    phone.engine.lifecycle(false);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let hs = w.tamper.connections(Role::Device).len();
    let o = phone.background_check(10_000).await;
    assert!(
        matches!(
            o,
            BackgroundOutcome::NothingNew | BackgroundOutcome::Refreshed { .. }
        ),
        "{o:?}"
    );
    assert_eq!(w.tamper.connections(Role::Device).len(), hs + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_relay_restarted_queued_requests_are_resent_and_run_once() {
    let _g = LOCK.lock().await;
    let mut w = world(1).await;
    let (mut phone, _dir) = pair("restart").await;
    let id = device_id(&mut phone).await;
    keyed(&id, 1).await;
    host_away(&w, &mut phone).await;
    let ask = phone.send("overlord.ask", json!({"text": "while you were away"}));
    // Queued at the relay (by mailbox).
    let t = Instant::now();
    while phone.outbox().first().map(|i| i["state"].clone()) != Some(json!("sent")) {
        assert!(t.elapsed() < WAIT, "{:?}", phone.outbox());
        tokio::time::sleep(Duration::from_millis(50)).await;
        phone.drain();
    }
    // The relay restarts: the queue is gone, `boot` changes. The engine
    // comes back (after the relay's retry_after) and resends.
    w.restart_relay().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    start_plugin(&w, &[]);
    let a = phone.reply(ask, LONG).await.unwrap();
    assert_eq!(a["id"], ask);
    assert_ne!(a["error"]["code"], "interrupted", "{a}");
    assert_eq!(asks(&id), 1, "ran once");
}

/// The host discards the device's mailbox while the device isn't linked
/// (here its saved state loses the mailbox key while the plugin is down).
/// The host's next `hello` names no mailbox; the engine, holding one, keys
/// a new one, so a request it later sends by mailbox reaches the host and
/// is answered, once.
#[tokio::test(flavor = "multi_thread")]
async fn a_mailbox_the_host_discarded_is_keyed_again() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("rekey").await;
    let id = device_id(&mut phone).await;
    keyed(&id, 1).await;
    host_away(&w, &mut phone).await;
    // (The supervisor settles after a stop before the next start.)
    tokio::time::sleep(Duration::from_millis(500)).await;
    let path = rlord_core::plugin::remote_dir().join(format!("mailbox-{id}.json"));
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved["state"].is_string(), "{saved}");
    saved["state"] = serde_json::Value::Null;
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    phone.mark();
    start_plugin(&w, &[]);
    let hello = phone
        .wait("hello", LONG, |v| v["event"] == "hello")
        .await
        .unwrap();
    assert!(hello.get("mailbox").is_none(), "the page's hello: {hello}");
    keyed(&id, 2).await;
    // Away again: the ask goes by the new mailbox, is taken from the relay
    // when the host is back, and is answered once.
    host_away(&w, &mut phone).await;
    let ask = phone.send("overlord.ask", json!({"text": "after the rekey"}));
    let t = Instant::now();
    while phone.outbox().first().map(|i| i["state"].clone()) != Some(json!("sent")) {
        assert!(t.elapsed() < WAIT, "{:?}", phone.outbox());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    start_plugin(&w, &[]);
    let a = phone.reply(ask, LONG).await.unwrap();
    assert_eq!(a["id"], ask);
    assert_ne!(a["error"]["code"], "interrupted", "{a}");
    audit_until(&format!("{id} mailbox op overlord.ask"), 1).await;
    assert_eq!(asks(&id), 1, "ran once");
    phone.conn("live", LONG).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    phone.drain();
    assert_eq!(phone.log.iter().filter(|v| v["id"] == ask).count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plugin_restarted_mid_request() {
    let _g = LOCK.lock().await;
    // A slow overlord.ask (the plugin inherits the environment).
    // Safety: under LOCK.
    std::env::set_var("RLORD_WEB_TEST_SLOW", "overlord.ask:3000");
    let w = world(1).await;
    std::env::remove_var("RLORD_WEB_TEST_SLOW");
    let (mut phone, _dir) = pair("slow").await;
    let id = device_id(&mut phone).await;
    // In progress: the link is cut while it runs, the resend by id is
    // told `in_progress`, and the answer follows on the new session.
    let ask = phone.send("overlord.ask", json!({"text": "one"}));
    audit_until(&format!("{id} op overlord.ask"), 1).await;
    w.tamper.cut(Role::Device);
    let a = phone.reply(ask, LONG).await.unwrap();
    assert_ne!(a["error"]["code"], "in_progress", "{a}");
    assert_ne!(a["error"]["code"], "disconnected", "{a}");
    assert_eq!(asks(&id), 1);
    // Interrupted: the plugin stops while it runs. Its resend is told it
    // was interrupted (the host can't know whether it happened).
    let ask = phone.send("overlord.ask", json!({"text": "two"}));
    audit_until(&format!("{id} op overlord.ask"), 2).await;
    w.rig.daemon.web().stop();
    // (The supervisor settles after a stop before the next start.)
    tokio::time::sleep(Duration::from_millis(500)).await;
    start_plugin(&w, &[]);
    let a = phone.reply(ask, LONG).await.unwrap();
    assert_eq!(a["error"]["code"], "interrupted", "{a}");
    assert_eq!(asks(&id), 2, "never run again");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hosts_drop_and_a_stalled_link() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let tuning = Tuning {
        ping_ms: 1_000,
        silence_ms: 3_000,
    };
    let (mut phone, _dir) = pair_tuned("stalls", tuning).await;
    // A frame the host can't open: it drops the link (4205) and the
    // engine is back at once with a fresh session.
    let flipped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let f2 = flipped.clone();
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        use rlord_relay_proto::frame::FromDevice;
        use tokio_tungstenite::tungstenite::Message;
        if p.role == Role::Device && dir == rlord_e2e_device::testing::Dir::Up {
            if let Message::Binary(b) = &m {
                if let Ok(FromDevice::Live(mut p)) = FromDevice::decode(b) {
                    if !f2.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        let n = p.len() - 1;
                        p[n] ^= 1;
                        return vec![Message::Binary(FromDevice::Live(p).encode().into())];
                    }
                }
            }
        }
        vec![m]
    }));
    let n = w.tamper.connections(Role::Device).len();
    let t = Instant::now();
    let r = phone.request("snapshot", json!({}), WAIT).await.unwrap();
    assert_eq!(r["error"]["code"], "disconnected", "{r}");
    phone.conn("live", WAIT).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(w.tamper.connections(Role::Device).len(), n + 1);
    w.tamper.clear_filter();
    assert!(phone
        .request("snapshot", json!({}), WAIT)
        .await
        .unwrap()
        .get("ok")
        .is_some());
    // A stalled link (nothing passes, not even a pong) is given up after
    // the silence limit, and replaced.
    let n = w.tamper.connections(Role::Device).len();
    w.tamper.freeze(Role::Device);
    phone.mark();
    phone
        .wait("a new session", LONG, |v| v["event"] == "hello")
        .await
        .unwrap();
    assert!(w.tamper.connections(Role::Device).len() > n);
    assert!(phone
        .request("snapshot", json!({}), WAIT)
        .await
        .unwrap()
        .get("ok")
        .is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_queue_refuses_a_frame_then_takes_it() {
    let _g = LOCK.lock().await;
    // A queue of one message.
    let w = world_with(1, |c| c.limits.mailbox_messages = 1).await;
    let (mut phone, _dir) = pair("full").await;
    let id = device_id(&mut phone).await;
    keyed(&id, 1).await;
    host_away(&w, &mut phone).await;
    // The relay hides the host from the device: it keeps using the
    // mailbox even after the host is back.
    use rlord_e2e_device::testing::Dir;
    use rlord_relay_proto::FromRelay;
    use tokio_tungstenite::tungstenite::Message;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let s2 = seen.clone();
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if p.role != Role::Device || dir != Dir::Down {
            return vec![m];
        }
        let Message::Text(t) = &m else { return vec![m] };
        match FromRelay::parse(t) {
            Ok(FromRelay::Ready(mut r)) => {
                r.host = Some(rlord_relay_proto::msg::HostState::Offline);
                vec![Message::Text(FromRelay::Ready(r).to_json().into())]
            }
            Ok(FromRelay::Host { .. }) => vec![],
            Ok(FromRelay::Queued { .. }) => {
                s2.lock().unwrap().push("queued".into());
                vec![m]
            }
            Ok(FromRelay::Refused { code, .. }) => {
                s2.lock().unwrap().push(format!("refused {code}"));
                vec![m]
            }
            _ => vec![m],
        }
    }));
    let a1 = phone.send("overlord.ask", json!({"text": "one"}));
    let a2 = phone.send("overlord.ask", json!({"text": "two"}));
    let t = Instant::now();
    while seen.lock().unwrap().len() < 2 {
        assert!(t.elapsed() < WAIT, "{:?}", seen.lock().unwrap());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(*seen.lock().unwrap(), ["queued", "refused full"]);
    // The host comes back and takes the first; the device (which still
    // thinks it away) offers the refused one again on its next link,
    // first, and the relay takes it now.
    start_plugin(&w, &[]);
    let r1 = phone.reply(a1, LONG).await.unwrap();
    assert_eq!(r1["id"], a1);
    w.tamper.cut(Role::Device);
    let r2 = phone.reply(a2, LONG).await.unwrap();
    assert_eq!(r2["id"], a2);
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|s| *s == "queued")
            .count()
            >= 2
    );
    assert_eq!(asks(&id), 2, "each ran once");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_at_each_step_still_answers_once() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("flaky").await;
    let id = device_id(&mut phone).await;
    keyed(&id, 1).await;
    // A new connection's messages: auth (2), message 1 (3), hello (4),
    // then requests and acknowledgements. Cut at each.
    for step in 2..=7 {
        w.tamper.cut_next(Role::Device, step);
        w.tamper.cut(Role::Device);
        // (A path change: the engine dials at once rather than after the
        // backoff these short-lived links have built up.)
        phone.engine.network(true);
        let ask = phone.send("overlord.ask", json!({"text": format!("step {step}")}));
        let a = phone.reply(ask, LONG).await.unwrap();
        assert_eq!(a["id"], ask, "step {step}");
        assert_ne!(a["error"]["code"], "disconnected", "step {step}: {a}");
        assert_eq!(asks(&id), step - 1, "step {step}: ran once");
        phone.conn("live", LONG).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_network_away_then_back() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("commuter").await;
    let id = device_id(&mut phone).await;
    // Away, and the phone knows it: offline, no dialling.
    w.tamper.away(true);
    phone.engine.network(false);
    phone.conn("offline", WAIT).await.unwrap();
    let n = w.tamper.connections(Role::Device).len();
    let r = phone
        .request(
            "session.rename",
            json!({"ref": "session-0000", "name": "x"}),
            WAIT,
        )
        .await
        .unwrap();
    assert_eq!(r["error"]["code"], "disconnected");
    let ask = phone.send("overlord.ask", json!({"text": "on the train"}));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        w.tamper.connections(Role::Device).len(),
        n,
        "no dial offline"
    );
    // The phone thinks it is online but the path stays away: it backs
    // off rather than spinning.
    phone.engine.network(true);
    tokio::time::sleep(Duration::from_secs(4)).await;
    let tries = w.tamper.connections(Role::Device).len() - n;
    assert!((2..=8).contains(&tries), "{tries}");
    // Back: answered, once.
    w.tamper.away(false);
    phone.engine.network(true);
    let a = phone.reply(ask, LONG).await.unwrap();
    assert_eq!(a["id"], ask);
    assert_eq!(asks(&id), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_engines_with_one_identity_dont_fight() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut a, dir) = pair("twin").await;
    let _ = device_id(&mut a).await;
    // A copy of the phone's folder: the same keys and host.
    let copy = tempfile::tempdir().unwrap();
    copy_dir(dir.path(), copy.path());
    let mut b = Phone::open(copy.path()).unwrap();
    let n = w.tamper.connections(Role::Device).len();
    b.engine.lifecycle(true);
    b.conn("live", WAIT).await.unwrap();
    // A was replaced (4409): it waits rather than taking the identity
    // back at once.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let more = w.tamper.connections(Role::Device).len() - n;
    assert!(more <= 2, "{more} connections in 5 s");
    assert_eq!(b.conn_state(), "live");
    assert_ne!(a.conn_state(), "live");
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

#[tokio::test(flavor = "multi_thread")]
async fn unpairing_without_a_session_forgets_here_only() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, dir) = pair("forgetful").await;
    w.tamper.away(true);
    phone.engine.network(false);
    phone.conn("offline", WAIT).await.unwrap();
    phone.say(json!({"app": "unpair"}));
    phone.conn("unpaired", WAIT).await.unwrap();
    assert!(rlord_e2e_client::store::host_files(dir.path()).is_empty());
    let s = rpc("relay.devices", json!({})).await.unwrap();
    assert_eq!(
        s["devices"].as_array().map(Vec::len),
        Some(1),
        "still on the host: {s}"
    );
}
