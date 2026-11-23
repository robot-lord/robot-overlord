//! The machine against a scripted link, a fake host (the real `rlord_e2e`
//! responder side) and a fake clock. Each row of the plan's reconnect
//! table has a test of its name.

use super::*;
use crate::soft::SoftSigners;
use rlord_e2e::{Responder, StaticKey};

const ORIGIN: &str = "http://127.0.0.1:9";
const RELAY_FP: [u8; 32] = [9; 32];
const HOST_FP: [u8; 32] = [8; 32];

/// The host's end of the content layer.
struct FakeHost {
    key: StaticKey,
    device: [u8; 32],
    session: Option<Session>,
    reasm: Reassembler,
    mailbox: Option<MailboxSession>,
    /// Requests and events the device sent on the live session.
    got: Vec<Value>,
    /// Requests the device sent by mailbox.
    mb_got: Vec<Value>,
    /// Message 1s seen.
    handshakes: usize,
    /// Answer handshakes (off: message 1 is swallowed).
    answer: bool,
    rev: String,
    api: Option<u64>,
    /// What its hello says of the device's mailbox.
    hello_mailbox: HelloMailbox,
    /// Kind-3 message 1s answered (mailboxes keyed).
    keyings: usize,
    /// A pairing offer the host shows.
    qr: Option<QrPayload>,
    /// The pairing's short code, at the host.
    code: Option<u32>,
}

impl FakeHost {
    fn relay() -> RelayId {
        RelayId::new(ORIGIN, RELAY_FP)
    }

    fn encrypt(&mut self, v: &Value) -> Vec<Vec<u8>> {
        let s = self.session.as_mut().expect("a host session");
        stream::fragment(CONTROL, v.to_string().as_bytes())
            .unwrap()
            .iter()
            .map(|f| s.encrypt(f).unwrap())
            .collect()
    }

    fn mailbox_frame(&mut self, v: &Value) -> Vec<u8> {
        let m = self.mailbox.as_mut().expect("a host mailbox");
        let f = stream::fragment(CONTROL, v.to_string().as_bytes()).unwrap();
        let mut p = m.id().to_vec();
        p.extend(m.encrypt(&f[0]).unwrap());
        p
    }
}

/// The `mailbox` field of the fake host's hello.
enum HelloMailbox {
    /// Its mailbox's id, or null for none (the host today).
    Current,
    /// No field (an older host).
    Absent,
    /// This value.
    Fixed(Value),
}

struct World {
    m: Machine,
    now: Now,
    signers: Arc<SoftSigners>,
    /// Every action not yet looked at.
    pending: Vec<Action>,
    /// What is on disk (the last save).
    disk: Option<Vec<u8>>,
    host: FakeHost,
    /// What the device told the relay (parsed), and frames it sent.
    relay_got: Vec<ToRelay>,
    page: Vec<Value>,
    notified: Vec<u32>,
    bg_done: Vec<BackgroundOutcome>,
    dials: Vec<u64>,
    closes: Vec<(u64, u16)>,
    /// Every lane-2 frame the device sent, as sent.
    mb_raw: Vec<Vec<u8>>,
    /// Pings the driver was asked for.
    pings: usize,
    /// Saves and frames in the order carried out.
    order: Vec<&'static str>,
    /// The current link id (from the last Dial).
    link: u64,
    boot: String,
}

fn now0() -> Now {
    Now {
        mono: 1_000_000,
        wall: 1_700_000_000_000,
    }
}

fn record(device: &StaticKey, host: &StaticKey, label: &str) -> Record {
    Record {
        v: RECORD_VERSION,
        device_name: "phone".into(),
        static_key: b64(device.private()),
        label: label.into(),
        host: HostRecord {
            static_key: b64(host.public()),
            pin: RelayAddress {
                origin: ORIGIN.into(),
                fingerprint: RELAY_FP,
            }
            .to_string(),
            fp: b64_fingerprint(&HOST_FP),
            name: "mac".into(),
        },
        mailbox: None,
        mailbox_seen: None,
        boot: None,
        rev: None,
        outbox: Outbox::default(),
        notified: vec![],
    }
}

impl World {
    /// A device paired with a fake host, in the foreground, not linked.
    fn paired() -> World {
        let host = StaticKey::from_private([2; 32]);
        let device = StaticKey::from_private([1; 32]);
        let signers = Arc::new(SoftSigners::new());
        let label = store::label_for(host.public());
        signers.create(label.clone()).unwrap();
        let r = record(&device, &host, &label);
        let disk = Some(r.encode());
        let m = Machine::new(signers.clone(), vec![r], 7);
        let mut w = World::with(m, signers, host, *device.public());
        w.disk = disk;
        w
    }

    fn with(m: Machine, signers: Arc<SoftSigners>, host: StaticKey, device: [u8; 32]) -> World {
        World {
            m,
            now: now0(),
            signers,
            pending: vec![],
            disk: None,
            host: FakeHost {
                key: host,
                device,
                session: None,
                reasm: Reassembler::new(),
                mailbox: None,
                got: vec![],
                mb_got: vec![],
                handshakes: 0,
                answer: true,
                rev: "r1".into(),
                api: Some(1),
                hello_mailbox: HelloMailbox::Current,
                keyings: 0,
                qr: None,
                code: None,
            },
            relay_got: vec![],
            page: vec![],
            notified: vec![],
            bg_done: vec![],
            dials: vec![],
            closes: vec![],
            mb_raw: vec![],
            pings: 0,
            order: vec![],
            link: 0,
            boot: "boot1".into(),
        }
    }

    /// Restart from what is on disk (a crash, or the app killed).
    fn restart(self) -> World {
        let saved: Vec<Record> = self
            .disk
            .as_ref()
            .and_then(|b| Record::decode(b))
            .into_iter()
            .collect();
        let m = Machine::new(self.signers.clone(), saved, 11);
        let mut w = World::with(m, self.signers, self.host.key, self.host.device);
        w.disk = self.disk;
        w.host.mailbox = self.host.mailbox;
        w.host.mb_got = self.host.mb_got;
        w.boot = self.boot;
        w.now = Now {
            mono: self.now.mono + 5_000,
            wall: self.now.wall + 5_000,
        };
        w
    }

    fn input(&mut self, i: Input) {
        self.m.handle(self.now, i);
        while let Some(a) = self.m.poll() {
            self.pending.push(a);
        }
        self.apply();
    }

    /// Carry out the queued actions the way the driver would (and the
    /// relay and the host behind it).
    fn apply(&mut self) {
        let actions: Vec<Action> = std::mem::take(&mut self.pending);
        let mut replies = Vec::new();
        for a in actions {
            match a {
                Action::Save { plain, .. } => {
                    let mb = Record::decode(&plain).is_some_and(|r| r.mailbox.is_some());
                    self.order.push(if mb { "save+mb" } else { "save" });
                    self.disk = Some(plain);
                }
                Action::Delete { .. } => self.disk = None,
                Action::Dial { link, .. } => {
                    self.dials.push(link);
                    self.link = link;
                }
                Action::Close { link, code } => self.closes.push((link, code)),
                Action::ToPage(s) => self.page.push(serde_json::from_str(&s).unwrap()),
                Action::Notify { count, .. } => self.notified.push(count),
                Action::BackgroundDone(o) => self.bg_done.push(o),
                Action::Ping { .. } => self.pings += 1,
                Action::Text { json, .. } => self.relay_got.push(ToRelay::parse(&json).unwrap()),
                Action::Binary { link, bytes } => match self.frame(&bytes) {
                    FromDevice::Live(p) => replies.extend(self.host_live(link, &p)),
                    FromDevice::Mailbox(p) => {
                        // The same bytes again are the same frame (a
                        // refused one offered again), opened once.
                        if !self.mb_raw.contains(&p) {
                            let v = self.host_mailbox(&p);
                            self.host.mb_got.push(v);
                        }
                        self.mb_raw.push(p);
                    }
                },
            }
        }
        for (link, frames) in replies {
            for f in frames {
                self.input(Input::Binary {
                    link,
                    bytes: ToDevice::Live(f).encode(),
                });
            }
        }
    }

