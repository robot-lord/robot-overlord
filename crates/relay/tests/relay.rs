//! The relay link against the real relay on 127.0.0.1: the connect
//! sequence and its close codes, registration, allow-lists, links and
//! splicing, rendezvous, mailbox queues, signals and limits
//! (docs/working/e2e-protocol.md, sections 1 and 2).

mod common;

use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use rlord_relay::Registration;
use rlord_relay_proto::frame::{FromDevice, FromHost, ToDevice, ToHost};
use rlord_relay_proto::msg::{close, code, FromRelay, HostState, LinkKind, ToRelay};
use rlord_relay_proto::{auth, B64};
use tokio_tungstenite::tungstenite::Message;

fn b64(b: &[u8]) -> B64 {
    B64(b.to_vec())
}

/// A host with a device on its list; the device connected. The host has
/// read the device's `link`; its id.
async fn host_and_device(r: &Relay) -> (Client, Client, u32) {
    let mut h = host(r).await;
    let dk = id();
    h.allow(&[dk.fingerprint()]).await;
    let d = connect(r, &dk, As::Device(h.fp())).await.unwrap();
    assert_eq!(d.ready.host, Some(HostState::Online));
    let link = expect_link(&mut h, &dk.fingerprint(), LinkKind::Device).await;
    (h, d, link)
}

async fn expect_link(h: &mut Client, device: &Fp, kind: LinkKind) -> u32 {
    match h.ctl().await {
        FromRelay::Link {
            id,
            device: d,
            kind: k,
            ..
        } => {
            assert_eq!(d.0, device.to_vec());
            assert_eq!(k, kind);
            id
        }
        other => panic!("expected link, got {other:?}"),
    }
}

#[tokio::test]
async fn both_sides_prove_their_keys() {
    let r = relay(|_| {}).await;
    // The client checks the relay's challenge against the pin (in
    // `handshake`); here the relay checks the peer's.
    let h = host(&r).await;
    assert_eq!(h.ready.boot, r.running.boot());
    assert!(h.ready.host.is_none() && h.ready.signal.is_none());
    assert_eq!(h.ready.limits.as_ref().unwrap().devices, Some(16));

    // A peer that signs the wrong transcript (another role) gets 4401.
    let key = id();
    let mut ws = open(&r.url()).await;
    let n = nonce();
    let who = As::Device(h.fp());
    ws.send(Message::text(ToRelay::Hello(who.hello(&key, n)).to_json()))
        .await
        .unwrap();
    let Some(Ok(Message::Text(t))) = ws.next().await else {
        panic!()
    };
    let FromRelay::Challenge {
        origin,
        key: rk,
        nonce: rn,
        ..
    } = FromRelay::parse(&t).unwrap()
    else {
        panic!()
    };
    let wrong = auth::peer_transcript(
        rlord_relay_proto::Role::Host,
        &origin,
        &rk.0,
        &key.public(),
        &rn.0,
        &n,
        &h.fp(),
    );
    ws.send(Message::text(
        ToRelay::Auth {
            sig: B64(key.sign(&wrong).to_vec()),
        }
        .to_json(),
    ))
    .await
    .unwrap();
    assert_eq!(close_code(&mut ws).await, Some(close::BAD_SIGNATURE));

    // Someone else's key, signed by another: 4401.
    let mut ws = open(&r.url()).await;
    let victim = id();
    let n = nonce();
    ws.send(Message::text(
        ToRelay::Hello(As::Host(None).hello(&victim, n)).to_json(),
    ))
    .await
    .unwrap();
    let Some(Ok(Message::Text(t))) = ws.next().await else {
        panic!()
    };
    let FromRelay::Challenge {
        origin,
        key: rk,
        nonce: rn,
        ..
    } = FromRelay::parse(&t).unwrap()
    else {
        panic!()
    };
    let t = auth::peer_transcript(
        rlord_relay_proto::Role::Host,
        &origin,
        &rk.0,
        &victim.public(),
        &rn.0,
        &n,
        &[],
    );
    ws.send(Message::text(
        ToRelay::Auth {
            sig: B64(id().sign(&t).to_vec()),
        }
        .to_json(),
    ))
    .await
    .unwrap();
    assert_eq!(close_code(&mut ws).await, Some(close::BAD_SIGNATURE));
}

