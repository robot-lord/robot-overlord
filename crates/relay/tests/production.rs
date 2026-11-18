//! What slice 3 builds for a production relay (docs/working/web-ui-remote.md,
//! "Production relay"): suspension, backpressure without dropping,
//! `max_connections`, `bye`, a pending enrolment, per-account counters.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use rlord_relay::{Admin, Authorizer, Decision, HostRequest, Limits};
use rlord_relay_proto::frame::{FromDevice, FromHost, ToDevice, ToHost};
use rlord_relay_proto::msg::{close, code, FromRelay, LinkKind, ToRelay};
use rlord_relay_proto::B64;
use tokio_tungstenite::tungstenite::Message;

async fn device_of(r: &Relay, h: &mut Client) -> Client {
    let dk = id();
    h.allow(&[dk.fingerprint()]).await;
    let d = connect(r, &dk, As::Device(h.fp())).await.unwrap();
    assert!(matches!(
        h.ctl().await,
        FromRelay::Link {
            kind: LinkKind::Device,
            ..
        }
    ));
    d
}

#[tokio::test]
async fn suspended_hosts_and_their_devices_get_4402() {
    let r = relay(|c| c.timeouts.sweep = Duration::from_millis(100)).await;
    let mut h = host(&r).await;
    let mut d = device_of(&r, &mut h).await;
    let (hk, dk) = (h.key.clone(), d.key.clone());

    // Suspended by the CLI (another process's handle on the store): the
    // relay closes both within a sweep, `bye` `suspended` first.
    let cli = Admin::open(&r.cfg.data_dir).unwrap();
    assert!(cli.host_suspend(&hk.fingerprint()).unwrap());
    for c in [&mut h, &mut d] {
        let (bye, code) = c.bye_and_close().await;
        assert_eq!(code, Some(close::SUSPENDED));
        assert!(matches!(bye, Some(FromRelay::Bye { code: c, .. }) if c == code::SUSPENDED));
    }
    // And refuses them; clients don't retry a final code.
    for (k, who) in [(&hk, As::Host(None)), (&dk, As::Device(hk.fingerprint()))] {
        let e = connect(&r, k, who).await.err().unwrap();
        assert_eq!(e.code, Some(close::SUSPENDED));
    }
    assert!(close::is_final(close::SUSPENDED));
    let hosts = cli.hosts().unwrap();
    assert_eq!(hosts[0].status, rlord_relay::Status::Suspended);

    // Resumed: back in.
    assert!(cli.host_resume(&hk.fingerprint()).unwrap());
    let mut h = connect(&r, &hk, As::Host(None)).await.unwrap();
    let _d = connect(&r, &dk, As::Device(hk.fingerprint()))
        .await
        .unwrap();
    assert!(matches!(h.ctl().await, FromRelay::Link { .. }));

    // In process (the relay's own admin) it is immediate.
    r.running.admin().host_suspend(&hk.fingerprint()).unwrap();
    assert_eq!(h.closed().await, Some(close::SUSPENDED));
    // A removed host's connection goes too.
    let admin = r.running.admin();
    admin.host_resume(&hk.fingerprint()).unwrap();
    let mut h = connect(&r, &hk, As::Host(None)).await.unwrap();
    assert!(admin.host_remove(&hk.fingerprint()).unwrap());
    assert_eq!(h.closed().await, Some(close::NOT_ALLOWED));
    let e = connect(&r, &dk, As::Device(hk.fingerprint()))
        .await
        .err()
        .unwrap();
    assert_eq!(e.code, Some(close::NOT_ALLOWED));
}

/// The link id in the next `link` the host reads.
async fn link_of(h: &mut Client) -> u32 {
    match h.ctl().await {
        FromRelay::Link { id, .. } => id,
        other => panic!("expected link, got {other:?}"),
    }
}