    /// The host reads a lane-1 frame; what it answers.
    fn host_live(&mut self, link: u64, p: &[u8]) -> Option<(u64, Vec<Vec<u8>>)> {
        if p.len() > 2 && p[0] == 1 && p[1] == 1 && self.host.session.is_none() {
            let qr = self.host.qr.as_ref()?;
            let (est, m2) =
                Responder::pair(&self.host.key, qr, &FakeHost::relay(), p, None).unwrap();
            self.host.code = Some(est.short_code());
            self.host.device = est.remote_static;
            self.host.session = Some(est.into_session().unwrap());
            return Some((link, vec![m2]));
        }
        if p.len() > 2 && p[0] == 1 && p[1] == 2 && self.host.session.is_none() {
            self.host.handshakes += 1;
            if !self.host.answer {
                return None;
            }
            let (est, m2) = Responder::reconnect(
                &self.host.key,
                &self.host.device,
                &FakeHost::relay(),
                p,
                None,
            )
            .unwrap();
            self.host.session = Some(est.into_session().unwrap());
            return Some((link, vec![m2]));
        }
        let s = self.host.session.as_mut()?;
        let plain = s.decrypt(p).ok()?;
        let msg = self.host.reasm.push(&plain).unwrap()?;
        let v: Value = serde_json::from_slice(&msg.body).unwrap();
        match v["event"].as_str() {
            Some("hello") => {
                let mut h = json!({"event": "hello", "version": "x", "rev": self.host.rev,
                    "device": device_id(&self.host.device)});
                if let Some(a) = self.host.api {
                    h["api"] = json!(a);
                }
                match &self.host.hello_mailbox {
                    HelloMailbox::Current => {
                        h["mailbox"] = json!(self.host.mailbox.as_ref().map(|m| b64(&m.id())))
                    }
                    HelloMailbox::Absent => {}
                    HelloMailbox::Fixed(v) => h["mailbox"] = v.clone(),
                }
                Some((link, self.host.encrypt(&h)))
            }
            Some("mailbox_key") => {
                let m1 = unb64(v["msg"].as_str().unwrap()).unwrap();
                let (est, m2) = Responder::mailbox(
                    &self.host.key,
                    &self.host.device,
                    &FakeHost::relay(),
                    &m1,
                    None,
                )
                .unwrap();
                self.host.mailbox = Some(est.into_mailbox().unwrap());
                self.host.keyings += 1;
                let ans = json!({"event": "mailbox_key", "msg": b64(&m2)});
                Some((link, self.host.encrypt(&ans)))
            }
            Some("mailbox_ready") => {
                let f = unb64(v["frame"].as_str().unwrap()).unwrap();
                let m = self.host.mailbox.as_mut().unwrap();
                assert_eq!(f[..8], m.id());
                m.decrypt(&f[8..]).expect("the first mailbox frame opens");
                None
            }
            _ => {
                self.host.got.push(v);
                None
            }
        }
    }

    fn frame(&mut self, bytes: &[u8]) -> FromDevice {
        self.order.push("frame");
        FromDevice::decode(bytes).unwrap()
    }

    fn host_mailbox(&mut self, p: &[u8]) -> Value {
        let m = self.host.mailbox.as_mut().expect("a host mailbox");
        assert_eq!(p[..8], m.id());
        let plain = m
            .decrypt(&p[8..])
            .expect("a mailbox frame opens at the host");
        serde_json::from_slice(&stream::single(&plain).unwrap().body).unwrap()
    }

    /// The host answers on the live session.
    fn host_says(&mut self, v: Value) {
        for f in self.host.encrypt(&v) {
            self.input(Input::Binary {
                link: self.link,
                bytes: ToDevice::Live(f).encode(),
            });
        }
    }

    fn ready(&self, host: HostState) -> Ready {
        Ready {
            boot: self.boot.clone(),
            host: Some(host),
            signal: Some(0),
            queued: Some(0),
            limits: None,
        }
    }

    /// The pending dial reaches `ready`.
    fn link_up(&mut self, host: HostState) {
        let ready = self.ready(host);
        self.link_up_with(ready);
    }

    fn link_up_with(&mut self, ready: Ready) {
        self.host.session = None;
        self.host.reasm = Reassembler::new();
        self.input(Input::Dialed {
            link: self.link,
            result: Ok(Linked {
                ready,
                relay: FakeHost::relay(),
            }),
        });
    }

    fn dial_fails(&mut self, e: DialError) {
        self.input(Input::Dialed {
            link: self.link,
            result: Err(e),
        });
    }

    fn closed(&mut self, code: u16) {
        self.input(Input::Closed {
            link: self.link,
            code,
        });
    }

    fn relay_says(&mut self, m: FromRelay) {
        self.input(Input::Text {
            link: self.link,
            text: m.to_json(),
        });
    }

    fn page_says(&mut self, v: Value) {
        self.input(Input::Page(v.to_string()));
    }

    /// Time passes in one-second ticks (as the driver ticks).
    fn advance(&mut self, ms: u64) {
        let mut left = ms;
        while left > 0 {
            let step = left.min(1000);
            self.now.mono += step;
            self.now.wall += step;
            left -= step;
            self.input(Input::Tick);
        }
    }

    /// Time passes with the relay heard from every 10 s (a link that
    /// isn't silent).
    fn advance_heard(&mut self, ms: u64) {
        let mut left = ms;
        while left > 0 {
            let step = left.min(10_000);
            self.advance(step);
            left -= step;
            self.input(Input::Heard { link: self.link });
        }
    }

    fn foreground(&mut self) {
        self.input(Input::Lifecycle { foreground: true });
    }

    /// In the foreground, linked, live, the mailbox keyed.
    fn live() -> World {
        let mut w = World::paired();
        w.foreground();
        w.link_up(HostState::Online);
        assert!(w.m.live(), "live");
        w
    }

    fn page_events(&self, name: &str) -> Vec<Value> {
        self.page
            .iter()
            .filter(|v| v["event"] == name)
            .cloned()
            .collect()
    }

    fn last_conn(&self) -> String {
        self.page_events("conn")
            .last()
            .map(|v| v["state"].as_str().unwrap().to_string())
            .unwrap_or_default()
    }

    fn reply(&self, id: u64) -> Option<Value> {
        self.page.iter().find(|v| v["id"] == id).cloned()
    }

    fn saved(&self) -> Record {
        Record::decode(self.disk.as_ref().unwrap()).unwrap()
    }

    fn requests(&self) -> Vec<Value> {
        self.host
            .got
            .iter()
            .filter(|v| v.get("op").is_some())
            .cloned()
            .collect()
    }

    /// The link closes; the next dial reaches `ready` with the host
    /// online (a fresh session and hello).
    fn reconnect(&mut self) {
        self.closed(1006);
        let d = self.m.dial_at.unwrap() - self.now.mono;
        self.advance(d);
        self.link_up(HostState::Online);
        assert!(self.m.live(), "live again");
    }

    fn ask(&mut self, page_id: u64, text: &str) {
        self.page_says(json!({"id": page_id, "op": "overlord.ask", "args": {"text": text}}));
    }
}

// ---- the reconnect table ----

#[test]
fn backoff_grows_and_resets() {
    let mut w = World::paired();
    w.foreground();
    assert_eq!(w.dials.len(), 1, "dials at once");
    let mut delays = Vec::new();
    for _ in 0..9 {
        w.dial_fails(DialError::Other("refused".into()));
        let at = w.m.dial_at.unwrap() - w.now.mono;
        delays.push(at);
        let n = w.dials.len();
        w.advance(at);
        assert_eq!(w.dials.len(), n + 1);
    }
    // 250 ms doubling, each with up to 20% jitter, to 30 s.
    for (i, d) in delays.iter().enumerate() {
        let base = Machine::backoff_ms(i as u32);
        assert!(*d >= base && *d <= base + base / 5, "{i}: {d} vs {base}");
    }
    assert_eq!(Machine::backoff_ms(8), BACKOFF_MAX_MS);
    // A link that stays ready 10 s resets it.
    w.link_up(HostState::Online);
    w.advance(STABLE_MS);
    w.closed(1006);
    let d = w.m.dial_at.unwrap() - w.now.mono;
    assert!(d <= BACKOFF_MIN_MS + BACKOFF_MIN_MS / 5, "{d}");
}

#[test]
fn reset_reconnects_at_once() {
    let mut w = World::live();
    let n = w.dials.len();
    w.closed(close::RESET);
    assert_eq!(w.dials.len(), n + 1, "redialled with no wait");
}