async fn close_code(ws: &mut Ws) -> Option<u16> {
    loop {
        match tokio::time::timeout(WAIT, ws.next()).await.unwrap() {
            Some(Ok(Message::Close(c))) => return c.map(|c| u16::from(c.code)),
            Some(Ok(_)) => {}
            _ => return None,
        }
    }
}

#[tokio::test]
async fn bad_hellos_and_versions_are_refused() {
    let r = relay(|_| {}).await;
    let key = id();
    let hello = |v: u32| {
        let mut h = As::Host(None).hello(&key, nonce());
        h.v = v;
        ToRelay::Hello(h).to_json()
    };
    for (text, want) in [
        (hello(2), close::UNSUPPORTED),
        (
            r#"{"t":"hello","v":7,"future":"field"}"#.to_string(),
            close::UNSUPPORTED,
        ),
        (r#"{"t":"auth","sig":""}"#.to_string(), close::BAD_SIGNATURE),
        (
            r#"{"t":"hello","role":"host"}"#.to_string(),
            close::BAD_SIGNATURE,
        ),
        ("not json".to_string(), close::BAD_SIGNATURE),
        // A device without its host, a pairing device without a rendezvous.
        (
            hello(1).replace(r#""role":"host""#, r#""role":"device""#),
            close::BAD_SIGNATURE,
        ),
        (
            hello(1).replace(r#""role":"host""#, r#""role":"pair""#),
            close::BAD_SIGNATURE,
        ),
        // A key that is not a P-256 point.
        (
            ToRelay::Hello(rlord_relay_proto::msg::Hello {
                key: B64(vec![4; 65]),
                ..As::Host(None).hello(&key, nonce())
            })
            .to_json(),
            close::BAD_SIGNATURE,
        ),
    ] {
        let mut ws = open(&r.url()).await;
        ws.send(Message::text(text.clone())).await.unwrap();
        assert_eq!(close_code(&mut ws).await, Some(want), "{text}");
    }
    // Binary before authenticating.
    let mut ws = open(&r.url()).await;
    ws.send(Message::binary(vec![1, 2, 3])).await.unwrap();
    assert_eq!(close_code(&mut ws).await, Some(close::BAD_SIGNATURE));
}

#[tokio::test]
async fn too_slow_to_authenticate() {
    let r = relay(|c| c.timeouts.auth = Duration::from_millis(300)).await;
    let mut ws = open(&r.url()).await;
    assert_eq!(close_code(&mut ws).await, Some(close::TOO_SLOW));
}

#[tokio::test]
async fn token_registration() {
    let r = relay(|_| {}).await;
    // No token, a made-up token: 4403.
    for who in [As::Host(None), As::Host(Some("rlt_nope".into()))] {
        let e = connect(&r, &id(), who).await.err().unwrap();
        assert_eq!(e.code, Some(close::NOT_ALLOWED));
    }
    // A token registers one host; used, it registers no other.
    let t = r.token();
    let a = id();
    connect(&r, &a, As::Host(Some(t.clone())))
        .await
        .unwrap()
        .close()
        .await;
    let e = connect(&r, &id(), As::Host(Some(t.clone())))
        .await
        .err()
        .unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));
    // The registered host comes back without one (and its token is ignored).
    connect(&r, &a, As::Host(None)).await.unwrap().close().await;
    connect(&r, &a, As::Host(Some(t)))
        .await
        .unwrap()
        .close()
        .await;

    // A token for two uses, under an account.
    let admin = r.running.admin();
    let t2 = admin
        .token_new(2, Duration::from_secs(60), "team-a")
        .unwrap();
    for _ in 0..2 {
        connect(&r, &id(), As::Host(Some(t2.clone())))
            .await
            .unwrap();
    }
    assert!(connect(&r, &id(), As::Host(Some(t2))).await.is_err());
    let hosts = admin.hosts().unwrap();
    assert_eq!(hosts.iter().filter(|h| h.account == "team-a").count(), 2);

    // An expired token.
    let t3 = admin.token_new(1, Duration::ZERO, "local").unwrap();
    let e = connect(&r, &id(), As::Host(Some(t3))).await.err().unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));
}

