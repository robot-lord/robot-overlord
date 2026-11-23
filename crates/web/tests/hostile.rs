//! Hostile relays end to end: a `Tamper` holds the pinned relay's own key
//! and sits in front of the real one, so it passes every pin check and
//! can then misbehave. Against the real plugin under an in-memory daemon
//! and the phone's own engine. No tmux session is started; no agent runs;
//! nothing leaves loopback.

mod common;
mod world;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use common::LOCK;
use rlord_e2e_device::testing::Dir;
use rlord_e2e_device::{Device, Host};
use rlord_relay_proto::frame::{FromDevice, ToDevice, ToHost};
use rlord_relay_proto::msg::{HostState, LinkKind};
use rlord_relay_proto::{FromRelay, Role, ToRelay, B64};
use world::*;

const LONG: Duration = Duration::from_secs(30);

fn asks(id: &str) -> usize {
    count(&format!("{id} op overlord.ask")) + count(&format!("{id} mailbox op overlord.ask"))
}

fn text(m: &FromRelay) -> Message {
    Message::Text(m.to_json().into())
}

fn relay_msg(m: &Message) -> Option<FromRelay> {
    match m {
        Message::Text(t) => FromRelay::parse(t).ok(),
        _ => None,
    }
}

fn device_fps(w: &World) -> Vec<[u8; 32]> {
    w.tamper
        .peers()
        .into_iter()
        .filter(|p| p.role == Role::Device)
        .map(|p| p.fp)
        .collect()
}