#[test]
fn repeated_reset_backs_off() {
    let mut w = World::live();
    w.closed(close::RESET);
    w.link_up(HostState::Online);
    w.advance(5_000);
    let n = w.dials.len();
    w.closed(close::RESET);
    assert_eq!(w.dials.len(), n, "a second 4205 within 60 s waits");
    assert!(w.m.dial_at.unwrap() > w.now.mono);
    // Much later, a single one is at once again.
    w.advance(BACKOFF_MAX_MS);
    w.link_up(HostState::Online);
    w.advance_heard(RESET_REPEAT_MS);
    let n = w.dials.len();
    w.closed(close::RESET);
    assert_eq!(w.dials.len(), n + 1);
}

#[test]
fn network_change_redials() {
    let mut w = World::live();
    let n = w.dials.len();
    w.input(Input::Network { online: true });
    assert_eq!(w.closes.last().unwrap().0, n as u64);
    assert_eq!(w.dials.len(), n + 1, "a changed path dials at once");
    assert!(!w.m.live());
}

#[test]
fn offline_stops_dialling() {
    let mut w = World::live();
    w.input(Input::Network { online: false });
    assert_eq!(w.last_conn(), "offline");
    let n = w.dials.len();
    w.advance(120_000);
    assert_eq!(w.dials.len(), n, "no dial without a network");
    w.input(Input::Network { online: true });
    assert_eq!(w.dials.len(), n + 1);
}

#[test]
fn foreground_dials_now() {
    let mut w = World::paired();
    assert!(w.dials.is_empty(), "nothing in the background");
    w.foreground();
    assert_eq!(w.dials.len(), 1);
    // Backgrounded: the link closes cleanly.
    w.link_up(HostState::Online);
    w.input(Input::Lifecycle { foreground: false });
    assert_eq!(w.closes.last(), Some(&(1, 1000)));
    // Failures built a backoff; the foreground resets it and dials now.
    w.foreground();
    w.dial_fails(DialError::Other("x".into()));
    w.dial_fails(DialError::Other("x".into()));
    w.advance(250);
    w.input(Input::Lifecycle { foreground: false });
    let n = w.dials.len();
    w.foreground();
    assert_eq!(w.dials.len(), n + 1);
    // A link found silent is replaced.
    w.link_up(HostState::Online);
    w.now.mono += PING_MS + 1;
    w.now.wall += PING_MS + 1;
    w.input(Input::Lifecycle { foreground: false });
    w.foreground();
    assert_eq!(w.dials.len(), n + 2);
}

#[test]
fn kk_on_host_online() {
    let mut w = World::paired();
    w.foreground();
    w.link_up(HostState::Offline);
    assert_eq!(w.host.handshakes, 0, "no handshake while the host is away");
    assert_eq!(w.last_conn(), "host_offline");
    w.relay_says(FromRelay::Host {
        state: HostState::Online,
    });
    assert_eq!(w.host.handshakes, 1);
    assert!(w.m.live());
    assert_eq!(w.last_conn(), "live");
    let hello = w.page_events("hello").pop().unwrap();
    assert_eq!(hello["page"], device_id(&w.host.device));
    // The host goes and comes back: a new handshake each time.
    w.relay_says(FromRelay::Host {
        state: HostState::Offline,
    });
    assert!(!w.m.live());
    w.host.session = None;
    w.relay_says(FromRelay::Host {
        state: HostState::Online,
    });
    assert_eq!(w.host.handshakes, 2);
    assert!(w.m.live());
}

#[test]
fn no_session_reuse_after_reconnect() {
    let mut w = World::live();
    w.closed(1006);
    w.advance(1_000);
    let old = w.host.session.take();
    w.link_up(HostState::Online);
    assert_eq!(w.host.handshakes, 2, "a fresh handshake");
    assert!(w.m.live());
    // A frame of the old session doesn't open on the new one: it poisons
    // it, and the link is closed.
    let mut old = old.unwrap();
    let f = old
        .encrypt(&stream::fragment(CONTROL, br#"{"event":"changed","rev":"x"}"#).unwrap()[0])
        .unwrap();
    let n = w.closes.len();
    w.input(Input::Binary {
        link: w.link,
        bytes: ToDevice::Live(f).encode(),
    });
    assert_eq!(w.closes.len(), n + 1);
    assert!(!w.m.live());
}

#[test]
fn rev_differs_fetches() {
    let mut w = World::paired();
    w.m.rec.as_mut().unwrap().rev = Some("r0".into());
    w.input(Input::Background { budget_ms: 30_000 });
    w.link_up_with(Ready {
        signal: Some(1),
        ..w.ready(HostState::Online)
    });
    let snap: Vec<Value> = w.requests();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0]["op"], "snapshot");
    // In the foreground, `changed` goes to the page when rev moved.
    let mut w = World::live();
    w.host_says(json!({"event": "changed", "rev": "r2"}));
    assert_eq!(w.page_events("changed").len(), 1);
}

#[test]
fn rev_same_asks_nothing() {
    let mut w = World::paired();
    w.m.rec.as_mut().unwrap().rev = Some("r1".into());
    w.input(Input::Background { budget_ms: 30_000 });
    w.link_up_with(Ready {
        signal: Some(1),
        ..w.ready(HostState::Online)
    });
    assert!(w.requests().is_empty(), "{:?}", w.host.got);
    assert!(matches!(
        w.bg_done.as_slice(),
        [BackgroundOutcome::Refreshed { notified: 0, .. }]
    ));
    // In the foreground, a `changed` with the rev it has is not passed on.
    let mut w = World::live();
    w.host_says(json!({"event": "changed", "rev": "r1"}));
    assert!(w.page_events("changed").is_empty());
}

#[test]
fn ids_survive_restart() {
    let mut w = World::paired();
    w.foreground();
    w.link_up(HostState::Offline);
    w.ask(1, "while away");
    let id = w.saved().outbox.entries[0].id.clone();
    assert_eq!(id.len(), 22, "16 random bytes, base64url");
    let mut w = w.restart();
    assert_eq!(w.m.record().unwrap().outbox.entries[0].id, id);
    w.foreground();
    w.link_up(HostState::Online);
    let reqs = w.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["id"], id.as_str());
    // A fresh page after the restart sees it pending.
    w.page_says(json!({"app": "ready"}));
    let ob = w.page_events("outbox").pop().unwrap();
    assert_eq!(ob["items"][0]["id"], id.as_str());
    assert_eq!(ob["items"][0]["text"], "while away");
}

#[test]
fn resend_by_id_after_reconnect() {
    let mut w = World::live();
    w.page_says(json!({"id": 1, "op": "session.rename", "args": {"ref": "a", "name": "b"}}));
    w.ask(2, "hi");
    let first: Vec<Value> = w.requests();
    assert_eq!(first.len(), 2);
    w.closed(1006);
    w.advance(1_000);
    w.host.got.clear();
    w.link_up(HostState::Online);
    let again = w.requests();
    assert_eq!(again.len(), 2, "both again, oldest first");
    assert_eq!(again[0]["id"], first[0]["id"]);
    assert_eq!(again[1]["id"], first[1]["id"]);
    // Answered: gone from the outbox, the page gets its reply.
    let rid = first[1]["id"].clone();
    w.host_says(json!({"id": rid, "ok": {"queued": true}}));
    assert_eq!(w.reply(2).unwrap()["ok"]["queued"], true);
    assert_eq!(w.saved().outbox.len(), 1);
}

/// Linked with the host offline, a mailbox keyed earlier.
fn offline_with_mailbox() -> World {
    let mut w = World::live();
    assert!(w.m.mailbox.is_some() && w.host.mailbox.is_some());
    w.relay_says(FromRelay::Host {
        state: HostState::Offline,
    });
    w
}

#[test]
fn boot_change_resends_mailbox() {
    let mut w = offline_with_mailbox();
    w.ask(1, "lost in a restart");
    assert_eq!(w.host.mb_got.len(), 1);
    w.relay_says(FromRelay::Queued {
        seq: 1,
        device: None,
    });
    // Same boot on the next link: nothing resent (it is in the queue).
    w.closed(1006);
    w.advance(1_000);
    w.link_up(HostState::Offline);
    assert_eq!(w.host.mb_got.len(), 1);
    // The relay restarted: resent, encrypted afresh, same id.
    w.closed(1006);
    w.advance(1_000);
    w.boot = "boot2".into();
    w.link_up(HostState::Offline);
    assert_eq!(w.host.mb_got.len(), 2);
    assert_eq!(w.host.mb_got[0]["id"], w.host.mb_got[1]["id"]);
}