#[tokio::test]
async fn closed_and_open_registration() {
    let r = relay(|c| c.registration = Registration::Closed).await;
    let a = id();
    // Even with a token.
    let t = r.token();
    let e = connect(&r, &a, As::Host(Some(t))).await.err().unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));
    assert!(r
        .running
        .admin()
        .host_add(&a.fingerprint(), "local")
        .unwrap());
    connect(&r, &a, As::Host(None)).await.unwrap();

    let r = relay(|c| {
        c.registration = Registration::Open;
        c.max_hosts = 1;
    })
    .await;
    connect(&r, &a, As::Host(None)).await.unwrap().close().await;
    // Over max_hosts.
    let e = connect(&r, &id(), As::Host(None)).await.err().unwrap();
    assert_eq!(e.code, Some(close::LIMIT));
    // The registered one still comes back.
    connect(&r, &a, As::Host(None)).await.unwrap();
}

#[tokio::test]
async fn devices_need_the_hosts_list() {
    let r = relay(|c| c.limits.devices_per_host = 2).await;
    let mut h = host(&r).await;
    let (a, b, c) = (id(), id(), id());
    // Unknown host, not on the list: 4403.
    let e = connect(&r, &a, As::Device(id().fingerprint()))
        .await
        .err()
        .unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));
    let e = connect(&r, &a, As::Device(h.fp())).await.err().unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));
    // Over devices_per_host.
    let allow = vec![
        b64(&a.fingerprint()),
        b64(&b.fingerprint()),
        b64(&c.fingerprint()),
    ];
    h.refused(code::LIMIT, |n| ToRelay::Devices { n, allow })
        .await;
    h.refused(code::BAD, |n| ToRelay::Devices {
        n,
        allow: vec![B64(vec![1; 5])],
    })
    .await;

    h.allow(&[a.fingerprint(), b.fingerprint()]).await;
    let mut da = connect(&r, &a, As::Device(h.fp())).await.unwrap();
    expect_link(&mut h, &a.fingerprint(), LinkKind::Device).await;
    // The list is replaced whole: `a` is off it, closed, and refused.
    h.allow(&[b.fingerprint()]).await;
    assert_eq!(da.closed().await, Some(close::NOT_ALLOWED));
    assert!(matches!(h.ctl().await, FromRelay::Unlink { .. }));
    let e = connect(&r, &a, As::Device(h.fp())).await.err().unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));
    connect(&r, &b, As::Device(h.fp())).await.unwrap();
    // The list is durable: it holds after a restart without a new `devices`.
    let r = r.restart().await;
    let e = connect(&r, &a, As::Device(h.fp())).await.err().unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));
    let d = connect(&r, &b, As::Device(h.fp())).await.unwrap();
    assert_eq!(d.ready.host, Some(HostState::Offline));
}

#[tokio::test]
async fn live_frames_are_spliced_both_ways() {
    let r = relay(|_| {}).await;
    let (mut h, mut d, link) = host_and_device(&r).await;
    d.send_bin(FromDevice::Live(b"hello host".to_vec()).encode())
        .await;
    assert_eq!(
        ToHost::decode(&h.bin().await).unwrap(),
        ToHost::Live {
            link,
            payload: b"hello host".to_vec()
        }
    );
    h.send_bin(
        FromHost::Live {
            link,
            payload: b"hello device".to_vec(),
        }
        .encode(),
    )
    .await;
    assert_eq!(
        ToDevice::decode(&d.bin().await).unwrap(),
        ToDevice::Live(b"hello device".to_vec())
    );
    // Many frames keep their order.
    for i in 0..200u32 {
        d.send_bin(FromDevice::Live(i.to_be_bytes().to_vec()).encode())
            .await;
    }
    for i in 0..200u32 {
        let ToHost::Live { payload, .. } = ToHost::decode(&h.bin().await).unwrap() else {
            panic!()
        };
        assert_eq!(payload, i.to_be_bytes());
    }
    // A link that doesn't exist.
    h.send_bin(
        FromHost::Live {
            link: link + 100,
            payload: vec![1],
        }
        .encode(),
    )
    .await;
    assert_eq!(
        h.ctl().await,
        FromRelay::Undeliverable {
            link: Some(link + 100)
        }
    );
    // The device goes: unlink, and its link is undeliverable.
    d.close().await;
    assert_eq!(h.ctl().await, FromRelay::Unlink { id: link });
    h.send_bin(
        FromHost::Live {
            link,
            payload: vec![1],
        }
        .encode(),
    )
    .await;
    assert_eq!(h.ctl().await, FromRelay::Undeliverable { link: Some(link) });
}

