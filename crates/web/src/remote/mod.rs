//! Remote access: `rlord-web` dials out to one pinned relay and serves
//! paired devices through it, end to end encrypted
//! (`docs/working/e2e-protocol.md`).
//!
//! **Off unless asked.** The plugin opens a relay connection only when
//! `[remote] enabled`, a pin is set (and matches the keys' `pin.json`), and
//! there is at least one paired device or an open pairing. The listener
//! stays the loopback one in `main.rs`: nothing remote is ever accepted.
//!
//! One task (the actor) owns all the state: the keys, the devices, the
//! relay connection, every link and its session, the pairing. Its inputs
//! are the daemon's user commands (`relay.*`, forwarded on the subscribe
//! stream), the daemon's changes, the relay's messages, finished daemon
//! calls and a one-second tick. Calls to the daemon run in their own tasks
//! (an `overlord.ask` may take a while); their answers come back to the
//! actor, which alone encrypts, so each session's frames stay in order.

mod audit;
mod devices;
mod keys;
pub mod link;
mod mailbox;
mod pairing;
mod session;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use rlord_core::config::RemoteConfig;
use rlord_core::plugin;
use rlord_relay_proto::address::{b64_fingerprint, fingerprint_from_b64};
use rlord_relay_proto::frame::{FromHost, ToHost};
use rlord_relay_proto::msg::{Enroll, LinkKind};
use rlord_relay_proto::{FromRelay, Role, ToRelay, B64};

use crate::daemon::Daemon;
use audit::Audit;
use devices::Devices;
use keys::Keys;
use pairing::Pairing;
use session::Link;

/// Unix seconds.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reconnect backoff: 1 s doubling to 60 s, never given up while enabled.
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A relay that says nothing (not even a ping) for this long is gone.
const SILENCE: Duration = Duration::from_secs(70);
/// At most one "something for you" per device per this long.
const SIGNAL_EVERY: Duration = Duration::from_secs(30);
/// Unused devices are looked for at start and once a day.
const EXPIRY_EVERY: Duration = Duration::from_secs(86_400);

/// What reaches the actor.
pub enum Input {
    /// A `relay.*` command forwarded by the daemon.
    Command {
        id: String,
        req: Value,
    },
    /// The daemon's state moved.
    Changed(String),
    /// A connect attempt finished.
    Connected {
        attempt: u64,
        result: Box<Result<(link::Ws, rlord_relay_proto::msg::Ready), link::Failure>>,
    },
    /// The relay is asking the user to approve this host.
    Enroll(link::EnrollPrompt),
    Relay {
        attempt: u64,
        msg: RelayIn,
    },
    /// A daemon call made for a live session finished.
    OpDone {
        link: u32,
        epoch: u64,
        answer: Value,
    },
    /// A daemon call made for a mailbox request finished.
    MailboxDone {
        device: String,
        answer: Value,
    },
    Tick,
}

pub enum RelayIn {
    /// A ping or pong: the relay is still there.
    Alive,
    Text(FromRelay),
    Binary(ToHost),
    Closed(link::Failure),
}

/// The handle `main.rs` holds.
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Input>,
}

impl Handle {
    pub fn command(&self, id: String, req: Value) {
        let _ = self.tx.send(Input::Command { id, req });
    }

    pub fn changed(&self, rev: String) {
        let _ = self.tx.send(Input::Changed(rev));
    }
}

/// The relay connection's state.
enum Conn {
    /// Not wanted, or stopped for good (`stopped` says why).
    Off,
    /// Wanted; the next attempt at `at`.
    Waiting {
        at: Instant,
    },
    Connecting {
        attempt: u64,
    },
    Up {
        attempt: u64,
        tx: mpsc::Sender<Message>,
        last_rx: Instant,
    },
}