#[test]
fn mailbox_one_at_a_time() {
    let mut w = offline_with_mailbox();
    w.ask(1, "one");
    w.ask(2, "two");
    w.page_says(json!({"id": 3, "op": "overlord.new_session", "args": {"text": "three"}}));
    assert_eq!(w.host.mb_got.len(), 1, "the next waits for queued");
    w.relay_says(FromRelay::Queued {
        seq: 1,
        device: None,
    });
    assert_eq!(w.host.mb_got.len(), 2);
    w.relay_says(FromRelay::Queued {
        seq: 2,
        device: None,
    });
    assert_eq!(w.host.mb_got.len(), 3);
    assert_eq!(w.host.mb_got[2]["op"], "overlord.new_session");
    // Each saved as queued under this boot.
    w.relay_says(FromRelay::Queued {
        seq: 3,
        device: None,
    });
    assert!(w
        .saved()
        .outbox
        .entries
        .iter()
        .all(|e| e.queued_boot.as_deref() == Some("boot1")));
}

#[test]
fn refused_full_resent_first() {
    let mut w = offline_with_mailbox();
    w.ask(1, "one");
    w.relay_says(FromRelay::Refused {
        code: "full".into(),
        device: None,
    });
    w.ask(2, "two");
    assert_eq!(w.mb_raw.len(), 1, "nothing newer before the refused one");
    // Every 30 s it goes again, the same bytes.
    w.advance(REFUSED_RETRY_MS - 1_000);
    assert_eq!(w.mb_raw.len(), 1);
    w.advance(1_000);
    assert_eq!(w.mb_raw.len(), 2);
    assert_eq!(w.mb_raw[1], w.mb_raw[0]);
    w.relay_says(FromRelay::Refused {
        code: "full".into(),
        device: None,
    });
    // And first on the next link.
    w.closed(1006);
    w.advance(1_000);
    w.link_up(HostState::Offline);
    assert_eq!(w.mb_raw.len(), 3);
    assert_eq!(w.mb_raw[2], w.mb_raw[0]);
    w.relay_says(FromRelay::Queued {
        seq: 1,
        device: None,
    });
    // Then the newer one.
    assert_eq!(w.mb_raw.len(), 4);
    assert_eq!(w.host.mb_got.last().unwrap()["args"]["text"], "two");
}

#[test]
fn no_queue_fails_request() {
    let mut w = offline_with_mailbox();
    w.ask(1, "one");
    w.relay_says(FromRelay::Refused {
        code: "no_queue".into(),
        device: None,
    });
    assert_eq!(w.reply(1).unwrap()["error"]["code"], "host_offline");
    assert!(w.saved().outbox.is_empty());
}

#[test]
fn redelivered_frame_ignored() {
    let mut w = offline_with_mailbox();
    w.ask(1, "one");
    let rid = w.host.mb_got[0]["id"].clone();
    w.relay_says(FromRelay::Queued {
        seq: 1,
        device: None,
    });
    let ans = w
        .host
        .mailbox_frame(&json!({"id": rid, "ok": {"queued": true}}));
    w.input(Input::Binary {
        link: w.link,
        bytes: ToDevice::Mailbox {
            seq: 5,
            payload: ans.clone(),
        }
        .encode(),
    });
    assert_eq!(w.reply(1).unwrap()["ok"]["queued"], true);
    assert_eq!(w.saved().mailbox_seen, Some(("boot1".into(), 5)));
    // Acknowledged after the save.
    assert!(matches!(
        w.relay_got.last(),
        Some(ToRelay::Ack { seq: 5, .. })
    ));
    let mailbox = w.saved().mailbox.clone();
    // The relay delivers it again: acknowledged, not opened.
    w.input(Input::Binary {
        link: w.link,
        bytes: ToDevice::Mailbox {
            seq: 5,
            payload: ans,
        }
        .encode(),
    });
    assert_eq!(w.page.iter().filter(|v| v["id"] == 1).count(), 1);
    assert_eq!(w.saved().mailbox, mailbox, "the mailbox didn't move");
    assert!(matches!(
        w.relay_got.last(),
        Some(ToRelay::Ack { seq: 5, .. })
    ));
}

#[test]
fn duplicate_answer_dropped() {
    let mut w = World::live();
    w.ask(1, "once");
    let rid = w.requests()[0]["id"].clone();
    w.host_says(json!({"id": rid, "ok": {"n": 1}}));
    w.host_says(json!({"id": rid, "ok": {"n": 2}}));
    let replies: Vec<&Value> = w.page.iter().filter(|v| v["id"] == 1).collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["ok"]["n"], 1);
}

#[test]
fn in_progress_waits() {
    let mut w = World::live();
    w.page_says(json!({"id": 1, "op": "session.restart", "args": {"ref": "a"}}));
    let rid = w.requests()[0]["id"].clone();
    w.host_says(json!({"id": rid, "error": {"code": "in_progress", "message": "running"}}));
    assert!(w.reply(1).is_none(), "the page waits");
    assert_eq!(w.saved().outbox.entries[0].state, State::Running);
    // No resend on this link, even after the window: it is running here.
    w.host.got.clear();
    w.advance_heard(outbox::RESEND_WINDOW_MS + 1_000);
    assert!(w.requests().is_empty());
    assert_eq!(w.saved().outbox.entries[0].state, State::Running);
    w.host_says(json!({"id": rid, "ok": {}}));
    assert!(w.reply(1).unwrap().get("ok").is_some());
}

#[test]
fn interrupted_surfaces_once() {
    let mut w = World::live();
    w.ask(1, "did it happen?");
    let rid = w.requests()[0]["id"].clone();
    // The page reloads (a new page): the answer goes to the outbox.
    w.page_says(json!({"app": "ready"}));
    w.host_says(json!({"id": rid, "error": {"code": "interrupted", "message": "x"}}));
    let ob = w.page_events("outbox").pop().unwrap();
    assert_eq!(ob["items"][0]["state"], "interrupted");
    assert_eq!(ob["items"][0]["error"]["code"], "interrupted");
    // Not resent: the user checks, then asks anew (a new id).
    w.host.got.clear();
    w.closed(1006);
    w.advance(1_000);
    w.link_up(HostState::Online);
    assert!(w.requests().is_empty());
    let id = ob["items"][0]["id"].as_str().unwrap().to_string();
    w.page_says(json!({"app": "cancel", "request": id}));
    assert!(w.saved().outbox.is_empty());
}

// ---- the rest of the device's side ----

#[test]
fn final_close_codes_stop_the_loop() {
    for (code, state) in [
        (close::SUSPENDED, "suspended"),
        (close::UNSUPPORTED, "update_app"),
        (close::BAD_SIGNATURE, "update_app"),
        (close::NOT_ALLOWED, "revoked"),
    ] {
        let mut w = World::live();
        w.closed(code);
        assert_eq!(w.last_conn(), state, "{code}");
        let n = w.dials.len();
        w.advance(120_000);
        assert_eq!(w.dials.len(), n, "{code}: not retried");
    }
    // Revoked: the keys and the record are gone.
    let mut w = World::live();
    let label = w.m.record().unwrap().label.clone();
    w.dial_fails(DialError::Closed(close::NOT_ALLOWED));
    w.closed(close::NOT_ALLOWED);
    assert!(w.disk.is_none());
    assert!(w.signers.identity(&label).is_none());
    assert_eq!(w.last_conn(), "revoked");
    // Wrong relay: final, no other address.
    let mut w = World::paired();
    w.foreground();
    w.dial_fails(DialError::WrongRelay("wrong relay: key".into()));
    assert_eq!(w.last_conn(), "wrong_relay");
    w.advance(120_000);
    assert_eq!(w.dials.len(), 1);
}

#[test]
fn bye_retry_after_is_honoured() {
    let mut w = World::live();
    w.relay_says(FromRelay::Bye {
        code: "overloaded".into(),
        retry_after: Some(20),
    });
    w.closed(close::OVERLOADED);
    let d = w.m.dial_at.unwrap() - w.now.mono;
    assert!((20_000..=24_000).contains(&d), "{d}");
    // Before ready, too.
    w.advance(d);
    w.dial_fails(DialError::Bye {
        retry_after: Some(40),
    });
    let d = w.m.dial_at.unwrap() - w.now.mono;
    assert!((40_000..=48_000).contains(&d), "{d}");
}

