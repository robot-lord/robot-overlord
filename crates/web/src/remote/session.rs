//! A device's link and its live session: `KKpsk2`, then the control
//! stream carried to the daemon as `web.call {op, args, via: "remote",
//! device}`, `changed` events, and mailbox keying
//! (`docs/working/e2e-protocol.md`, "A live session").

use std::time::Instant;

use base64::Engine as _;
use serde_json::{json, Value};

use rlord_e2e::stream::{self, Reassembler, CONTROL};
use rlord_e2e::{Kind, MailboxSession, Responder, Session};

use super::requests::{Next, Waiter};
use super::Actor;

/// A device may send this many ops per second, with this burst.
const OPS_PER_SEC: f64 = 20.0;
const OPS_BURST: f64 = 40.0;

/// What a link is doing.
pub(crate) enum Phase {
    /// Waiting for the device's message 1.
    AwaitMsg1,
    /// Pairing: message 2 sent; the device's first frame is due. The
    /// handshake's short code and the device's static key wait with it.
    PairFirst {
        session: Session,
        code: u32,
        device_static: [u8; 32],
    },
    /// Pairing: waiting for the user to confirm.
    PairWaiting(Session),
    /// Reconnect: message 2 sent; the device's hello is due.
    AwaitHello {
        session: Session,
        device: String,
    },
    Live(Box<Live>),
}

pub(crate) struct Live {
    pub session: Session,
    pub reasm: Reassembler,
    pub device: String,
    tokens: f64,
    refilled: Instant,
    /// A mailbox keyed on this session, waiting for the device's first
    /// mailbox frame.
    pub mailbox_pending: Option<MailboxSession>,
}

impl Live {
    fn new(session: Session, device: String) -> Live {
        Live {
            session,
            reasm: Reassembler::new(),
            device,
            tokens: OPS_BURST,
            refilled: Instant::now(),
            mailbox_pending: None,
        }
    }

    /// One op's worth of the rate limit.
    fn take(&mut self) -> bool {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.refilled).as_secs_f64() * OPS_PER_SEC)
            .min(OPS_BURST);
        self.refilled = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub(crate) struct Link {
    /// The device's relay key fingerprint, as the relay named it.
    fp: [u8; 32],
    pair: bool,
    pub phase: Phase,
    /// Distinguishes this link from a later one with the same id.
    pub epoch: u64,
    /// The pairing's first frame decrypted (pairing links).
    pub proved: bool,
}

impl Link {
    pub fn new(fp: [u8; 32], pair: bool, epoch: u64) -> Link {
        Link {
            fp,
            pair,
            phase: Phase::AwaitMsg1,
            epoch,
            proved: false,
        }
    }

    pub fn fp(&self) -> [u8; 32] {
        self.fp
    }

    pub fn is_pair(&self) -> bool {
        self.pair
    }

    pub fn is_live(&self) -> bool {
        matches!(self.phase, Phase::Live(_))
    }

    pub fn live_device(&self) -> Option<String> {
        match &self.phase {
            Phase::Live(l) => Some(l.device.clone()),
            _ => None,
        }
    }

    pub fn device_id(&self) -> Option<String> {
        match &self.phase {
            Phase::Live(l) => Some(l.device.clone()),
            Phase::AwaitHello { device, .. } => Some(device.clone()),
            _ => None,
        }
    }

    pub fn session(&mut self) -> Option<&mut Session> {
        match &mut self.phase {
            Phase::PairFirst { session: s, .. } | Phase::PairWaiting(s) => Some(s),
            Phase::AwaitHello { session, .. } => Some(session),
            Phase::Live(l) => Some(&mut l.session),
            Phase::AwaitMsg1 => None,
        }
    }
}

pub(crate) fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub(crate) fn unb64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()
}

/// An error answer, the local control socket's shape.
pub(crate) fn error_answer(id: &Value, code: &str, message: &str) -> Value {
    json!({"id": id, "error": {"code": code, "message": message}})
}

/// The daemon's RPC code as the control socket's error code.
pub(crate) fn code_of(code: i64) -> &'static str {
    match code {
        0 => "daemon_unavailable",
        c if c == i64::from(rlord_core::plugin::RPC_ERR_NOT_ALLOWED) => "not_allowed",
        _ => "failed",
    }
}