/// Waits until `n` device connections have been made in all.
async fn device_conns(w: &World, n: usize) {
    let t = Instant::now();
    while w.tamper.connections(Role::Device).len() < n {
        assert!(t.elapsed() < LONG, "never {n} device connections");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_device_let_through_fails_at_the_host() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("lost").await;
    let id = device_id(&mut phone).await;
    let fp = device_fps(&w)[0];
    // The phone is in a drawer when it is revoked: it never hears 4403.
    phone.engine.lifecycle(false);
    phone.conn("connecting", WAIT).await.unwrap();
    rpc("relay.revoke", json!({"device": id})).await.unwrap();
    status_until("revoked", |s| {
        s["devices"].as_array().is_some_and(Vec::is_empty)
    })
    .await;
    let links = count(&format!("{id} link"));
    let ops = count(" op ");
    // The relay lets it through anyway, inventing a link to the host.
    w.tamper.terminate(fp);
    phone.engine.lifecycle(true);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_ne!(phone.conn_state(), "live");
    assert_eq!(
        count(&format!("{id} link")),
        links,
        "no session at the host"
    );
    assert_eq!(count(" op "), ops);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_invented_for_a_stranger_gets_nothing() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("owner").await;
    // A device the host never paired, let in by the relay.
    let p = rpc("relay.pair", json!({})).await.unwrap();
    let qr = rlord_e2e::QrPayload::decode(p["qr"].as_str().unwrap()).unwrap();
    rpc("relay.pair_cancel", json!({})).await.unwrap();
    let mut dev = Device::new("stranger").unwrap();
    dev.host = Some(Host {
        static_key: qr.host_static,
        pin: w.relay.as_ref().unwrap().address().clone(),
        fp: [0; 32],
        name: String::new(),
    });
    w.tamper.terminate(dev.relay_key.fingerprint());
    let ops = count(" op ");
    let mut c = dev.connect().await.unwrap();
    assert_eq!(c.ready.boot, "tamper");
    assert!(dev.live(&mut c).await.is_err(), "no session for a stranger");
    assert_eq!(count(" op "), ops);
    let s = rpc("relay.status", json!({})).await.unwrap();
    assert!(s["pending"].is_null(), "{s}");
    assert_eq!(s["devices"].as_array().map(Vec::len), Some(1));
    // The real device is unaffected.
    let r = phone.request("snapshot", json!({}), WAIT).await.unwrap();
    assert!(r.get("ok").is_some(), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_devices_frames_swapped() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut a, _da) = pair("left").await;
    let (mut b, _db) = pair("right").await;
    let (ia, ib) = (device_id(&mut a).await, device_id(&mut b).await);
    a.conn("live", WAIT).await.unwrap();
    // The links' ids, as the relay told the host.
    let links: Arc<Mutex<Vec<u32>>> = Arc::default();
    let swap: Arc<Mutex<Option<(u32, u32)>>> = Arc::default();
    // The first frame of the pair is held until the other's comes, then
    // both go to the host swapped, back to back: passed on at once, the
    // host could drop the second link (for the first frame) before the
    // relay forwarded the second, leaving one phone waiting on a live
    // session for an answer that never comes (no answer watchdog yet).
    let held: Arc<Mutex<Vec<(u32, Vec<u8>)>>> = Arc::default();
    let (l2, s2, h2) = (links.clone(), swap.clone(), held.clone());
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if p.role != Role::Host || dir != Dir::Down {
            return vec![m];
        }
        if let Some(FromRelay::Link { id, .. }) = relay_msg(&m) {
            l2.lock().unwrap().push(id);
        }
        let Some((x, y)) = *s2.lock().unwrap() else {
            return vec![m];
        };
        let Message::Binary(bytes) = &m else {
            return vec![m];
        };
        let swapped = |link: u32, payload: Vec<u8>| {
            let other = if link == x { y } else { x };
            Message::Binary(
                ToHost::Live {
                    link: other,
                    payload,
                }
                .encode()
                .into(),
            )
        };
        match ToHost::decode(bytes) {
            Ok(ToHost::Live { link, payload }) if link == x || link == y => {
                let mut h = h2.lock().unwrap();
                if h.first().is_none_or(|(first, _)| *first == link) {
                    h.push((link, payload));
                    return vec![];
                }
                *s2.lock().unwrap() = None;
                let mut out: Vec<Message> = h.drain(..).map(|(l, p)| swapped(l, p)).collect();
                out.push(swapped(link, payload));
                out
            }
            _ => vec![m],
        }
    }));
    // Fresh links, so the filter knows both ids.
    a.mark();
    b.mark();
    w.tamper.cut(Role::Device);
    a.engine.network(true);
    b.engine.network(true);
    a.wait("a new session", WAIT, |v| v["event"] == "hello")
        .await
        .unwrap();
    b.wait("a new session", WAIT, |v| v["event"] == "hello")
        .await
        .unwrap();
    // (Settled: each session idle before both send.)
    tokio::time::sleep(Duration::from_secs(1)).await;
    let n = w.tamper.connections(Role::Device).len();
    {
        let ids = links.lock().unwrap();
        let k = ids.len();
        // (Only these two: the links the phones come back on are not.)
        *swap.lock().unwrap() = Some((ids[k - 2], ids[k - 1]));
    }
    let qa = a.send("overlord.ask", json!({"text": "from a"}));
    let qb = b.send("overlord.ask", json!({"text": "from b"}));
    // Neither frame opens under the other's session: both links are
    // dropped, both phones come back on new links and resend.
    device_conns(&w, n + 2).await;
    let ra = a.reply(qa, LONG).await.unwrap();
    let rb = b.reply(qb, LONG).await.unwrap();
    assert_eq!((ra["id"].clone(), rb["id"].clone()), (json!(qa), json!(qb)));
    assert_eq!((asks(&ia), asks(&ib)), (1, 1), "each ran once, as its own");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mailbox_frame_replayed() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("replayed").await;
    let id = device_id(&mut phone).await;
    keyed(&id, 1).await;
    host_away(&w, &mut phone).await;
    // Replayed: the relay queues the frame twice.
    let once = Arc::new(AtomicBool::new(false));
    let o2 = once.clone();
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if p.role == Role::Device && dir == Dir::Up {
            if let Message::Binary(b) = &m {
                if matches!(FromDevice::decode(b), Ok(FromDevice::Mailbox(_)))
                    && !o2.swap(true, Ordering::SeqCst)
                {
                    return vec![m.clone(), m];
                }
            }
        }
        vec![m]
    }));
    let q = phone.send("overlord.ask", json!({"text": "twice?"}));
    let t = Instant::now();
    while !once.load(Ordering::SeqCst) {
        assert!(t.elapsed() < WAIT);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    w.tamper.clear_filter();
    start_plugin(&w, &[]);
    let r = phone.reply(q, LONG).await.unwrap();
    assert_eq!(r["id"], q);
    assert_eq!(asks(&id), 1, "the replay never ran it again");
    audit_until(&format!("{id} mailbox discarded"), 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mailbox_frame_skipped() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("skipped").await;
    let id = device_id(&mut phone).await;
    keyed(&id, 1).await;
    host_away(&w, &mut phone).await;
    // The relay swallows one frame and says it queued it.
    let dropped = Arc::new(AtomicBool::new(false));
    let d2 = dropped.clone();
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if p.role == Role::Device && dir == Dir::Up {
            if let Message::Binary(b) = &m {
                if matches!(FromDevice::decode(b), Ok(FromDevice::Mailbox(_)))
                    && !d2.swap(true, Ordering::SeqCst)
                {
                    return vec![];
                }
            }
        }
        vec![m]
    }));
    let q1 = phone.send("overlord.ask", json!({"text": "lost"}));
    let t = Instant::now();
    while !dropped.load(Ordering::SeqCst) {
        assert!(t.elapsed() < WAIT);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    w.tamper.inject(
        Role::Device,
        text(&FromRelay::Queued {
            seq: 999,
            device: None,
        }),
    );
    let q2 = phone.send("overlord.ask", json!({"text": "after the gap"}));
    tokio::time::sleep(Duration::from_millis(500)).await;
    w.tamper.clear_filter();
    start_plugin(&w, &[]);
    let r2 = phone.reply(q2, LONG).await.unwrap();
    let r1 = phone.reply(q1, LONG).await.unwrap();
    assert_eq!((r1["id"].clone(), r2["id"].clone()), (json!(q1), json!(q2)));
    assert_eq!(asks(&id), 2, "each ran once");
    // The frame after the gap opened at the host: the mailbox lives on.
    assert_eq!(count(&format!("{id} mailbox discarded")), 0);
}