#[test]
fn limit_and_overload_take_the_backoff() {
    for code in [close::LIMIT, close::OVERLOADED, close::TOO_SLOW] {
        let mut w = World::live();
        let n = w.dials.len();
        w.closed(code);
        assert_eq!(w.dials.len(), n, "{code}");
        assert!(w.m.dial_at.unwrap() > w.now.mono);
    }
}

#[test]
fn replaced_does_not_fight() {
    let mut w = World::live();
    w.advance(STABLE_MS + 1_000);
    w.closed(close::REPLACED);
    let d = w.m.dial_at.unwrap() - w.now.mono;
    assert!(d >= BACKOFF_MAX_MS, "{d}");
    // Even after a stable link, another 4409 waits the longest backoff.
    w.advance(d);
    w.link_up(HostState::Online);
    w.advance(STABLE_MS + 1_000);
    w.closed(close::REPLACED);
    assert!(w.m.dial_at.unwrap() - w.now.mono >= BACKOFF_MAX_MS);
    // The user coming back is a fresh start.
    w.input(Input::Lifecycle { foreground: false });
    let n = w.dials.len();
    w.foreground();
    assert_eq!(w.dials.len(), n + 1);
}

#[test]
fn enroll_is_shown_and_waited_on() {
    let mut w = World::paired();
    w.foreground();
    w.input(Input::Enroll {
        link: w.link,
        url: "https://relay.example/enroll".into(),
        code: "ABCD".into(),
    });
    let c = w.page_events("conn").pop().unwrap();
    assert_eq!(c["state"], "connecting");
    assert!(c["detail"].as_str().unwrap().contains("ABCD"), "{c}");
    // Waited on: no new dial meanwhile.
    w.advance(30_000);
    assert_eq!(w.dials.len(), 1);
    w.link_up(HostState::Online);
    assert!(w.page_events("conn").pop().unwrap().get("detail").is_none());
}

#[test]
fn silence_pings_then_gives_up() {
    let mut w = World::live();
    w.advance(PING_MS);
    // (A ping is an action the driver turns into a websocket ping.)
    assert_eq!(w.pings, 1);
    w.advance(PING_MS);
    assert_eq!(w.pings, 2);
    let n = w.dials.len();
    w.advance(SILENCE_MS - 2 * PING_MS - 1_000);
    assert_eq!(w.dials.len(), n, "not yet");
    w.advance(1_000);
    assert!(!w.m.live(), "given up after 45 s of silence");
    w.advance(BACKOFF_MAX_MS);
    assert_eq!(w.dials.len(), n + 1);
    // Anything heard keeps it.
    w.link_up(HostState::Online);
    for _ in 0..10 {
        w.advance(10_000);
        w.input(Input::Heard { link: w.link });
    }
    assert!(w.m.live());
}

#[test]
fn clock_jump_redials() {
    let mut w = World::live();
    w.advance(1_000);
    let n = w.dials.len();
    // The phone slept: the wall clock ran 60 s ahead of the monotonic one.
    w.now.mono += 1_000;
    w.now.wall += 61_000;
    w.input(Input::Tick);
    assert_eq!(w.dials.len(), n + 1, "redialled at once");
    // A monotonic jump does it too.
    w.link_up(HostState::Online);
    w.advance(1_000);
    w.now.mono += 20_000;
    w.now.wall += 20_000;
    w.input(Input::Tick);
    assert_eq!(w.dials.len(), n + 2);
}

#[test]
fn handshake_retries_on_the_same_link() {
    let mut w = World::paired();
    w.host.answer = false;
    w.foreground();
    w.link_up(HostState::Online);
    assert_eq!(w.host.handshakes, 1);
    let mut times = Vec::new();
    let t0 = w.now.mono;
    for _ in 0..6 {
        let n = w.host.handshakes;
        while w.host.handshakes == n {
            w.advance(100);
            w.input(Input::Heard { link: w.link });
        }
        times.push(w.now.mono - t0);
    }
    // 10 s for message 2, then 1 s, 2 s, 4 s … between attempts.
    let gaps: Vec<u64> = times.windows(2).map(|p| p[1] - p[0]).collect();
    assert_eq!(times[0], HANDSHAKE_MS + 1_000);
    assert_eq!(gaps, [12_000, 14_000, 18_000, 26_000, 40_000]);
    assert_eq!(w.dials.len(), 1, "never redials the relay for it");
    // `undeliverable` fails it at once.
    w.host.answer = true;
    w.relay_says(FromRelay::Undeliverable { link: None });
    let n = w.host.handshakes;
    w.advance_heard(HANDSHAKE_RETRY_MAX_MS);
    assert_eq!(w.host.handshakes, n + 1);
    assert!(w.m.live());
    // The cap is 30 s.
    assert_eq!(
        HANDSHAKE_RETRY_MIN_MS
            .saturating_mul(1 << 10)
            .min(HANDSHAKE_RETRY_MAX_MS),
        30_000
    );
}

#[test]
fn a_replayed_message_2_poisons_and_redials() {
    let mut w = World::paired();
    w.host.answer = false;
    w.foreground();
    w.link_up(HostState::Online);
    // A message 2 from another handshake.
    let other = StaticKey::from_private([5; 32]);
    let (_, m1) =
        Initiator::reconnect(&other, w.host.key.public(), &FakeHost::relay(), None).unwrap();
    let (_, m2) =
        Responder::reconnect(&w.host.key, other.public(), &FakeHost::relay(), &m1, None).unwrap();
    let n = w.closes.len();
    w.input(Input::Binary {
        link: w.link,
        bytes: ToDevice::Live(m2).encode(),
    });
    assert_eq!(w.closes.len(), n + 1);
    assert!(w.m.dial_at.is_some());
}

#[test]
fn garbage_binary_closes_and_unknown_text_is_skipped() {
    let mut w = World::live();
    w.input(Input::Text {
        link: w.link,
        text: r#"{"t":"something_new","x":1}"#.into(),
    });
    assert!(w.m.live());
    let n = w.closes.len();
    w.input(Input::Binary {
        link: w.link,
        bytes: vec![7, 7, 7],
    });
    assert_eq!(w.closes.len(), n + 1);
    // Garbage on the live lane is a decrypt failure: closed too.
    let mut w = World::live();
    let n = w.closes.len();
    w.input(Input::Binary {
        link: w.link,
        bytes: ToDevice::Live(vec![0; 40]).encode(),
    });
    assert_eq!(w.closes.len(), n + 1);
}

#[test]
fn only_asks_and_new_sessions_wait() {
    let mut w = World::paired();
    w.foreground();
    w.page_says(json!({"id": 1, "op": "session.rename", "args": {}}));
    assert_eq!(w.reply(1).unwrap()["error"]["code"], "disconnected");
    w.page_says(json!({"id": 2, "op": "snapshot", "args": {}}));
    assert_eq!(w.reply(2).unwrap()["error"]["code"], "disconnected");
    w.link_up(HostState::Offline);
    w.page_says(json!({"id": 3, "op": "session.rename", "args": {}}));
    assert_eq!(w.reply(3).unwrap()["error"]["code"], "host_offline");
    w.ask(4, "waits");
    w.page_says(json!({"id": 5, "op": "overlord.new_session", "args": {"text": "too"}}));
    assert!(w.reply(4).is_none() && w.reply(5).is_none());
    assert_eq!(w.saved().outbox.len(), 2);
}

#[test]
fn a_lookup_in_flight_fails_when_the_link_dies() {
    let mut w = World::live();
    w.page_says(json!({"id": 1, "op": "snapshot", "args": {}}));
    w.ask(2, "keeps waiting");
    w.closed(1006);
    assert_eq!(w.reply(1).unwrap()["error"]["code"], "disconnected");
    assert!(w.reply(2).is_none());
    assert!(w
        .saved()
        .outbox
        .get(w.requests()[1]["id"].as_str().unwrap())
        .is_some());
}