pub(crate) struct Actor {
    pub(crate) d: Daemon,
    pub(crate) dir: PathBuf,
    pub(crate) cfg: RemoteConfig,
    pub(crate) keys: Option<Keys>,
    pub(crate) devices: Devices,
    pub(crate) audit: Audit,
    pub(crate) rev: String,
    /// The relay's `boot` from the current connection's `ready`.
    pub(crate) boot: String,
    tx: mpsc::UnboundedSender<Input>,
    conn: Conn,
    attempts: u64,
    backoff: Duration,
    /// A relay-imposed earliest next attempt (`bye`'s `retry_after`).
    not_before: Option<Instant>,
    /// Why the connection is stopped for good, until a command or config
    /// change (wrong relay, refused).
    stopped: Option<String>,
    /// The last error, for `rlord relay status`.
    pub(crate) error: Option<String>,
    /// Request numbers on the relay link.
    n: u64,
    pub(crate) links: HashMap<u32, Link>,
    pub(crate) pairing: Option<Pairing>,
    pub(crate) mailboxes: mailbox::Mailboxes,
    last_signal: HashMap<String, Instant>,
    last_expiry: Option<Instant>,
    state_tx: watch::Sender<Value>,
}

/// Start the remote-access task. `rev` is the daemon's state now.
pub fn start(d: Daemon, cfg: RemoteConfig, rev: String) -> Handle {
    let dir = plugin::remote_dir();
    let (tx, rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = watch::channel(Value::Null);
    let keys = Keys::load(&dir).unwrap_or_else(|e| {
        eprintln!("rlord-web: remote keys: {e:#}");
        None
    });
    let devices = Devices::load(&dir).unwrap_or_else(|e| {
        eprintln!("rlord-web: remote devices: {e:#}");
        Devices::default()
    });
    let actor = Actor {
        d: d.clone(),
        mailboxes: mailbox::Mailboxes::new(dir.clone()),
        dir,
        cfg,
        keys,
        devices,
        audit: Audit::new(plugin::remote_log_path()),
        rev,
        boot: String::new(),
        tx: tx.clone(),
        conn: Conn::Off,
        attempts: 0,
        backoff: Duration::from_secs(1),
        not_before: None,
        stopped: None,
        error: None,
        n: 0,
        links: HashMap::new(),
        pairing: None,
        last_signal: HashMap::new(),
        last_expiry: None,
        state_tx,
    };
    tokio::spawn(report_states(d, state_rx));
    let ticker = tx.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(1));
        loop {
            t.tick().await;
            if ticker.send(Input::Tick).is_err() {
                break;
            }
        }
    });
    tokio::spawn(actor.run(rx));
    Handle { tx }
}