static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) fn next_epoch() -> u64 {
    EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Actor {
    /// The relay says a device is connected.
    pub(crate) fn device_link(&mut self, id: u32, fp: [u8; 32]) {
        // The host's own check: a link the relay invents for a device not
        // on our list is refused, whatever the relay thinks.
        if self.devices.by_fp(&fp).is_none() {
            return self.drop_link(id, "not one of this host's devices");
        }
        self.links.insert(id, Link::new(fp, false, next_epoch()));
    }

    /// A lane-1 frame from the relay.
    pub(crate) fn live_frame(&mut self, id: u32, payload: &[u8]) {
        let Some(l) = self.links.get_mut(&id) else {
            return;
        };
        if l.is_pair() {
            return self.pair_frame(id, payload);
        }
        let fp = l.fp();
        match std::mem::replace(&mut l.phase, Phase::AwaitMsg1) {
            Phase::AwaitMsg1 => self.reconnect(id, fp, payload),
            Phase::AwaitHello {
                mut session,
                device,
            } => {
                let hello = session
                    .decrypt(payload)
                    .ok()
                    .and_then(|p| stream::single(&p).ok())
                    .filter(|m| m.stream == CONTROL)
                    .and_then(|m| serde_json::from_slice::<Value>(&m.body).ok());
                if hello.as_ref().and_then(|v| v["event"].as_str()) != Some("hello") {
                    return self.drop_link(id, "no hello");
                }
                // The session exists now: the device proved its key.
                let _ = self.devices.seen(&device, super::now());
                self.audit.log(&device, "link");
                let ev = json!({
                    "event": "hello",
                    "version": env!("CARGO_PKG_VERSION"),
                    "rev": self.rev,
                    "device": device,
                });
                if let Some(l) = self.links.get_mut(&id) {
                    l.phase = Phase::Live(Box::new(Live::new(session, device)));
                }
                self.send_control(id, &ev);
            }
            Phase::Live(mut live) => {
                let msg = match live.session.decrypt(payload) {
                    Ok(p) => live.reasm.push(&p),
                    Err(e) => Err(e),
                };
                let device = live.device.clone();
                if let Some(l) = self.links.get_mut(&id) {
                    l.phase = Phase::Live(live);
                }
                match msg {
                    Err(e) => self.drop_link(id, &format!("{device}: {e}")),
                    Ok(None) => {}
                    Ok(Some(m)) if m.stream != CONTROL => {
                        self.drop_link(id, "panes are not served yet")
                    }
                    Ok(Some(m)) => self.control_message(id, &m.body),
                }
            }
            other => {
                if let Some(l) = self.links.get_mut(&id) {
                    l.phase = other;
                }
            }
        }
    }

    /// Message 1 of a live reconnect, answered with the device's stored
    /// key (a revoked or swapped device fails here).
    fn reconnect(&mut self, id: u32, fp: [u8; 32], msg1: &[u8]) {
        let (Some(keys), Some(dev)) = (self.keys.as_ref(), self.devices.by_fp(&fp)) else {
            return self.drop_link(id, "unknown device");
        };
        if Kind::of_message(msg1).ok() != Some(Kind::Reconnect) {
            return self.drop_link(id, "expected a reconnect");
        }
        let Some(device_static) = dev.static_key() else {
            return self.drop_link(id, "bad device record");
        };
        let device = dev.id.clone();
        match Responder::reconnect(&keys.host, &device_static, &keys.relay_id(), msg1, None) {
            Ok((est, msg2)) => match est.into_session() {
                Ok(session) => {
                    if let Some(l) = self.links.get_mut(&id) {
                        l.phase = Phase::AwaitHello { session, device };
                    }
                    self.send_live(id, msg2);
                }
                Err(e) => self.drop_link(id, &e.to_string()),
            },
            Err(e) => self.drop_link(id, &format!("{device}: handshake: {e}")),
        }
    }

    /// Encrypt and send a control message on a link's session.
    pub(crate) fn send_control(&mut self, id: u32, msg: &Value) {
        let Some(l) = self.links.get_mut(&id) else {
            return;
        };
        let Some(s) = l.session() else {
            return;
        };
        let body = msg.to_string();
        let frames = match stream::fragment(CONTROL, body.as_bytes()) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("rlord-web: an answer too large for a device: {e}");
                return;
            }
        };
        let mut out = Vec::with_capacity(frames.len());
        for f in frames {
            match s.encrypt(&f) {
                Ok(w) => out.push(w),
                Err(e) => return self.drop_link(id, &e.to_string()),
            }
        }
        for payload in out {
            if !self.send_live(id, payload) {
                return;
            }
        }
    }

    /// A whole control message from a live device.
    fn control_message(&mut self, id: u32, body: &[u8]) {
        let Ok(req) = serde_json::from_slice::<Value>(body) else {
            return self.send_control(id, &error_answer(&Value::Null, "bad_request", "not JSON"));
        };
        if let Some(ev) = req["event"].as_str() {
            return match ev {
                "mailbox_key" => self.mailbox_key(id, &req),
                "mailbox_ready" => self.mailbox_ready(id, &req),
                _ => {}
            };
        }
        let Some(l) = self.links.get_mut(&id) else {
            return;
        };
        let epoch = l.epoch;
        let Phase::Live(live) = &mut l.phase else {
            return;
        };
        if !live.take() {
            return self.send_control(
                id,
                &error_answer(&req["id"], "rate_limited", "too many requests; slow down"),
            );
        }
        let device = live.device.clone();
        let next = self.admit(&device, &req, Waiter::Live { link: id, epoch });
        if matches!(next, Next::Run { key: Some(_), .. }) {
            // Recorded as running before it runs: a resend by id, by any
            // way, never runs it again.
            if let Err(e) = self.mailboxes.save(&device) {
                eprintln!("rlord-web: saving a request store: {e:#}");
            }
        }
        self.proceed(&device, next, "op");
    }

    /// Kind-3 message 1, on a live session: answer, and keep the mailbox
    /// until the device's first mailbox frame proves it.
    fn mailbox_key(&mut self, id: u32, req: &Value) {
        let Some(msg1) = req["msg"].as_str().and_then(unb64) else {
            return self.drop_link(id, "bad mailbox key");
        };
        let Some(keys) = self.keys.as_ref() else {
            return;
        };
        let Some(l) = self.links.get(&id) else {
            return;
        };
        let Some(dev) = self.devices.by_fp(&l.fp()) else {
            return self.drop_link(id, "unknown device");
        };
        let Some(device_static) = dev.static_key() else {
            return;
        };
        match Responder::mailbox(&keys.host, &device_static, &keys.relay_id(), &msg1, None)
            .and_then(|(est, msg2)| est.into_mailbox().map(|m| (m, msg2)))
        {
            Ok((m, msg2)) => {
                if let Some(Phase::Live(live)) = self.links.get_mut(&id).map(|l| &mut l.phase) {
                    live.mailbox_pending = Some(m);
                }
                self.send_control(id, &json!({"event": "mailbox_key", "msg": b64(&msg2)}));
            }
            Err(e) => self.drop_link(id, &format!("mailbox keying: {e}")),
        }
    }

    /// The device's first mailbox frame: the new mailbox is proved; save
    /// it in place of any older one.
    fn mailbox_ready(&mut self, id: u32, req: &Value) {
        let Some(frame) = req["frame"].as_str().and_then(unb64) else {
            return self.drop_link(id, "bad mailbox frame");
        };
        let Some(Phase::Live(live)) = self.links.get_mut(&id).map(|l| &mut l.phase) else {
            return;
        };
        let Some(mut m) = live.mailbox_pending.take() else {
            return;
        };
        let device = live.device.clone();
        // The frame carries the mailbox id in front, as on lane 2.
        if frame.len() < 8 || frame[..8] != m.id() || m.decrypt(&frame[8..]).is_err() {
            return self.drop_link(id, "mailbox keying failed");
        }
        if let Err(e) = self.mailboxes.install(&device, m) {
            eprintln!("rlord-web: saving a mailbox: {e:#}");
            return;
        }
        self.audit.log(&device, "mailbox keyed");
    }

    pub(crate) fn tx_clone(&self) -> tokio::sync::mpsc::UnboundedSender<super::Input> {
        self.tx.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_limit_allows_a_burst_then_twenty_a_second() {
        let s = {
            let host = rlord_e2e::StaticKey::from_private([1; 32]);
            let dev = rlord_e2e::StaticKey::from_private([2; 32]);
            let r = rlord_e2e::RelayId::new("http://127.0.0.1", [0; 32]);
            let (ini, m1) = rlord_e2e::Initiator::reconnect(&dev, host.public(), &r, None).unwrap();
            let (he, m2) = Responder::reconnect(&host, dev.public(), &r, &m1, None).unwrap();
            ini.finish(&m2).unwrap();
            he.into_session().unwrap()
        };
        let mut l = Live::new(s, "d".into());
        let n = (0..100).filter(|_| l.take()).count();
        assert_eq!(n, 40);
        l.refilled -= std::time::Duration::from_millis(500);
        let n = (0..100).filter(|_| l.take()).count();
        assert!((9..=11).contains(&n), "{n}");
    }
}