type Recorded = (Option<(B64, [u8; 32])>, Vec<Vec<u8>>);

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_pairing_replayed_after_the_rendezvous_closed() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    // The pairing's hello (rendezvous, device) and its lane-1 frames.
    let rec: Arc<Mutex<Recorded>> = Arc::default();
    let r2 = rec.clone();
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if p.role == Role::Pair && dir == Dir::Up {
            match &m {
                Message::Text(t) => {
                    if let Ok(ToRelay::Hello(h)) = ToRelay::parse(t) {
                        r2.lock().unwrap().0 = h.rendezvous.map(|r| (r, p.fp));
                    }
                }
                Message::Binary(b) => {
                    if let Ok(FromDevice::Live(x)) = FromDevice::decode(b) {
                        r2.lock().unwrap().1.push(x);
                    }
                }
                _ => {}
            }
        }
        vec![m]
    }));
    let (_phone, _dir) = pair("original").await;
    w.tamper.clear_filter();
    let (hello, frames) = rec.lock().unwrap().clone();
    let (rendezvous, fp) = hello.expect("the pairing's hello");
    assert!(frames.len() >= 2, "message 1 and the first frame");
    // After the rendezvous closed, the relay plays it all again.
    let link = 0x6000_0001;
    w.tamper.inject(
        Role::Host,
        text(&FromRelay::Link {
            id: link,
            device: B64(fp.to_vec()),
            kind: LinkKind::Pair,
            rendezvous: Some(rendezvous),
        }),
    );
    for f in frames {
        w.tamper.inject(
            Role::Host,
            Message::Binary(ToHost::Live { link, payload: f }.encode().into()),
        );
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let s = rpc("relay.status", json!({})).await.unwrap();
    assert!(s["pending"].is_null(), "{s}");
    assert_eq!(s["devices"].as_array().map(Vec::len), Some(1), "{s}");
}