#[test]
fn a_change_is_resent_for_two_minutes_then_interrupted() {
    let mut w = World::live();
    w.page_says(json!({"id": 1, "op": "session.rename", "args": {}}));
    w.closed(1006);
    w.input(Input::Network { online: false });
    w.advance(outbox::RESEND_WINDOW_MS - 1_000);
    assert!(w.reply(1).is_none());
    w.advance(1_000);
    let r = w.reply(1).unwrap();
    assert_eq!(r["error"]["code"], "interrupted", "{r}");
    assert!(w.saved().outbox.is_empty());
}

#[test]
fn outbox_bounds_and_the_24_hour_stop() {
    let mut w = World::paired();
    w.foreground();
    w.link_up(HostState::Offline);
    for i in 0..outbox::MAX_ENTRIES as u64 {
        w.ask(i, "q");
    }
    w.ask(99, "one too many");
    assert_eq!(w.reply(99).unwrap()["error"]["code"], "outbox_full");
    // A day later, the page has reloaded: they end as failed, shown.
    w.page_says(json!({"app": "ready"}));
    w.now.wall += outbox::EXPIRY_MS;
    w.last_tick_reset();
    w.input(Input::Tick);
    let ob = w.page_events("outbox").pop().unwrap();
    let items = ob["items"].as_array().unwrap();
    assert_eq!(items.len(), outbox::MAX_ENTRIES);
    assert!(items.iter().all(|i| i["error"]["code"] == "expired"));
    // Not sent again.
    w.host.got.clear();
    w.relay_says(FromRelay::Host {
        state: HostState::Online,
    });
    assert!(w.requests().is_empty());
}

impl World {
    /// Forget the last tick, so a wall-clock step isn't taken for a jump.
    fn last_tick_reset(&mut self) {
        self.m.last_tick = None;
    }
}

#[test]
fn one_resend_pass_per_link() {
    let mut w = World::paired();
    w.foreground();
    w.link_up(HostState::Offline);
    w.ask(1, "a");
    w.ask(2, "b");
    w.relay_says(FromRelay::Host {
        state: HostState::Online,
    });
    assert_eq!(w.requests().len(), 2);
    // The session dies on the same link (undeliverable) and a retried
    // handshake makes a new one: no second pass.
    w.relay_says(FromRelay::Undeliverable { link: None });
    assert!(!w.m.live());
    w.host.session = None;
    w.advance_heard(HANDSHAKE_RETRY_MIN_MS);
    assert!(w.m.live());
    assert_eq!(w.requests().len(), 2);
    // The host went away and came back: its link to us is new, and what
    // it may have lost goes again (by id).
    w.relay_says(FromRelay::Host {
        state: HostState::Offline,
    });
    w.host.session = None;
    w.relay_says(FromRelay::Host {
        state: HostState::Online,
    });
    let reqs = w.requests();
    assert_eq!(reqs.len(), 4);
    assert_eq!(reqs[2]["id"], reqs[0]["id"]);
}

// ---- crashes between steps ----

/// The device's steps for one ask while the host is away, cut after each
/// action, then restarted from what was saved: the host never runs the
/// request twice, and once its entry was saved, the request is never
/// lost.
#[test]
fn a_crash_between_any_two_steps_neither_repeats_nor_loses() {
    // How many actions one ask makes.
    let steps = {
        let mut w = offline_with_mailbox();
        w.m.handle(
            w.now,
            Input::Page(json!({"id": 1, "op": "overlord.ask", "args": {"text": "x"}}).to_string()),
        );
        let mut n = 0;
        while w.m.poll().is_some() {
            n += 1;
        }
        n
    };
    assert!(steps >= 3, "{steps}");
    for cut in 0..=steps {
        let mut w = offline_with_mailbox();
        let before = w.host.mb_got.len();
        w.m.handle(
            w.now,
            Input::Page(json!({"id": 1, "op": "overlord.ask", "args": {"text": "x"}}).to_string()),
        );
        let mut taken = Vec::new();
        while let Some(a) = w.m.poll() {
            taken.push(a);
        }
        let saved_entry = taken[..cut]
            .iter()
            .any(|a| matches!(a, Action::Save { .. }));
        w.pending = taken[..cut].to_vec();
        w.apply();
        let sent = w.host.mb_got.len() - before;
        // Crash: restart from disk, the host comes online.
        let mut w = w.restart();
        w.foreground();
        w.link_up(HostState::Online);
        let live: Vec<Value> = w.requests();
        let asked: Vec<&Value> = w
            .host
            .mb_got
            .iter()
            .skip(before)
            .chain(live.iter())
            .filter(|v| v["op"] == "overlord.ask")
            .collect();
        let ids: std::collections::BTreeSet<String> = asked
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_string())
            .collect();
        if saved_entry {
            assert_eq!(ids.len(), 1, "cut {cut}: one request id, never lost");
        } else {
            assert!(
                ids.is_empty() && sent == 0,
                "cut {cut}: nothing left unsaved"
            );
        }
        // Sent by mailbox before the crash and live after: the same id
        // (the host's store runs it once).
        if sent > 0 {
            assert!(saved_entry, "cut {cut}: a frame left before its save");
        }
        // The host's mailbox never saw a counter twice (it would have
        // failed to open): the mailbox state was saved before sending.
        assert!(w.host.mailbox.as_ref().is_none_or(|m| !m.is_poisoned()));
    }
}

#[test]
fn mailbox_keying_saves_before_the_first_frame() {
    let mut w = World::paired();
    w.foreground();
    w.m.handle(
        w.now,
        Input::Dialed {
            link: w.link,
            result: Ok(Linked {
                ready: w.ready(HostState::Online),
                relay: FakeHost::relay(),
            }),
        },
    );
    // Drive the handshake and keying by hand, checking order: the save
    // with the mailbox comes before the frame that advanced it.
    let mut all = Vec::new();
    loop {
        let batch: Vec<Action> = std::iter::from_fn(|| w.m.poll()).collect();
        if batch.is_empty() {
            break;
        }
        all.extend(batch.clone());
        w.pending = batch;
        w.apply();
    }
    // The save that holds the new mailbox comes before the last frame
    // sent (`mailbox_ready`, the frame that advanced it).
    let _ = all;
    let saved_at = w.order.iter().position(|o| *o == "save+mb").unwrap();
    let last_frame = w.order.iter().rposition(|o| *o == "frame").unwrap();
    assert!(saved_at < last_frame, "{:?}", w.order);
    assert!(w.saved().mailbox.is_some());
}

// ---- the background check ----

#[test]
fn background_check_without_handshake() {
    let mut w = World::paired();
    w.m.rec.as_mut().unwrap().boot = Some("boot1".into());
    w.input(Input::Background { budget_ms: 20_000 });
    assert_eq!(w.dials.len(), 1);
    w.link_up(HostState::Online);
    assert_eq!(w.host.handshakes, 0, "nothing waiting: no handshake");
    assert_eq!(w.bg_done, [BackgroundOutcome::NothingNew]);
    assert_eq!(w.closes.last().unwrap().1, 1000, "closed cleanly");
    // No dial afterwards in the background.
    w.advance(60_000);
    assert_eq!(w.dials.len(), 1);
}

#[test]
fn background_check_refreshes_and_notifies() {
    let mut w = World::paired();
    w.m.rec.as_mut().unwrap().notified = vec!["s0".into()];
    w.input(Input::Background { budget_ms: 20_000 });
    w.link_up_with(Ready {
        signal: Some(1),
        ..w.ready(HostState::Online)
    });
    let rid = w.requests()[0]["id"].clone();
    w.host_says(json!({"id": rid, "ok": {"rev": "r9", "rows": [
        {"id": "s0", "shown_status": "waiting_input"},
        {"id": "s1", "shown_status": "waiting_input"},
        {"id": "s2", "shown_status": "waiting_input"},
        {"id": "s3", "shown_status": "working"},
    ]}}));
    assert_eq!(w.notified, [2], "two newly waiting");
    assert_eq!(
        w.bg_done,
        [BackgroundOutcome::Refreshed {
            waiting: 3,
            notified: 2
        }]
    );
    assert!(w
        .relay_got
        .iter()
        .any(|m| matches!(m, ToRelay::AckSignal { .. })));
    assert_eq!(w.saved().rev.as_deref(), Some("r9"));
    assert!(
        w.page.iter().all(|v| v["event"] != "hello"),
        "no page in the background"
    );
    // Again with nothing new: no notification.
    let mut w = w.restart();
    w.input(Input::Background { budget_ms: 20_000 });
    w.host.rev = "r10".into();
    w.link_up_with(Ready {
        signal: Some(1),
        ..w.ready(HostState::Online)
    });
    let rid = w.requests()[0]["id"].clone();
    w.host_says(json!({"id": rid, "ok": {"rev": "r10", "rows": [
        {"id": "s1", "shown_status": "waiting_input"},
    ]}}));
    assert!(w.notified.is_empty());
}