#[tokio::test]
async fn devices_hear_their_host_come_and_go() {
    let r = relay(|_| {}).await;
    let hk = id();
    let mut h = connect(&r, &hk, As::Host(Some(r.token()))).await.unwrap();
    let dk = id();
    h.allow(&[dk.fingerprint()]).await;
    h.close().await;
    let mut d = connect(&r, &dk, As::Device(hk.fingerprint()))
        .await
        .unwrap();
    assert_eq!(d.ready.host, Some(HostState::Offline));
    // Live with no host: undeliverable.
    d.send_bin(FromDevice::Live(vec![1]).encode()).await;
    assert_eq!(d.ctl().await, FromRelay::Undeliverable { link: None });
    let mut h = connect(&r, &hk, As::Host(None)).await.unwrap();
    assert_eq!(
        d.ctl().await,
        FromRelay::Host {
            state: HostState::Online
        }
    );
    let link = expect_link(&mut h, &dk.fingerprint(), LinkKind::Device).await;
    d.send_bin(FromDevice::Live(vec![2]).encode()).await;
    assert_eq!(
        ToHost::decode(&h.bin().await).unwrap(),
        ToHost::Live {
            link,
            payload: vec![2]
        }
    );
    h.close().await;
    assert_eq!(
        d.ctl().await,
        FromRelay::Host {
            state: HostState::Offline
        }
    );
}

#[tokio::test]
async fn a_newer_connection_replaces_the_older() {
    let r = relay(|_| {}).await;
    let (mut h, mut d1, link1) = host_and_device(&r).await;
    let d2 = connect(&r, &d1.key.clone(), As::Device(h.fp()))
        .await
        .unwrap();
    assert_eq!(d1.closed().await, Some(close::REPLACED));
    // The host sees the old link go and the new one come.
    let mut got = [h.ctl().await, h.ctl().await];
    got.sort_by_key(|m| matches!(m, FromRelay::Link { .. }));
    assert_eq!(got[0], FromRelay::Unlink { id: link1 });
    assert!(matches!(got[1], FromRelay::Link { id, .. } if id != link1));
    drop(d2);

    // A host too; its devices hear it go and come back.
    let hk = h.key.clone();
    let h2 = connect(&r, &hk, As::Host(None)).await.unwrap();
    assert_eq!(h.closed().await, Some(close::REPLACED));
    drop(h2);
}

