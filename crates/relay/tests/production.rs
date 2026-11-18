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
use rlord_relay_proto::frame::{FromDevice, ToHost};
use rlord_relay_proto::msg::{close, code, FromRelay, HostState, LinkKind};
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

#[tokio::test]
async fn a_slow_receiver_stops_the_sender_and_nothing_is_dropped() {
    let stall = Duration::from_secs(2);
    let r = relay(|c| c.timeouts.stall = stall).await;
    let mut h = host(&r).await;
    let d = device_of(&r, &mut h).await;
    let dfp = d.fp();
    let (mut tx, mut rx) = d.ws.split();

    // The device sends far more than any buffer holds; the host reads
    // nothing.
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
    // The relay stops reading the device: its received bytes stop growing
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

    // After the stall limit the host is closed; the device hears its host
    // go and is told its frames are undeliverable.
    let mut offline = false;
    let mut undeliverable = false;
    let start = std::time::Instant::now();
    while !(offline && undeliverable) {
        match tokio::time::timeout(stall * 3, rx.next()).await.unwrap() {
            Some(Ok(Message::Text(t))) => match FromRelay::parse(&t).unwrap() {
                FromRelay::Host {
                    state: HostState::Offline,
                } => offline = true,
                FromRelay::Undeliverable { link: None } => undeliverable = true,
                other => panic!("{other:?}"),
            },
            Some(Ok(_)) => {}
            other => panic!("{other:?}"),
        }
    }
    assert!(start.elapsed() < stall * 3);
    sender.abort();

    // The host got a gapless prefix of the device's frames, then the close.
    let mut next = 0u32;
    loop {
        match h.recv().await {
            Got::Bin(b) => {
                let ToHost::Live { payload, .. } = ToHost::decode(&b).unwrap() else {
                    panic!()
                };
                let i = u32::from_be_bytes(payload[..4].try_into().unwrap());
                assert_eq!(i, next, "a frame was dropped or reordered");
                next += 1;
            }
            Got::Ctl(FromRelay::Bye { code: c, .. }) => assert_eq!(c, code::IDLE),
            Got::Ctl(other) => panic!("{other:?}"),
            Got::Closed(_) => break,
        }
    }
    assert!(next > 0 && next < COUNT, "{next}");
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