#[test]
fn background_check_takes_mailbox_answers_while_the_host_is_away() {
    let mut w = offline_with_mailbox();
    w.ask(1, "q");
    let rid = w.host.mb_got[0]["id"].clone();
    w.relay_says(FromRelay::Queued {
        seq: 1,
        device: None,
    });
    w.input(Input::Lifecycle { foreground: false });
    let mut w = w.restart();
    w.input(Input::Background { budget_ms: 20_000 });
    w.link_up_with(Ready {
        queued: Some(1),
        ..w.ready(HostState::Offline)
    });
    assert!(w.bg_done.is_empty(), "waits for the queued answer");
    let ans = w.host.mailbox_frame(&json!({"id": rid, "ok": {}}));
    w.input(Input::Binary {
        link: w.link,
        bytes: ToDevice::Mailbox {
            seq: 1,
            payload: ans,
        }
        .encode(),
    });
    assert_eq!(w.bg_done, [BackgroundOutcome::HostOffline]);
    assert!(w.saved().outbox.is_empty());
}

// ---- pairing, unpairing, the page ----

#[test]
fn pairing_by_qr() {
    let host = StaticKey::from_private([2; 32]);
    let qr = QrPayload::new(FakeHost::relay(), *host.public()).unwrap();
    let signers = Arc::new(SoftSigners::new());
    let m = Machine::new(signers.clone(), vec![], 3);
    let mut w = World::with(m, signers.clone(), host, [0; 32]);
    w.host.qr = Some(qr.clone());
    w.foreground();
    assert!(w.dials.is_empty());
    assert_eq!(w.last_conn(), "unpaired");
    w.input(Input::Scanned {
        qr: qr.encode(),
        name: "iPhone".into(),
    });
    let ev = w.page_events("pairing").pop().unwrap();
    assert_eq!(ev["state"], "scanned");
    assert_eq!(ev["relay"], ORIGIN);
    assert_eq!(ev["name"], "iPhone");
    w.page_says(json!({"app": "pair_continue", "name": "my phone"}));
    assert_eq!(w.dials.len(), 1);
    assert_eq!(
        w.page_events("pairing").pop().unwrap()["state"],
        "connecting"
    );
    w.link_up(HostState::Online);
    // The host answered message 1; the device's first frame names it.
    let first = w.host.got.pop().unwrap();
    assert_eq!(first["event"], "pair");
    assert_eq!(first["name"], "my phone");
    let label = store::label_for(&qr.host_static);
    let key = signers.public_key(label.clone()).unwrap();
    assert_eq!(first["key"], b64(&key));
    let code = format!("{:06}", w.host.code.unwrap());
    w.host_says(json!({"event": "pair", "state": "waiting", "code": code}));
    let ev = w.page_events("pairing").pop().unwrap();
    assert_eq!(ev["state"], "waiting");
    assert_eq!(ev["code"], code.as_str());
    // A reload while waiting shows it again.
    w.page.clear();
    w.page_says(json!({"app": "ready"}));
    assert_eq!(w.page_events("pairing")[0]["state"], "waiting");
    w.host_says(json!({"event": "pair", "state": "done", "host": "mac",
        "host_key": b64_fingerprint(&HOST_FP)}));
    assert_eq!(w.page_events("pairing").pop().unwrap()["state"], "done");
    let r = w.saved();
    assert_eq!(r.host.name, "mac");
    assert_eq!(r.device_name, "my phone");
    assert_eq!(r.label, label);
    // Then the first live session (a device link, at once).
    assert_eq!(w.dials.len(), 2);
    w.host.session = None;
    w.link_up(HostState::Online);
    assert!(w.m.live());
    assert!(w.saved().mailbox.is_some(), "the mailbox keyed");
}

#[test]
fn pairing_codes_that_differ_fail() {
    let host = StaticKey::from_private([2; 32]);
    let qr = QrPayload::new(FakeHost::relay(), *host.public()).unwrap();
    let signers = Arc::new(SoftSigners::new());
    let m = Machine::new(signers.clone(), vec![], 3);
    let mut w = World::with(m, signers.clone(), host, [0; 32]);
    w.host.qr = Some(qr.clone());
    w.input(Input::Scanned {
        qr: qr.encode(),
        name: "x".into(),
    });
    w.page_says(json!({"app": "pair_continue", "name": "x"}));
    w.link_up(HostState::Online);
    w.host_says(json!({"event": "pair", "state": "waiting", "code": "000000x"}));
    assert_eq!(w.page_events("pairing").pop().unwrap()["state"], "failed");
    assert!(signers
        .identity(&store::label_for(&qr.host_static))
        .is_none());
    assert!(!w.m.paired());
}

#[test]
fn a_rejected_pairing_deletes_its_key() {
    let host = StaticKey::from_private([2; 32]);
    let qr = QrPayload::new(FakeHost::relay(), *host.public()).unwrap();
    let signers = Arc::new(SoftSigners::new());
    let m = Machine::new(signers.clone(), vec![], 3);
    let mut w = World::with(m, signers.clone(), host, [0; 32]);
    w.input(Input::Scanned {
        qr: qr.encode(),
        name: "iPhone".into(),
    });
    w.page_says(json!({"app": "pair_continue", "name": ""}));
    let label = store::label_for(&qr.host_static);
    assert!(signers.identity(&label).is_some());
    w.dial_fails(DialError::Closed(close::NOT_ALLOWED));
    let ev = w.page_events("pairing").pop().unwrap();
    assert_eq!(ev["state"], "failed");
    assert!(ev["reason"].as_str().unwrap().contains("used already"));
    assert!(signers.identity(&label).is_none());
    assert!(w.disk.is_none());
}

#[test]
fn unpair_leaves_when_live_and_forgets_either_way() {
    let mut w = World::live();
    let label = w.m.record().unwrap().label.clone();
    w.page_says(json!({"app": "unpair"}));
    let leave = w.requests().pop().unwrap();
    assert_eq!(leave["op"], "device.leave");
    assert!(w.m.paired(), "waits for the answer");
    w.host_says(json!({"id": leave["id"], "error": {"code": "not_found", "message": "?"}}));
    assert!(!w.m.paired());
    assert!(w.disk.is_none());
    assert!(w.signers.identity(&label).is_none());
    let c = w.page_events("conn").pop().unwrap();
    assert_eq!(c["state"], "unpaired");
    // Without a session: forgotten here, with a word about the computer.
    let mut w = World::paired();
    w.page_says(json!({"app": "unpair"}));
    assert!(!w.m.paired());
    let c = w.page_events("conn").pop().unwrap();
    assert!(c["detail"].as_str().unwrap().contains("revoke"), "{c}");
}

#[test]
fn the_page_reloading_gets_everything_again() {
    let mut w = World::live();
    w.ask(1, "pending");
    w.page.clear();
    w.page_says(json!({"app": "ready"}));
    assert_eq!(w.page_events("conn")[0]["state"], "live");
    assert_eq!(w.page_events("outbox")[0]["items"][0]["text"], "pending");
    assert_eq!(w.page_events("hello").len(), 1);
    w.page_says(json!({"app": "status"}));
    let s = w.page_events("status").pop().unwrap();
    assert_eq!(s["keys"], "software");
    assert_eq!(s["device_id"], device_id(&w.host.device));
    assert_eq!(s["host"], "mac");
    assert_eq!(s["api"]["max"], API_MAX);
    // Answers to the old page's numbers aren't sent to the new page.
    let rid = w.requests()[0]["id"].clone();
    w.host_says(json!({"id": rid, "ok": {}}));
    assert!(w.reply(1).is_none());
    assert!(w.saved().outbox.is_empty(), "finished OK: gone");
}