#[tokio::test]
async fn rendezvous_admit_pairing_devices() {
    let r = relay(|_| {}).await;
    let mut h = host(&r).await;
    let rid = [7u8; 16];
    // No such rendezvous.
    let e = connect(&r, &id(), As::Pair(rid)).await.err().unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));
    h.ok(|n| ToRelay::Rendezvous {
        n,
        id: b64(&rid),
        ttl: 60,
    })
    .await;
    let pk = id();
    let mut p = connect(&r, &pk, As::Pair(rid)).await.unwrap();
    assert_eq!(p.ready.boot, r.running.boot());
    let link = match h.ctl().await {
        FromRelay::Link {
            id,
            device,
            kind: LinkKind::Pair,
            rendezvous: Some(rv),
        } => {
            assert_eq!(device.0, pk.fingerprint());
            assert_eq!(rv.0, rid);
            id
        }
        other => panic!("{other:?}"),
    };
    // Splices like a device link.
    p.send_bin(FromDevice::Live(b"msg1".to_vec()).encode())
        .await;
    assert_eq!(
        ToHost::decode(&h.bin().await).unwrap(),
        ToHost::Live {
            link,
            payload: b"msg1".to_vec()
        }
    );
    h.send_bin(
        FromHost::Live {
            link,
            payload: b"msg2".to_vec(),
        }
        .encode(),
    )
    .await;
    assert_eq!(
        ToDevice::decode(&p.bin().await).unwrap(),
        ToDevice::Live(b"msg2".to_vec())
    );
    // No mailbox on a pairing link.
    p.send_bin(FromDevice::Mailbox(vec![1]).encode()).await;
    assert!(matches!(p.ctl().await, FromRelay::Err { n: 0, code } if code == code::BAD));
    // One pairing link at a time.
    let e = connect(&r, &id(), As::Pair(rid)).await.err().unwrap();
    assert_eq!(e.code, Some(close::LIMIT));
    // The host drops it.
    h.ok(|n| ToRelay::Drop { n, link }).await;
    assert_eq!(p.closed().await, Some(1000));
    assert_eq!(h.ctl().await, FromRelay::Unlink { id: link });
    h.refused(code::UNKNOWN, |n| ToRelay::Drop { n, link })
        .await;
    // Closed by the host: refused.
    h.ok(|n| ToRelay::RendezvousClose { n, id: b64(&rid) })
        .await;
    let e = connect(&r, &id(), As::Pair(rid)).await.err().unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));
    h.refused(code::UNKNOWN, |n| ToRelay::RendezvousClose {
        n,
        id: b64(&rid),
    })
    .await;
}

#[tokio::test]
async fn rendezvous_limits() {
    let r = relay(|_| {}).await;
    let mut h = host(&r).await;
    let open = |id: u8, ttl| {
        move |n| ToRelay::Rendezvous {
            n,
            id: B64(vec![id; 16]),
            ttl,
        }
    };
    // At most 300 s; not 0; 16 bytes.
    h.refused(code::LIMIT, open(1, 301)).await;
    h.refused(code::BAD, open(1, 0)).await;
    h.refused(code::BAD, |n| ToRelay::Rendezvous {
        n,
        id: B64(vec![1; 15]),
        ttl: 60,
    })
    .await;
    // Four at once.
    for i in 1..=4 {
        h.ok(open(i, 300)).await;
    }
    h.refused(code::LIMIT, open(5, 60)).await;
    // Re-opening one only renews it.
    h.ok(open(1, 60)).await;
    // Another host can't take an id in use.
    let mut h2 = host(&r).await;
    h2.refused(code::BAD, open(1, 60)).await;

    // Three attempts close it.
    for _ in 0..3 {
        let p = connect(&r, &id(), As::Pair([2; 16])).await.unwrap();
        assert!(matches!(h.ctl().await, FromRelay::Link { .. }));
        p.close().await;
        assert!(matches!(h.ctl().await, FromRelay::Unlink { .. }));
    }
    let e = connect(&r, &id(), As::Pair([2; 16])).await.err().unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));

    // It expires after its ttl.
    h.ok(open(9, 1)).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let e = connect(&r, &id(), As::Pair([9; 16])).await.err().unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));

    // A host that goes takes its rendezvous with it.
    h.ok(open(3, 60)).await;
    h.close().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let e = connect(&r, &id(), As::Pair([3; 16])).await.err().unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));
}