#[tokio::test(flavor = "multi_thread")]
async fn bye_retry_after_on_the_engine_and_the_plugin() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("patient").await;
    let bye = |r| {
        text(&FromRelay::Bye {
            code: "overloaded".into(),
            retry_after: Some(r),
        })
    };
    let close = Message::Close(Some(CloseFrame {
        code: CloseCode::from(4503),
        reason: "".into(),
    }));
    // The engine.
    let n = w.tamper.connections(Role::Device).len();
    let t = Instant::now();
    w.tamper.inject(Role::Device, bye(3));
    w.tamper.inject(Role::Device, close.clone());
    device_conns(&w, n + 1).await;
    let waited = w.tamper.connections(Role::Device)[n] - t;
    assert!(waited >= Duration::from_secs(3), "{waited:?}");
    assert!(waited < Duration::from_secs(6), "{waited:?}");
    phone.conn("live", WAIT).await.unwrap();
    // The plugin.
    let n = w.tamper.connections(Role::Host).len();
    let t = Instant::now();
    w.tamper.inject(Role::Host, bye(3));
    w.tamper.inject(Role::Host, close);
    let t2 = Instant::now();
    while w.tamper.connections(Role::Host).len() <= n {
        assert!(t2.elapsed() < LONG, "the host never came back");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let waited = w.tamper.connections(Role::Host)[n] - t;
    assert!(waited >= Duration::from_secs(3), "{waited:?}");
    phone.conn("live", LONG).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn enroll_on_the_engine_and_the_plugin() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("enrolled").await;
    // Before `ready`, the relay asks both to enrol (shown, waited on).
    let pending: Arc<Mutex<Vec<Role>>> = Arc::new(Mutex::new(vec![Role::Device, Role::Host]));
    let p2 = pending.clone();
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if dir == Dir::Down && matches!(relay_msg(&m), Some(FromRelay::Ready(_))) {
            let mut g = p2.lock().unwrap();
            if let Some(i) = g.iter().position(|r| *r == p.role) {
                g.remove(i);
                let enroll = FromRelay::Enroll {
                    url: "https://relay.example/enrol".into(),
                    code: "WXYZ".into(),
                    expires: u64::MAX / 2,
                };
                return vec![text(&enroll), m];
            }
        }
        vec![m]
    }));
    phone.mark();
    w.tamper.cut(Role::Device);
    phone.engine.network(true);
    let ev = phone
        .wait("the enrolment shown", WAIT, |v| {
            v["event"] == "conn" && v["detail"].as_str().is_some_and(|d| d.contains("WXYZ"))
        })
        .await
        .unwrap();
    assert_eq!(ev["state"], "connecting");
    phone.conn("live", WAIT).await.unwrap();
    // The plugin: it keeps waiting through `enroll` and links.
    w.tamper.cut(Role::Host);
    let t = Instant::now();
    while !pending.lock().unwrap().is_empty() {
        assert!(t.elapsed() < LONG, "the host never came back");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    status_until("the host back", |s| s["connected"] == true).await;
    phone.conn("live", LONG).await.unwrap();
    let r = phone.request("snapshot", json!({}), WAIT).await.unwrap();
    assert!(r.get("ok").is_some(), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_lying_that_the_host_is_online_gets_no_spin() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("lied-to").await;
    host_away(&w, &mut phone).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let a2 = attempts.clone();
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if p.role != Role::Device {
            return vec![m];
        }
        match (dir, &m) {
            (Dir::Down, _) => match relay_msg(&m) {
                Some(FromRelay::Ready(mut r)) => {
                    r.host = Some(HostState::Online);
                    vec![text(&FromRelay::Ready(r))]
                }
                Some(FromRelay::Host { .. }) => vec![],
                _ => vec![m],
            },
            (Dir::Up, Message::Binary(b)) => {
                if let Ok(FromDevice::Live(x)) = FromDevice::decode(b) {
                    // Message 1 of a reconnect.
                    if x.len() > 2 && x[0] == 1 && x[1] == 2 {
                        a2.fetch_add(1, Ordering::SeqCst);
                    }
                }
                vec![m]
            }
            _ => vec![m],
        }
    }));
    let n = w.tamper.connections(Role::Device).len();
    w.tamper.cut(Role::Device);
    phone.engine.network(true);
    tokio::time::sleep(Duration::from_secs(8)).await;
    // `undeliverable` each time: retried on the same link at 1, 2, 4 s.
    let tried = attempts.load(Ordering::SeqCst);
    assert!((2..=5).contains(&tried), "{tried} handshakes in 8 s");
    assert_eq!(w.tamper.connections(Role::Device).len(), n + 1, "no redial");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_boot_every_connect_and_constant_signals_stay_within_bounds() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("bounded").await;
    let id = device_id(&mut phone).await;
    keyed(&id, 1).await;
    host_away(&w, &mut phone).await;
    let boots = Arc::new(AtomicUsize::new(0));
    let lane2 = Arc::new(AtomicUsize::new(0));
    let texts = Arc::new(AtomicUsize::new(0));
    let (b2, l2, t2) = (boots.clone(), lane2.clone(), texts.clone());
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if p.role != Role::Device {
            return vec![m];
        }
        match (dir, &m) {
            (Dir::Down, _) => match relay_msg(&m) {
                Some(FromRelay::Ready(mut r)) => {
                    r.boot = format!("boot-{}", b2.fetch_add(1, Ordering::SeqCst));
                    vec![text(&FromRelay::Ready(r))]
                }
                _ => vec![m],
            },
            (Dir::Up, Message::Binary(b)) => {
                if matches!(FromDevice::decode(b), Ok(FromDevice::Mailbox(_))) {
                    l2.fetch_add(1, Ordering::SeqCst);
                }
                vec![m]
            }
            (Dir::Up, Message::Text(_)) => {
                t2.fetch_add(1, Ordering::SeqCst);
                vec![m]
            }
            _ => vec![m],
        }
    }));
    let q = phone.send("overlord.ask", json!({"text": "once per link"}));
    let n = w.tamper.connections(Role::Device).len();
    for _ in 0..6 {
        for _ in 0..50 {
            w.tamper.inject(Role::Device, text(&FromRelay::Signal));
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
        w.tamper.cut(Role::Device);
    }
    let conns = w.tamper.connections(Role::Device).len() - n;
    let frames = lane2.load(Ordering::SeqCst);
    assert!(conns <= 7, "{conns} connections: the backoff holds");
    assert!(
        frames <= conns + 1,
        "{frames} mailbox frames on {conns} links"
    );
    // Signals make it send nothing (beyond its acknowledgements).
    assert!(
        texts.load(Ordering::SeqCst) <= 2 * conns + 2,
        "{}",
        texts.load(Ordering::SeqCst)
    );
    w.tamper.clear_filter();
    start_plugin(&w, &[]);
    phone.engine.network(true);
    let r = phone.reply(q, LONG).await.unwrap();
    assert_eq!(r["id"], q);
    assert_eq!(asks(&id), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_that_never_answers_queued_is_given_up() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("unanswered").await;
    let id = device_id(&mut phone).await;
    keyed(&id, 1).await;
    host_away(&w, &mut phone).await;
    let silent = Arc::new(AtomicBool::new(true));
    let s2 = silent.clone();
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if p.role == Role::Device
            && dir == Dir::Down
            && s2.load(Ordering::SeqCst)
            && matches!(
                relay_msg(&m),
                Some(FromRelay::Queued { .. } | FromRelay::Refused { .. })
            )
        {
            return vec![];
        }
        vec![m]
    }));
    let n = w.tamper.connections(Role::Device).len();
    let t = Instant::now();
    phone.send("overlord.ask", json!({"text": "hello?"}));
    device_conns(&w, n + 1).await;
    silent.store(false, Ordering::SeqCst);
    let waited = w.tamper.connections(Role::Device)[n] - t;
    assert!(waited >= Duration::from_secs(9), "{waited:?}");
    // On the new link it goes again and is queued.
    let t = Instant::now();
    while phone.outbox().first().map(|i| i["state"].clone()) != Some(json!("sent")) {
        assert!(t.elapsed() < WAIT, "{:?}", phone.outbox());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_message_2_and_garbage_frames() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("garbled").await;
    // Record the next session's message 2.
    let rec: Arc<Mutex<Option<Vec<u8>>>> = Arc::default();
    let replay = Arc::new(AtomicBool::new(false));
    let (r2, p2) = (rec.clone(), replay.clone());
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if p.role != Role::Device || dir != Dir::Down {
            return vec![m];
        }
        let Message::Binary(b) = &m else {
            return vec![m];
        };
        let Ok(ToDevice::Live(x)) = ToDevice::decode(b) else {
            return vec![m];
        };
        if x.len() > 2 && x[0] == 1 && x[1] == 2 {
            let mut g = r2.lock().unwrap();
            if g.is_none() {
                *g = Some(x);
            } else if p2.swap(false, Ordering::SeqCst) {
                let old = g.clone().unwrap();
                return vec![Message::Binary(ToDevice::Live(old).encode().into())];
            }
        }
        vec![m]
    }));
    phone.mark();
    w.tamper.cut(Role::Device);
    phone.engine.network(true);
    phone
        .wait("a new session", WAIT, |v| v["event"] == "hello")
        .await
        .unwrap();
    assert!(rec.lock().unwrap().is_some());
    // The next session gets the old message 2: it is refused, and the
    // engine starts over on a new link.
    let n = w.tamper.connections(Role::Device).len();
    replay.store(true, Ordering::SeqCst);
    phone.mark();
    w.tamper.cut(Role::Device);
    phone.engine.network(true);
    device_conns(&w, n + 2).await;
    phone
        .wait("a new session", WAIT, |v| v["event"] == "hello")
        .await
        .unwrap();
    w.tamper.clear_filter();
    // Garbage: a frame that doesn't parse, and one that doesn't decrypt.
    for junk in [vec![9u8, 9, 9], ToDevice::Live(vec![7; 64]).encode()] {
        let n = w.tamper.connections(Role::Device).len();
        w.tamper.inject(Role::Device, Message::Binary(junk.into()));
        device_conns(&w, n + 1).await;
        phone.mark();
        phone
            .wait("live again", WAIT, |v| v["event"] == "hello")
            .await
            .unwrap();
    }
    let r = phone.request("snapshot", json!({}), WAIT).await.unwrap();
    assert!(r.get("ok").is_some(), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn old_mailbox_frames_replayed_to_the_device() {
    let _g = LOCK.lock().await;
    let w = world(1).await;
    let (mut phone, _dir) = pair("haunted").await;
    let id = device_id(&mut phone).await;
    keyed(&id, 1).await;
    host_away(&w, &mut phone).await;
    let rec: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let r2 = rec.clone();
    w.tamper.set_filter(Box::new(move |p, dir, m| {
        if p.role == Role::Device && dir == Dir::Down {
            if let Message::Binary(b) = &m {
                if let Ok(ToDevice::Mailbox { payload, .. }) = ToDevice::decode(b) {
                    r2.lock().unwrap().push(payload);
                }
            }
        }
        vec![m]
    }));
    let q = phone.send("overlord.ask", json!({"text": "answer by mailbox"}));
    let t = Instant::now();
    while phone.outbox().first().map(|i| i["state"].clone()) != Some(json!("sent")) {
        assert!(t.elapsed() < WAIT, "{:?}", phone.outbox());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    start_plugin(&w, &[]);
    let r = phone.reply(q, LONG).await.unwrap();
    assert_eq!(r["id"], q);
    let t = Instant::now();
    while rec.lock().unwrap().is_empty() {
        assert!(t.elapsed() < LONG, "the host never answered by mailbox");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    phone.conn("live", WAIT).await.unwrap();
    // The relay replays the host's old frame under a new number.
    let old = rec.lock().unwrap()[0].clone();
    w.tamper.inject(
        Role::Device,
        Message::Binary(
            ToDevice::Mailbox {
                seq: 9_999,
                payload: old,
            }
            .encode()
            .into(),
        ),
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    phone.drain();
    assert_eq!(
        phone.log.iter().filter(|v| v["id"] == q).count(),
        1,
        "no answer accepted twice"
    );
    // The mailbox was discarded: the next session keys a new one.
    w.tamper.cut(Role::Device);
    phone.engine.network(true);
    keyed(&id, 2).await;
}