#[test]
fn an_api_level_out_of_range_stops() {
    let mut w = World::paired();
    w.host.api = Some(2);
    w.foreground();
    w.link_up(HostState::Online);
    assert_eq!(w.last_conn(), "update_app");
    let mut w = World::paired();
    w.host.api = Some(0);
    w.foreground();
    w.link_up(HostState::Online);
    assert_eq!(w.last_conn(), "update_host");
    // Missing: taken as level 1 (a host from before `api`).
    let mut w = World::paired();
    w.host.api = None;
    w.foreground();
    w.link_up(HostState::Online);
    assert_eq!(w.last_conn(), "live");
}

#[test]
fn old_mailbox_frames_replayed_discard_the_mailbox_and_no_answer_counts_twice() {
    let mut w = offline_with_mailbox();
    w.ask(1, "q");
    let rid = w.host.mb_got[0]["id"].clone();
    w.relay_says(FromRelay::Queued {
        seq: 1,
        device: None,
    });
    let ans = w
        .host
        .mailbox_frame(&json!({"id": rid, "ok": {"first": true}}));
    w.input(Input::Binary {
        link: w.link,
        bytes: ToDevice::Mailbox {
            seq: 1,
            payload: ans.clone(),
        }
        .encode(),
    });
    assert_eq!(w.reply(1).unwrap()["ok"]["first"], true);
    // The same old frame replayed under another (boot, seq): the counter
    // went back, so the mailbox is discarded; nothing reaches the page.
    w.input(Input::Binary {
        link: w.link,
        bytes: ToDevice::Mailbox {
            seq: 9,
            payload: ans,
        }
        .encode(),
    });
    assert_eq!(w.page.iter().filter(|v| v["id"] == 1).count(), 1);
    assert!(w.m.mailbox.is_none() && w.saved().mailbox.is_none());
    // Keyed again on the next live session.
    w.host.session = None;
    w.relay_says(FromRelay::Host {
        state: HostState::Online,
    });
    assert!(w.saved().mailbox.is_some());
}

#[test]
fn a_relay_that_never_answers_queued_is_given_up() {
    let mut w = offline_with_mailbox();
    w.ask(1, "q");
    let n = w.closes.len();
    w.advance(MAILBOX_ANSWER_MS);
    assert_eq!(w.closes.len(), n + 1);
    // Sent again on the next link (its fate was unknown).
    w.advance(BACKOFF_MAX_MS);
    w.link_up(HostState::Offline);
    assert_eq!(w.host.mb_got.len(), 2);
}

#[test]
fn a_relay_changing_boot_every_time_gets_one_resend_per_link() {
    let mut w = offline_with_mailbox();
    w.ask(1, "q");
    w.relay_says(FromRelay::Queued {
        seq: 1,
        device: None,
    });
    for i in 0..5 {
        w.closed(1006);
        let d = w.m.dial_at.unwrap() - w.now.mono;
        w.advance(d);
        w.boot = format!("b{i}");
        w.link_up(HostState::Offline);
        w.relay_says(FromRelay::Queued {
            seq: 1,
            device: None,
        });
        w.relay_says(FromRelay::Signal);
        w.relay_says(FromRelay::Signal);
    }
    // One mailbox frame per link, and the links themselves backed off.
    assert_eq!(w.host.mb_got.len(), 6);
    assert_eq!(w.dials.len(), 6);
}

// ---- the host's mailbox in its hello ----

#[test]
fn a_hello_naming_another_mailbox_keys_a_new_one() {
    // The same id: nothing changes.
    let mut w = World::live();
    assert_eq!(w.host.keyings, 1);
    let old = w.m.mailbox.as_ref().unwrap().id();
    w.reconnect();
    assert_eq!(w.host.keyings, 1);
    assert_eq!(w.m.mailbox.as_ref().unwrap().id(), old);
    assert!(
        w.page_events("hello")
            .iter()
            .all(|h| h.get("mailbox").is_none()),
        "the page's hello is a local page's"
    );
    // Another id: ours is discarded and a new one keyed, saved, and the
    // host's.
    w.host.hello_mailbox = HelloMailbox::Fixed(json!(b64(&[7; 8])));
    w.reconnect();
    assert_eq!(w.host.keyings, 2);
    let new = w.m.mailbox.as_ref().unwrap().id();
    assert_ne!(new, old);
    assert_eq!(w.host.mailbox.as_ref().unwrap().id(), new);
    let saved = MailboxSession::import(&unb64(&w.saved().mailbox.unwrap()).unwrap()).unwrap();
    assert_eq!(saved.id(), new);
    // None at the host (null): the same.
    w.host.hello_mailbox = HelloMailbox::Current;
    w.host.mailbox = None;
    w.reconnect();
    assert_eq!(w.host.keyings, 3);
    assert_ne!(w.m.mailbox.as_ref().unwrap().id(), new);
}

#[test]
fn an_older_host_without_the_field_keeps_todays_behaviour() {
    let mut w = World::live();
    let old = w.m.mailbox.as_ref().unwrap().id();
    // The host lost it but doesn't say: the device can't know.
    w.host.hello_mailbox = HelloMailbox::Absent;
    w.host.mailbox = None;
    w.reconnect();
    assert_eq!(w.host.keyings, 1);
    assert_eq!(w.m.mailbox.as_ref().unwrap().id(), old);
}

#[test]
fn a_request_sent_under_a_discarded_mailbox_is_neither_lost_nor_answered_twice() {
    let mut w = offline_with_mailbox();
    w.ask(1, "q");
    let rid = w.host.mb_got[0]["id"].clone();
    w.relay_says(FromRelay::Queued {
        seq: 1,
        device: None,
    });
    // The host loses the mailbox before it opens the frame (its state was
    // lost): the frame is dropped there.
    w.host.mailbox = None;
    w.host.session = None;
    w.relay_says(FromRelay::Host {
        state: HostState::Online,
    });
    assert!(w.m.live());
    assert_eq!(w.host.keyings, 2, "keyed anew");
    let live: Vec<Value> = w.requests();
    assert_eq!(live.len(), 1, "resent live by id");
    assert_eq!(live[0]["id"], rid);
    // The host goes again before answering: it goes by the new mailbox.
    w.relay_says(FromRelay::Host {
        state: HostState::Offline,
    });
    assert_eq!(w.host.mb_got.len(), 2);
    assert_eq!(w.host.mb_got[1]["id"], rid);
    w.relay_says(FromRelay::Queued {
        seq: 2,
        device: None,
    });
    // Its answer by mailbox, then the same again (from the live resend):
    // the page hears once.
    for seq in [1, 2] {
        let ans = w.host.mailbox_frame(&json!({"id": rid, "ok": {"n": seq}}));
        w.input(Input::Binary {
            link: w.link,
            bytes: ToDevice::Mailbox { seq, payload: ans }.encode(),
        });
    }
    assert_eq!(w.page.iter().filter(|v| v["id"] == 1).count(), 1);
    assert_eq!(w.reply(1).unwrap()["ok"]["n"], 1);
    assert!(w.saved().outbox.is_empty());
}

#[test]
fn a_frame_queued_or_refused_under_a_discarded_mailbox_goes_again() {
    // Refused `full` under the old mailbox, then the host says it has
    // another: the refused bytes are not offered again; the request is
    // sealed afresh under the new mailbox.
    let mut w = offline_with_mailbox();
    w.ask(1, "q");
    let rid = w.host.mb_got[0]["id"].clone();
    w.relay_says(FromRelay::Refused {
        code: "full".into(),
        device: None,
    });
    w.host.mailbox = None;
    w.host.session = None;
    w.relay_says(FromRelay::Host {
        state: HostState::Online,
    });
    assert_eq!(w.host.keyings, 2);
    w.relay_says(FromRelay::Host {
        state: HostState::Offline,
    });
    let new = w.m.mailbox.as_ref().unwrap().id();
    let last = w.mb_raw.last().unwrap();
    assert_eq!(last[..8], new, "sealed under the new mailbox");
    assert_eq!(w.host.mb_got.last().unwrap()["id"], rid);
    // A frame in flight when the mailbox went: its `queued` doesn't mark
    // the request as queued (the host drops the frame), so it goes again.
    let mut w = offline_with_mailbox();
    w.ask(1, "q");
    w.host.mailbox = None;
    w.m.discard_mailbox();
    w.relay_says(FromRelay::Queued {
        seq: 1,
        device: None,
    });
    assert_eq!(w.saved().outbox.entries[0].queued_boot, None);
}