/// Sends each new state to the daemon (`web.remote_state`), in order.
async fn report_states(d: Daemon, mut rx: watch::Receiver<Value>) {
    while rx.changed().await.is_ok() {
        let state = rx.borrow_and_update().clone();
        if let Err(e) = d.remote_state(state).await {
            eprintln!("rlord-web: reporting remote state: {e}");
        }
    }
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Input>) {
        self.expire_devices();
        self.report();
        while let Some(input) = rx.recv().await {
            self.handle(input);
            self.report();
        }
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Command { id, req } => {
                let r = self.command(&req);
                let d = self.d.clone();
                tokio::spawn(async move {
                    if let Err(e) = d.remote_reply(&id, r).await {
                        eprintln!("rlord-web: replying to the daemon: {e}");
                    }
                });
            }
            Input::Changed(rev) => {
                self.rev = rev;
                self.changed();
            }
            Input::Connected { attempt, result } => self.connected(attempt, *result),
            Input::Enroll(p) => {
                self.error = Some(format!(
                    "the relay asks you to approve this host: open {} and enter {}",
                    p.url, p.code
                ));
            }
            Input::Relay { attempt, msg } => {
                if !matches!(self.conn, Conn::Up { attempt: a, .. } if a == attempt) {
                    return;
                }
                if let Conn::Up { last_rx, .. } = &mut self.conn {
                    *last_rx = Instant::now();
                }
                match msg {
                    RelayIn::Alive => {}
                    RelayIn::Text(m) => self.relay_text(m),
                    RelayIn::Binary(f) => self.relay_binary(f),
                    RelayIn::Closed(f) => self.disconnected(f),
                }
            }
            Input::OpDone {
                link,
                epoch,
                answer,
            } => self.op_done(link, epoch, answer),
            Input::MailboxDone { device, answer } => self.mailbox_done(&device, answer),
            Input::Tick => self.tick(),
        }
    }

    /// Is a relay connection wanted now?
    fn wanted(&self) -> bool {
        self.cfg.enabled
            && self.pin_ok().is_ok()
            && (!self.devices.list.is_empty() || self.pairing.is_some())
    }

    /// The config's relay is the one the keys were made for.
    pub(crate) fn pin_ok(&self) -> Result<&Keys, String> {
        let Some(k) = &self.keys else {
            return Err("not set up: run `rlord relay setup <relay address>`".into());
        };
        let fp = fingerprint_from_b64(&self.cfg.relay_key).ok();
        if self.cfg.relay != k.pin.origin || fp != Some(k.pin.fingerprint) {
            return Err(format!(
                "the config names another relay than this host's keys were made for ({}); \
                 moving is `rlord relay setup --reset <address>` (every device pairs again)",
                k.pin.origin
            ));
        }
        Ok(k)
    }

    pub(crate) fn connected_now(&self) -> bool {
        matches!(self.conn, Conn::Up { .. })
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self
            .last_expiry
            .is_none_or(|t| now.duration_since(t) >= EXPIRY_EVERY)
        {
            self.expire_devices();
        }
        self.pairing_tick();
        let wanted = self.wanted();
        match &self.conn {
            Conn::Up { last_rx, .. } if !wanted || now.duration_since(*last_rx) > SILENCE => {
                if wanted {
                    self.disconnected(link::Failure::Retry {
                        after: None,
                        why: "the relay went silent".into(),
                    });
                } else {
                    self.close();
                }
            }
            Conn::Off if wanted && self.stopped.is_none() => self.connect(),
            Conn::Waiting { at } if wanted && *at <= now => self.connect(),
            Conn::Waiting { .. } if !wanted => self.conn = Conn::Off,
            _ => {}
        }
    }

    fn connect(&mut self) {
        if let Some(t) = self.not_before {
            if Instant::now() < t {
                self.conn = Conn::Waiting { at: t };
                return;
            }
            self.not_before = None;
        }
        let Ok(keys) = self.pin_ok() else {
            return;
        };
        let pin = keys.pin.clone();
        let id = keys.relay.clone();
        self.attempts += 1;
        let attempt = self.attempts;
        let ca = self.cfg.ca_file.clone();
        let token = keys::token(&self.dir);
        let tx = self.tx.clone();
        self.conn = Conn::Connecting { attempt };
        tokio::spawn(async move {
            let result = async {
                let mut ws = link::open(&pin, &ca).await?;
                let nonce = keys::random32().map_err(|e| link::Failure::Retry {
                    after: None,
                    why: e.to_string(),
                })?;
                let enroll = token.map(|token| Enroll::Token { token });
                let etx = tx.clone();
                let mut on_enroll = move |p| {
                    let _ = etx.send(Input::Enroll(p));
                };
                let ready = link::authenticate(
                    &mut ws,
                    &pin,
                    &id,
                    nonce,
                    Role::Host,
                    &[],
                    enroll,
                    &mut on_enroll,
                )
                .await?;
                Ok((ws, ready))
            }
            .await;
            let _ = tx.send(Input::Connected {
                attempt,
                result: Box::new(result),
            });
        });
    }

    fn connected(
        &mut self,
        attempt: u64,
        result: Result<(link::Ws, rlord_relay_proto::msg::Ready), link::Failure>,
    ) {
        if !matches!(self.conn, Conn::Connecting { attempt: a } if a == attempt) {
            return;
        }
        let (ws, ready) = match result {
            Ok(x) => x,
            Err(f) => return self.disconnected(f),
        };
        keys::forget_token(&self.dir);
        self.boot = ready.boot;
        self.error = None;
        self.backoff = Duration::from_secs(1);
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);
        tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                let close = matches!(m, Message::Close(_));
                if sink.send(m).await.is_err() || close {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let closed = loop {
                let msg = match stream.next().await {
                    Some(Ok(Message::Text(t))) => match FromRelay::parse(&t) {
                        Ok(m) => RelayIn::Text(m),
                        Err(e) => {
                            eprintln!("rlord-web: relay sent {e}");
                            continue;
                        }
                    },
                    Some(Ok(Message::Binary(b))) => match ToHost::decode(&b) {
                        Ok(f) => RelayIn::Binary(f),
                        Err(e) => {
                            eprintln!("rlord-web: relay sent a bad frame: {e}");
                            continue;
                        }
                    },
                    Some(Ok(Message::Close(c))) => {
                        break match c {
                            Some(c) => link::closed(u16::from(c.code), &c.reason),
                            None => link::Failure::Retry {
                                after: None,
                                why: "the relay closed the connection".into(),
                            },
                        }
                    }
                    Some(Ok(_)) => RelayIn::Alive,
                    Some(Err(e)) => {
                        break link::Failure::Retry {
                            after: None,
                            why: format!("relay: {e}"),
                        }
                    }
                    None => {
                        break link::Failure::Retry {
                            after: None,
                            why: "the relay closed the connection".into(),
                        }
                    }
                };
                if tx.send(Input::Relay { attempt, msg }).is_err() {
                    return;
                }
            };
            let _ = tx.send(Input::Relay {
                attempt,
                msg: RelayIn::Closed(closed),
            });
        });
        self.conn = Conn::Up {
            attempt,
            tx: out_tx,
            last_rx: Instant::now(),
        };
        let origin = self
            .keys
            .as_ref()
            .map(|k| k.pin.origin.clone())
            .unwrap_or_default();
        eprintln!("rlord-web: connected to the relay {origin}");
        self.send_allow();
        self.pairing_connected();
    }

    /// The connection ended (or never came up).
    fn disconnected(&mut self, f: link::Failure) {
        self.drop_links();
        let now = Instant::now();
        match &f {
            link::Failure::WrongRelay(_) | link::Failure::Final { .. } => {
                self.stopped = Some(f.to_string());
                self.conn = Conn::Off;
            }
            link::Failure::Retry { after, .. } => {
                if let Some(a) = after {
                    // Honour the relay's wait, with up to 20% jitter.
                    let jitter =
                        a.mul_f64(f64::from(keys::random32().map(|b| b[0]).unwrap_or(0)) / 1275.0);
                    self.not_before = Some(now + *a + jitter);
                }
                let at = (now + self.backoff).max(self.not_before.unwrap_or(now));
                self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
                self.conn = Conn::Waiting { at };
            }
        }
        eprintln!("rlord-web: relay: {f}");
        self.error = Some(f.to_string());
    }

    /// Close the connection on purpose (not wanted any more).
    fn close(&mut self) {
        if let Conn::Up { tx, .. } = &self.conn {
            let _ = tx.try_send(link::close_frame());
        }
        self.drop_links();
        self.conn = Conn::Off;
    }

    fn drop_links(&mut self) {
        for (_, l) in self.links.drain() {
            if l.is_live() {
                self.audit
                    .log(&l.device_id().unwrap_or_default(), "link closed");
            }
        }
        self.pairing_link_gone();
    }

    /// Send a control message to the relay.
    pub(crate) fn send_relay(&mut self, m: impl FnOnce(u64) -> ToRelay) {
        self.n += 1;
        let msg = m(self.n);
        if let Conn::Up { tx, .. } = &self.conn {
            if tx.try_send(Message::Text(msg.to_json().into())).is_err() {
                eprintln!("rlord-web: the relay link is backed up; dropping the connection");
            }
        }
    }

    /// Send a binary frame to the relay.
    pub(crate) fn send_frame(&mut self, f: FromHost) {
        if let Conn::Up { tx, .. } = &self.conn {
            if tx.try_send(Message::Binary(f.encode().into())).is_err() {
                eprintln!("rlord-web: the relay link is backed up");
            }
        }
    }

    /// The relay's allow-list: every paired device.
    pub(crate) fn send_allow(&mut self) {
        let allow = self.devices.allow();
        self.send_relay(|n| ToRelay::Devices { n, allow });
    }

    fn relay_text(&mut self, m: FromRelay) {
        match m {
            FromRelay::Link {
                id,
                device,
                kind,
                rendezvous,
            } => {
                let Some(fp) = device.as_array::<32>() else {
                    return self.send_relay(|n| ToRelay::Drop { n, link: id });
                };
                match kind {
                    LinkKind::Device => self.device_link(id, fp),
                    LinkKind::Pair => self.pair_link(id, fp, rendezvous),
                }
            }
            FromRelay::Unlink { id } => self.unlink(id),
            FromRelay::Err { n, code } => eprintln!("rlord-web: relay refused request {n}: {code}"),
            FromRelay::Bye { code, retry_after } => {
                if let Some(s) = retry_after {
                    self.not_before = Some(Instant::now() + Duration::from_secs(s));
                }
                eprintln!("rlord-web: relay says bye ({code})");
            }
            _ => {}
        }
    }

    fn relay_binary(&mut self, f: ToHost) {
        match f {
            ToHost::Live { link, payload } => self.live_frame(link, &payload),
            ToHost::Mailbox {
                device,
                seq,
                payload,
            } => self.mailbox_frame(device, seq, &payload),
        }
    }

    fn unlink(&mut self, id: u32) {
        if let Some(l) = self.links.remove(&id) {
            if let Some(dev) = l.device_id() {
                if l.is_live() {
                    self.audit.log(&dev, "link closed");
                }
            }
            if l.is_pair() {
                self.pair_unlinked(id, l);
            }
        }
    }

    /// Close one link at the relay.
    pub(crate) fn drop_link(&mut self, id: u32, why: &str) {
        if let Some(l) = self.links.remove(&id) {
            eprintln!("rlord-web: dropping link {id}: {why}");
            if l.is_pair() {
                self.pair_unlinked(id, l);
            }
        }
        self.send_relay(|n| ToRelay::Drop { n, link: id });
    }

    /// The daemon's state moved: tell live sessions; signal devices with
    /// none.
    fn changed(&mut self) {
        let rev = self.rev.clone();
        let live: Vec<u32> = self
            .links
            .iter()
            .filter(|(_, l)| l.is_live())
            .map(|(id, _)| *id)
            .collect();
        for id in live {
            self.send_control(id, &json!({"event": "changed", "rev": rev}));
        }
        if !self.connected_now() {
            return;
        }
        let live_devices: Vec<String> = self
            .links
            .values()
            .filter_map(|l| l.live_device())
            .collect();
        let now = Instant::now();
        let to_signal: Vec<(String, B64)> = self
            .devices
            .list
            .iter()
            .filter(|d| !live_devices.contains(&d.id))
            .filter(|d| {
                self.last_signal
                    .get(&d.id)
                    .is_none_or(|t| now.duration_since(*t) >= SIGNAL_EVERY)
            })
            .map(|d| (d.id.clone(), d.relay_fp.clone()))
            .collect();
        for (id, fp) in to_signal {
            self.last_signal.insert(id, now);
            self.send_relay(|n| ToRelay::Signal { n, device: fp });
        }
    }

    /// Revoke devices unused for `device_expiry_days`.
    fn expire_devices(&mut self) {
        self.last_expiry = Some(Instant::now());
        let now = now();
        let expired: Vec<String> = self
            .devices
            .list
            .iter()
            .filter(|d| d.expired(now, self.cfg.device_expiry_days))
            .map(|d| d.id.clone())
            .collect();
        for id in expired {
            let _ = self.revoke(&id, "expired");
        }
    }

    /// Revoke a device: its record and mailbox go, its live link closes,
    /// and the relay gets the shorter allow-list.
    pub(crate) fn revoke(&mut self, id: &str, why: &str) -> Result<(), String> {
        let Some(d) = self.devices.remove(id).map_err(|e| e.to_string())? else {
            return Err(format!("no device {id}"));
        };
        self.mailboxes.forget(id);
        self.audit.log(id, &format!("revoked ({why})"));
        let links: Vec<u32> = self
            .links
            .iter()
            .filter(|(_, l)| l.fp() == d.relay_fp().unwrap_or_default())
            .map(|(k, _)| *k)
            .collect();
        for l in links {
            self.drop_link(l, "revoked");
        }
        self.send_allow();
        Ok(())
    }

    /// `relay.*` commands from the daemon (user-only).
    fn command(&mut self, req: &Value) -> Result<Value, String> {
        let cmd = req["cmd"].as_str().unwrap_or("");
        match cmd {
            "setup" => self.setup(req),
            "pair" => self.pair(),
            "pair_cancel" => {
                self.cancel_pairing("cancelled");
                Ok(json!({}))
            }
            "confirm" => self.confirm(),
            "reject" => {
                self.reject_pending("rejected by the user")?;
                Ok(json!({}))
            }
            "revoke" => {
                let id = req["device"].as_str().ok_or("which device?")?;
                self.revoke(id, "by the user")?;
                Ok(json!({}))
            }
            "disable" => {
                self.cfg.enabled = false;
                self.cancel_pairing("remote access turned off");
                self.close();
                Ok(json!({}))
            }
            _ => Err(format!("unknown remote command {cmd:?}")),
        }
    }

    fn setup(&mut self, req: &Value) -> Result<Value, String> {
        let address = req["address"].as_str().ok_or("no relay address")?;
        let pin = rlord_relay_proto::RelayAddress::parse(address).map_err(|e| e.to_string())?;
        if req["reset"].as_bool() == Some(true) {
            self.close();
            self.cancel_pairing("the relay was reset");
            keys::reset(&self.dir).map_err(|e| format!("{e:#}"))?;
            self.keys = None;
            self.devices = Devices::load(&self.dir).map_err(|e| format!("{e:#}"))?;
            self.mailboxes = mailbox::Mailboxes::new(self.dir.clone());
            self.audit
                .log("", &format!("reset; set up for {}", pin.origin));
        }
        match &self.keys {
            Some(k) if k.pin != pin => {
                return Err(format!(
                    "this host is set up for {} already; moving to another relay is \
                     `rlord relay setup --reset <address>` (every device pairs again)",
                    k.pin
                ))
            }
            Some(_) => {}
            None => {
                self.keys = Some(Keys::create(&self.dir, &pin).map_err(|e| format!("{e:#}"))?);
                self.audit.log("", &format!("set up for {}", pin.origin));
            }
        }
        if let Some(t) = req["token"].as_str().filter(|t| !t.is_empty()) {
            keys::save_token(&self.dir, t).map_err(|e| format!("{e:#}"))?;
        }
        self.cfg.enabled = true;
        self.cfg.relay = pin.origin.clone();
        self.cfg.relay_key = b64_fingerprint(&pin.fingerprint);
        self.stopped = None;
        self.error = None;
        if matches!(self.conn, Conn::Waiting { .. }) {
            self.conn = Conn::Off;
        }
        Ok(json!({"origin": pin.origin, "fingerprint": self.cfg.relay_key}))
    }

    /// The state the daemon mirrors (`web.remote_state`).
    fn state(&self) -> Value {
        let live: Vec<String> = self
            .links
            .values()
            .filter_map(|l| l.live_device())
            .collect();
        let error = self
            .stopped
            .clone()
            .or_else(|| self.pin_ok().err().filter(|_| self.cfg.enabled))
            .or_else(|| self.error.clone());
        json!({
            "relay": self.keys.as_ref().map(|k| k.pin.origin.clone()),
            "enabled": self.cfg.enabled,
            "connected": self.connected_now(),
            "error": error,
            "devices": self.devices.list.iter().map(|d| json!({
                "id": d.id,
                "name": d.name,
                "paired_at": d.paired_at,
                "last_seen": d.last_seen,
                "linked": live.contains(&d.id),
            })).collect::<Vec<_>>(),
            "pairing": self.pairing.as_ref().map(|p| json!({"expires": p.expires})),
            "pending": self.pairing.as_ref().and_then(|p| p.pending.as_ref()).map(|w| json!({
                "name": w.name,
                "code": format!("{:06}", w.code),
                "expires": w.deadline,
            })),
        })
    }

    fn report(&self) {
        let s = self.state();
        self.state_tx.send_if_modified(|old| {
            if *old == s {
                false
            } else {
                *old = s;
                true
            }
        });
    }
}