#[tokio::test]
async fn a_stalled_link_closes_its_device_not_the_host() {
    let stall = Duration::from_secs(2);
    let r = relay(|c| c.timeouts.stall = stall).await;
    let mut h = host(&r).await;
    let (k1, k2) = (id(), id());
    h.allow(&[k1.fingerprint(), k2.fingerprint()]).await;
    let d1 = connect(&r, &k1, As::Device(h.fp())).await.unwrap();
    let link1 = link_of(&mut h).await;
    let mut d2 = connect(&r, &k2, As::Device(h.fp())).await.unwrap();
    let link2 = link_of(&mut h).await;
    let dfp = d1.fp();
    let (mut tx, mut rx) = d1.ws.split();

    // The first device sends far more than any buffer holds; the host
    // reads nothing.
    const FRAME: usize = 19_000;
    const COUNT: u32 = 2000;
    let sender = tokio::spawn(async move {
        for i in 0..COUNT {
            let mut p = i.to_be_bytes().to_vec();
            p.resize(FRAME, 0);
            if tx
                .send(Message::binary(FromDevice::Live(p).encode()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    // The relay stops reading that device: its received bytes stop growing
    // well short of what was sent.
    let meter = |r: &Relay| r.running.meter().identity(&dfp).bytes_in;
    let mut last = 0;
    let mut still = 0;
    while still < 6 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = meter(&r);
        if now == last && now > 0 {
            still += 1;
        } else {
            still = 0;
        }
        last = now;
    }
    assert!(
        (last as usize) < FRAME * COUNT as usize / 2,
        "read {last} bytes"
    );

    // After the stall limit that device is closed with "reconnect now";
    // the host isn't.
    let start = std::time::Instant::now();
    let code = loop {
        match tokio::time::timeout(stall * 3, rx.next()).await.unwrap() {
            Some(Ok(Message::Close(c))) => break c.map(|c| u16::from(c.code)),
            Some(Ok(_)) => {}
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(code, Some(close::RESET));
    assert!(start.elapsed() < stall * 3);
    sender.abort();

    // The host gets a gapless prefix of that device's frames, then its
    // `unlink`, and stays connected.
    let mut next = 0u32;
    loop {
        match h.recv().await {
            Got::Bin(b) => {
                let ToHost::Live { payload, link } = ToHost::decode(&b).unwrap() else {
                    panic!()
                };
                assert_eq!(link, link1);
                let i = u32::from_be_bytes(payload[..4].try_into().unwrap());
                assert_eq!(i, next, "a frame was dropped or reordered");
                next += 1;
            }
            Got::Ctl(FromRelay::Unlink { id }) => {
                assert_eq!(id, link1);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(next > 0 && next < COUNT, "{next}");
    // The other device's link still works both ways.
    d2.send_bin(FromDevice::Live(b"still here".to_vec()).encode())
        .await;
    assert_eq!(
        ToHost::decode(&h.bin().await).unwrap(),
        ToHost::Live {
            link: link2,
            payload: b"still here".to_vec()
        }
    );
    h.send_bin(
        FromHost::Live {
            link: link2,
            payload: b"and you".to_vec(),
        }
        .encode(),
    )
    .await;
    assert_eq!(
        ToDevice::decode(&d2.bin().await).unwrap(),
        ToDevice::Live(b"and you".to_vec())
    );
    h.ok(|n| ToRelay::Devices {
        n,
        allow: vec![B64(k2.fingerprint().to_vec())],
    })
    .await;
}

#[tokio::test]
async fn a_device_that_doesnt_read_is_closed_alone() {
    // A long stall limit: nothing below may wait for it.
    let stall = Duration::from_secs(5);
    let r = relay(|c| c.timeouts.stall = stall).await;
    let mut h = host(&r).await;
    let (k1, k2) = (id(), id());
    h.allow(&[k1.fingerprint(), k2.fingerprint()]).await;
    let d1 = connect(&r, &k1, As::Device(h.fp())).await.unwrap();
    let link1 = link_of(&mut h).await;
    let d2 = connect(&r, &k2, As::Device(h.fp())).await.unwrap();
    let link2 = link_of(&mut h).await;
    let (mut htx, mut hrx) = h.ws.split();

    // The second device reads everything, counting.
    let (_, mut rx2) = d2.ws.split();
    let got2 = Arc::new(AtomicUsize::new(0));
    let counter = got2.clone();
    let reader2 = tokio::spawn(async move {
        while let Some(Ok(m)) = rx2.next().await {
            if let Message::Binary(b) = m {
                assert_eq!(ToDevice::decode(&b).unwrap(), ToDevice::Live(vec![2; 100]));
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    // The host sends the first device (which reads nothing) far more than
    // its buffer holds, and the second a frame every so often.
    const FRAME: usize = 19_000;
    const COUNT: usize = 2000;
    let start = std::time::Instant::now();
    let sender = tokio::spawn(async move {
        let mut to2 = 0;
        for i in 0..COUNT {
            let f = FromHost::Live {
                link: link1,
                payload: vec![1; FRAME],
            };
            htx.send(Message::binary(f.encode())).await.unwrap();
            if i % 50 == 0 {
                let f = FromHost::Live {
                    link: link2,
                    payload: vec![2; 100],
                };
                htx.send(Message::binary(f.encode())).await.unwrap();
                to2 += 1;
            }
        }
        (to2, htx)
    });
    // The host hears the first device's frames are undeliverable, then
    // its `unlink`.
    let mut undeliverable = false;
    loop {
        match tokio::time::timeout(WAIT, hrx.next()).await.unwrap() {
            Some(Ok(Message::Text(t))) => match FromRelay::parse(&t).unwrap() {
                FromRelay::Undeliverable { link } => {
                    assert_eq!(link, Some(link1));
                    undeliverable = true;
                }
                FromRelay::Unlink { id } => {
                    assert_eq!(id, link1);
                    break;
                }
                other => panic!("{other:?}"),
            },
            Some(Ok(_)) => {}
            other => panic!("{other:?}"),
        }
    }
    assert!(undeliverable);
    // That device is closed with "reconnect now" after what it was sent.
    let mut d1 = d1;
    assert_eq!(d1.closed().await, Some(close::RESET));
    // The host's reader never waited on it: everything went through well
    // inside the stall limit, and the other device got all of its frames.
    let (to2, _htx) = sender.await.unwrap();
    assert!(start.elapsed() < stall, "{:?}", start.elapsed());
    eventually("the second device's frames", || {
        got2.load(Ordering::Relaxed) == to2
    })
    .await;
    reader2.abort();
}

#[tokio::test]
async fn a_dropped_link_tells_its_device_to_reconnect_now() {
    let r = relay(|_| {}).await;
    let mut h = host(&r).await;
    let mut d = device_of(&r, &mut h).await;
    let link = 1;
    h.ok(|n| ToRelay::Drop { n, link }).await;
    assert_eq!(d.closed().await, Some(close::RESET));
    assert!(!close::is_final(close::RESET));
    assert_eq!(h.ctl().await, FromRelay::Unlink { id: link });
}

#[tokio::test]
async fn at_max_connections_an_upgrade_gets_503() {
    let r = relay(|c| c.max_connections = 2).await;
    let _a = open(&r.url()).await;
    let b = open(&r.url()).await;
    match tokio_tungstenite::connect_async(r.url()).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), 503);
            assert_eq!(resp.headers()["retry-after"], "5");
        }
        other => panic!("expected 503, got {:?}", other.map(|_| ())),
    }
    // A slot frees when a connection ends.
    drop(b);
    let mut ok = false;
    for _ in 0..100 {
        if tokio_tungstenite::connect_async(r.url()).await.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(ok);
}

#[tokio::test]
async fn shutdown_says_bye_with_retry_after() {
    let r = relay(|_| {}).await;
    let mut h = host(&r).await;
    let Relay { running, dir, .. } = r;
    let stop = tokio::spawn(running.shutdown());
    let (bye, c) = h.bye_and_close().await;
    assert_eq!(c, Some(close::OVERLOADED));
    assert_eq!(
        bye,
        Some(FromRelay::Bye {
            code: code::OVERLOADED.into(),
            retry_after: Some(5)
        })
    );
    assert!(!close::is_final(close::OVERLOADED));
    stop.await.unwrap();
    drop(dir);
}

/// An account service's authorizer, stood in: pending twice, then admit.
struct Pending {
    asked: AtomicUsize,
    expires: u64,
}

impl Authorizer for Pending {
    fn host(&self, _req: &HostRequest<'_>) -> anyhow::Result<Decision> {
        Ok(if self.asked.fetch_add(1, Ordering::Relaxed) < 2 {
            Decision::Pending {
                url: "https://accounts.example/enroll".into(),
                code: "WXYZ-1234".into(),
                expires: self.expires,
            }
        } else {
            Decision::Admit {
                account: "acct".into(),
                limits: Limits::default(),
            }
        })
    }

    fn limits(&self, _account: &str) -> Limits {
        Limits::default()
    }
}

#[tokio::test]
async fn a_pending_enrolment_shows_enroll_then_ready() {
    let far = rlord_relay::store::now() + 60;
    let r = relay(|c| {
        c.timeouts.enroll_poll = Duration::from_millis(50);
        c.authorizer = Some(Arc::new(Pending {
            asked: AtomicUsize::new(0),
            expires: far,
        }));
    })
    .await;
    let hk = id();
    let h = connect(&r, &hk, As::Host(None)).await.unwrap();
    // Shown once, then `ready`.
    assert_eq!(
        h.enroll,
        [FromRelay::Enroll {
            url: "https://accounts.example/enroll".into(),
            code: "WXYZ-1234".into(),
            expires: far
        }]
    );
    // The host now has a record under the account its devices join.
    let hosts = r.running.admin().hosts().unwrap();
    assert_eq!(hosts[0].account, "acct");

    // One that runs out: 4408.
    let r = relay(|c| {
        c.authorizer = Some(Arc::new(Pending {
            asked: AtomicUsize::new(0),
            expires: rlord_relay::store::now(),
        }))
    })
    .await;
    let e = connect(&r, &id(), As::Host(None)).await.err().unwrap();
    assert_eq!(e.code, Some(close::TOO_SLOW));
}

#[tokio::test]
async fn per_account_counters_add_up_across_a_host_and_its_devices() {
    let r = relay(|_| {}).await;
    let t = r
        .running
        .admin()
        .token_new(1, Duration::from_secs(60), "acme")
        .unwrap();
    let mut h = connect(&r, &id(), As::Host(Some(t))).await.unwrap();
    let mut d1 = device_of(&r, &mut h).await;
    let dk2 = id();
    h.allow(&[d1.fp(), dk2.fingerprint()]).await;
    let mut d2 = connect(&r, &dk2, As::Device(h.fp())).await.unwrap();
    let link2 = match h.ctl().await {
        FromRelay::Link { id, .. } => id,
        other => panic!("{other:?}"),
    };
    for _ in 0..3 {
        d1.send_bin(FromDevice::Live(vec![1; 100]).encode()).await;
        h.bin().await;
    }
    h.send_bin(
        rlord_relay_proto::frame::FromHost::Live {
            link: link2,
            payload: vec![2; 50],
        }
        .encode(),
    )
    .await;
    d2.bin().await;
    // Another account's host doesn't count.
    let _other = host(&r).await;

    let m = r.running.meter();
    let acme = m.account("acme");
    let ids = [h.fp(), d1.fp(), d2.fp()].map(|fp| m.identity(&fp));
    let sum = |f: fn(&rlord_relay::Usage) -> u64| ids.iter().map(f).sum::<u64>();
    assert_eq!(acme.bytes_in, sum(|u| u.bytes_in));
    assert_eq!(acme.bytes_out, sum(|u| u.bytes_out));
    assert_eq!(acme.messages_in, sum(|u| u.messages_in));
    assert_eq!(acme.messages_out, sum(|u| u.messages_out));
    assert_eq!(acme.connections, 3);
    assert_eq!(acme.open, 3);
    assert!(ids[1].bytes_in >= 3 * 101);
    assert!(ids[0].bytes_out >= 3 * 105);
    assert!(ids[2].bytes_out >= 51);
    assert_eq!(m.account("local").connections, 1);
    d1.close().await;
    eventually("the device's connection to close", || {
        r.running.meter().account("acme").open == 2
    })
    .await;
}

#[tokio::test]
async fn unused_and_removed_hosts_are_forgotten() {
    let r = relay(|c| {
        c.timeouts.sweep = Duration::from_millis(100);
        c.host_expiry = Some(Duration::ZERO);
    })
    .await;
    // Removed by the CLI while connected: closed within a sweep (4403),
    // devices too.
    let mut h = host(&r).await;
    let mut d = device_of(&r, &mut h).await;
    let cli = Admin::open(&r.cfg.data_dir).unwrap();
    assert!(cli.host_remove(&h.fp()).unwrap());
    assert_eq!(h.closed().await, Some(close::NOT_ALLOWED));
    assert_eq!(d.closed().await, Some(close::NOT_ALLOWED));

    // Not seen for longer than `host_expiry`: forgotten while away, kept
    // while connected.
    let away = host(&r).await;
    let here = host(&r).await;
    away.close().await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    eventually("the unused host to be forgotten", || {
        let hosts = cli.hosts().unwrap();
        hosts.len() == 1 && hosts[0].fingerprint == here.fp()
    })
    .await;
}