#[tokio::test]
async fn mailbox_queues_in_order_until_acked() {
    let r = relay(|_| {}).await;
    let mut h = host(&r).await;
    let dk = id();
    let dev = dk.fingerprint();
    // Not on the list: refused (`err` with `n` 0 answers a binary frame).
    h.send_bin(
        FromHost::Mailbox {
            device: dev,
            payload: vec![1],
        }
        .encode(),
    )
    .await;
    assert_eq!(
        h.ctl().await,
        FromRelay::Err {
            n: 0,
            code: code::UNKNOWN.into()
        }
    );
    h.allow(&[dev]).await;
    for i in 1..=3u8 {
        h.send_bin(
            FromHost::Mailbox {
                device: dev,
                payload: vec![i; 10],
            }
            .encode(),
        )
        .await;
    }
    // Sent while the device is away; delivered after `ready`, in order.
    let mut d = connect(&r, &dk, As::Device(h.fp())).await.unwrap();
    assert_eq!(d.ready.queued, Some(3));
    for i in 1..=3u8 {
        assert_eq!(
            ToDevice::decode(&d.bin().await).unwrap(),
            ToDevice::Mailbox {
                seq: i as u64,
                payload: vec![i; 10]
            }
        );
    }
    // Kept until acknowledged.
    d.ok(|n| ToRelay::Ack {
        n,
        seq: 2,
        device: None,
    })
    .await;
    d.close().await;
    let mut d = connect(&r, &dk, As::Device(h.fp())).await.unwrap();
    assert_eq!(d.ready.queued, Some(1));
    assert_eq!(
        ToDevice::decode(&d.bin().await).unwrap(),
        ToDevice::Mailbox {
            seq: 3,
            payload: vec![3; 10]
        }
    );
    // A connected device gets one at once.
    h.send_bin(
        FromHost::Mailbox {
            device: dev,
            payload: b"now".to_vec(),
        }
        .encode(),
    )
    .await;
    assert_eq!(
        ToDevice::decode(&d.bin().await).unwrap(),
        ToDevice::Mailbox {
            seq: 4,
            payload: b"now".to_vec()
        }
    );

    // The other way: queued for the host while it is away.
    let hk = h.key.clone();
    h.close().await;
    assert!(matches!(d.ctl().await, FromRelay::Host { .. }));
    d.send_bin(FromDevice::Mailbox(b"ask".to_vec()).encode())
        .await;
    d.send_bin(FromDevice::Mailbox(b"ask2".to_vec()).encode())
        .await;
    d.quiet(Duration::from_millis(100)).await;
    let mut h = connect(&r, &hk, As::Host(None)).await.unwrap();
    // The link and both queued messages, in order (link first).
    expect_link(&mut h, &dev, LinkKind::Device).await;
    for (seq, p) in [(1, &b"ask"[..]), (2, b"ask2")] {
        assert_eq!(
            ToHost::decode(&h.bin().await).unwrap(),
            ToHost::Mailbox {
                device: dev,
                seq,
                payload: p.to_vec()
            }
        );
    }
    h.ok(|n| ToRelay::Ack {
        n,
        seq: 2,
        device: Some(b64(&dev)),
    })
    .await;
    // A host's ack must name the device.
    h.refused(code::BAD, |n| ToRelay::Ack {
        n,
        seq: 2,
        device: None,
    })
    .await;
    h.close().await;
    let mut h = connect(&r, &hk, As::Host(None)).await.unwrap();
    expect_link(&mut h, &dev, LinkKind::Device).await;
    h.quiet(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn full_queues_refuse_and_never_drop() {
    let r = relay(|c| {
        c.limits.mailbox_messages = 2;
        c.limits.mailbox_bytes = 100;
    })
    .await;
    let mut h = host(&r).await;
    let dk = id();
    h.allow(&[dk.fingerprint()]).await;
    let send = |payload: Vec<u8>| {
        FromHost::Mailbox {
            device: dk.fingerprint(),
            payload,
        }
        .encode()
    };
    h.send_bin(send(vec![1; 10])).await;
    h.send_bin(send(vec![2; 10])).await;
    h.send_bin(send(vec![3; 10])).await;
    assert_eq!(
        h.ctl().await,
        FromRelay::Err {
            n: 0,
            code: code::FULL.into()
        }
    );
    let mut d = connect(&r, &dk, As::Device(h.fp())).await.unwrap();
    expect_link(&mut h, &dk.fingerprint(), LinkKind::Device).await;
    assert_eq!(d.ready.queued, Some(2));
    for i in 1..=2u8 {
        let ToDevice::Mailbox { payload, .. } = ToDevice::decode(&d.bin().await).unwrap() else {
            panic!()
        };
        assert_eq!(payload, vec![i; 10]);
    }
    d.ok(|n| ToRelay::Ack {
        n,
        seq: 2,
        device: None,
    })
    .await;
    // Bytes: 60 fits, 60 more doesn't.
    h.send_bin(send(vec![4; 60])).await;
    h.send_bin(send(vec![5; 60])).await;
    assert_eq!(
        h.ctl().await,
        FromRelay::Err {
            n: 0,
            code: code::FULL.into()
        }
    );
    assert_eq!(
        ToDevice::decode(&d.bin().await).unwrap(),
        ToDevice::Mailbox {
            seq: 3,
            payload: vec![4; 60]
        }
    );
}

#[tokio::test]
async fn queued_messages_expire() {
    let r = relay(|c| c.limits.mailbox_ttl = Duration::from_millis(300)).await;
    let mut h = host(&r).await;
    let dk = id();
    h.allow(&[dk.fingerprint()]).await;
    h.send_bin(
        FromHost::Mailbox {
            device: dk.fingerprint(),
            payload: vec![1],
        }
        .encode(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut d = connect(&r, &dk, As::Device(h.fp())).await.unwrap();
    assert_eq!(d.ready.queued, Some(0));
    d.quiet(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn queues_can_be_off() {
    let r = relay(|c| c.limits.mailbox_ttl = Duration::ZERO).await;
    let (mut h, mut d, _) = host_and_device(&r).await;
    h.send_bin(
        FromHost::Mailbox {
            device: d.fp(),
            payload: vec![1],
        }
        .encode(),
    )
    .await;
    assert_eq!(
        h.ctl().await,
        FromRelay::Err {
            n: 0,
            code: code::NO_QUEUE.into()
        }
    );
    d.send_bin(FromDevice::Mailbox(vec![1]).encode()).await;
    assert_eq!(
        d.ctl().await,
        FromRelay::Err {
            n: 0,
            code: code::NO_QUEUE.into()
        }
    );
}

#[tokio::test]
async fn signals_and_boot() {
    let r = relay(|_| {}).await;
    let mut h = host(&r).await;
    let dk = id();
    let hfp = h.fp();
    h.refused(code::UNKNOWN, |n| ToRelay::Signal {
        n,
        device: b64(&dk.fingerprint()),
    })
    .await;
    h.allow(&[dk.fingerprint()]).await;
    // Nothing waiting: the cheap check reads 0, 0 and the boot.
    let d = connect(&r, &dk, As::Device(hfp)).await.unwrap();
    assert_eq!((d.ready.signal, d.ready.queued), (Some(0), Some(0)));
    let boot = d.ready.boot.clone();
    d.close().await;
    h.ok(|n| ToRelay::Signal {
        n,
        device: b64(&dk.fingerprint()),
    })
    .await;
    let mut d = connect(&r, &dk, As::Device(hfp)).await.unwrap();
    assert_eq!(d.ready.signal, Some(1));
    // A connected device hears a signal at once.
    h.ok(|n| ToRelay::Signal {
        n,
        device: b64(&dk.fingerprint()),
    })
    .await;
    assert_eq!(d.ctl().await, FromRelay::Signal);
    d.ok(|n| ToRelay::AckSignal { n }).await;
    // A device can't send host requests.
    d.refused(code::BAD, |n| ToRelay::Signal {
        n,
        device: b64(&dk.fingerprint()),
    })
    .await;
    d.close().await;
    let d = connect(&r, &dk, As::Device(hfp)).await.unwrap();
    assert_eq!(d.ready.signal, Some(0));
    assert_eq!(d.ready.boot, boot);
    d.close().await;
    drop(h);
    // A restart forgets flags and queues and changes the boot.
    let r = r.restart().await;
    let d = connect(&r, &dk, As::Device(hfp)).await.unwrap();
    assert_ne!(d.ready.boot, boot);
    assert_eq!(d.ready.boot, r.running.boot());
}

#[tokio::test]
async fn no_compression_is_negotiated() {
    let r = relay(|_| {}).await;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = r.url().into_client_request().unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Extensions",
        "permessage-deflate; client_max_window_bits"
            .parse()
            .unwrap(),
    );
    let (_ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert!(
        resp.headers().get("sec-websocket-extensions").is_none(),
        "{:?}",
        resp.headers()
    );
    // Only /v1.
    let other = r.url().replace("/v1", "/v2");
    assert!(tokio_tungstenite::connect_async(other).await.is_err());
}

#[tokio::test]
async fn unauthenticated_connections_per_address() {
    let r = relay(|c| c.unauthenticated_per_address = 2).await;
    let a = open(&r.url()).await;
    let _b = open(&r.url()).await;
    let e = handshake(open(&r.url()).await, &r.pin(), &id(), &As::Host(None))
        .await
        .err()
        .unwrap();
    assert_eq!(e.code, Some(close::LIMIT));
    assert!(
        matches!(e.bye, Some(FromRelay::Bye { ref code, retry_after: Some(_) }) if code == code::LIMIT)
    );
    // An authenticated connection doesn't count; a closed one frees its slot.
    drop(a);
    let t = r.token();
    let mut ok = None;
    for _ in 0..50 {
        match connect(&r, &id(), As::Host(Some(t.clone()))).await {
            Ok(c) => {
                ok = Some(c);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    assert!(ok.is_some());
    connect(&r, &id(), As::Host(Some(r.token()))).await.unwrap();
}

#[tokio::test]
async fn idle_connections_are_closed() {
    let r = relay(|c| {
        c.timeouts.ping = Duration::from_millis(100);
        c.timeouts.idle = Duration::from_millis(400);
    })
    .await;
    // Answering pings keeps it open.
    let mut h = host(&r).await;
    h.quiet(Duration::from_millis(700)).await;
    h.ok(|n| ToRelay::Devices { n, allow: vec![] }).await;
    // Silence closes it: `bye` `idle`, then 4408.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let (bye, c) = h.bye_and_close().await;
    assert_eq!(c, Some(close::TOO_SLOW));
    assert!(matches!(bye, Some(FromRelay::Bye { code, .. }) if code == code::IDLE));
}

#[tokio::test]
async fn bytes_per_connection_are_rate_limited() {
    let r = relay(|c| {
        c.limits.bytes_per_sec = 100_000;
        c.limits.burst_bytes = 10_000;
    })
    .await;
    let (mut h, mut d, _) = host_and_device(&r).await;
    let start = std::time::Instant::now();
    for _ in 0..3 {
        d.send_bin(FromDevice::Live(vec![0; 19_000]).encode()).await;
    }
    for _ in 0..3 {
        h.bin().await;
    }
    // 57 KB over a 10 KB burst at 100 KB/s: about 0.47 s; nothing dropped.
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(400), "{took:?}");
}

#[tokio::test]
async fn control_messages_are_numbered_and_checked() {
    let r = relay(|_| {}).await;
    let (mut h, mut d, _) = host_and_device(&r).await;
    // Unparseable: `err` `bad` with `n` 0.
    h.ws.send(Message::text("{\"t\":\"nope\"}")).await.unwrap();
    assert_eq!(
        h.ctl().await,
        FromRelay::Err {
            n: 0,
            code: code::BAD.into()
        }
    );
    // A device can't do host things; a host can't `ack_signal`.
    d.refused(code::BAD, |n| ToRelay::Devices { n, allow: vec![] })
        .await;
    h.refused(code::BAD, |n| ToRelay::AckSignal { n }).await;
    // A bad binary frame.
    h.send_bin(vec![9, 9]).await;
    assert_eq!(
        h.ctl().await,
        FromRelay::Err {
            n: 0,
            code: code::BAD.into()
        }
    );
    // A frame over 20 KiB ends the connection.
    d.send_bin(vec![1; 21 * 1024]).await;
    assert_eq!(d.closed().await, Some(1009));
    assert!(matches!(h.ctl().await, FromRelay::Unlink { .. }));
}
