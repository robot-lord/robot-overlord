//! The device engine's state machine, sans-IO: no tokio, no sockets, no
//! clock of its own. Every input carries the time ([`Now`]); every effect
//! is an [`Action`] the driver carries out in order (so a `Save` always
//! lands before the frame queued after it leaves). The rules are those of
//! `docs/working/web-ui-remote.md`, "The reconnect loop", and
//! `docs/working/e2e-protocol.md` §4 (Device); each has a named test in
//! `tests.rs`.
//!
//! One link at a time: a `device` link to the paired host's relay, or a
//! `pair` link while pairing. A link carries at most one Noise session
//! (never reused after a reconnect); requests that change something live
//! in the [`Outbox`] until answered.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use serde_json::{json, Value};

use rlord_e2e::stream::{self, Reassembler, CONTROL};
use rlord_e2e::{device_id, Initiator, MailboxSession, QrPayload, RelayId, Session, StaticKey};
use rlord_relay_proto::address::{b64_fingerprint, fingerprint_from_b64};
use rlord_relay_proto::frame::{FromDevice, ToDevice};
use rlord_relay_proto::msg::{close, code, HostState, Ready};
use rlord_relay_proto::{FromRelay, RelayAddress, Role, ToRelay};

use crate::outbox::{self, Entry, ErrorInfo, Outbox, State};
use crate::store::{self, b64, unb64, HostRecord, Record, RECORD_VERSION};
use crate::{BackgroundOutcome, Signers, API_MAX, API_MIN};

#[cfg(test)]
mod tests;

/// Reconnect backoff: 250 ms doubling to 30 s, each with up to 20% jitter.
pub const BACKOFF_MIN_MS: u64 = 250;
pub const BACKOFF_MAX_MS: u64 = 30_000;
pub const JITTER_PERCENT: u64 = 20;
/// A link that stayed ready this long resets the backoff.
pub const STABLE_MS: u64 = 10_000;
/// A second 4205 within this window takes the backoff.
pub const RESET_REPEAT_MS: u64 = 60_000;
/// Ping the relay this often; give a link up after this much silence.
pub const PING_MS: u64 = 15_000;
pub const SILENCE_MS: u64 = 45_000;
/// A wall or monotonic clock jump of this much between ticks redials.
pub const JUMP_MS: u64 = 10_000;
/// Message 2 (and then the host's hello) is due within this.
pub const HANDSHAKE_MS: u64 = 10_000;
/// A handshake that got nothing is tried again on the same link after
/// 1 s, doubling to 30 s.
pub const HANDSHAKE_RETRY_MIN_MS: u64 = 1_000;
pub const HANDSHAKE_RETRY_MAX_MS: u64 = 30_000;
/// The relay answers a mailbox frame (`queued` / `refused`) within this,
/// or the link is given up.
pub const MAILBOX_ANSWER_MS: u64 = 10_000;
/// A frame refused `full` is sent again this often (and on each link).
pub const REFUSED_RETRY_MS: u64 = 30_000;
/// The host confirms a pairing within 120 s; a little slack.
pub const PAIR_CONFIRM_MS: u64 = 130_000;
/// `device.leave` is answered within this, or the phone forgets the host
/// anyway.
pub const LEAVE_MS: u64 = 5_000;
/// Device names are clipped to this many characters.
pub const NAME_MAX: usize = 40;

/// Timings a test may shorten (the integration tests' stalled links).
/// The defaults are the protocol's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tuning {
    pub ping_ms: u64,
    pub silence_ms: u64,
}

impl Default for Tuning {
    fn default() -> Tuning {
        Tuning {
            ping_ms: PING_MS,
            silence_ms: SILENCE_MS,
        }
    }
}

/// The time, as the driver read it: a monotonic clock and the wall clock,
/// both in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Now {
    pub mono: u64,
    /// Unix milliseconds.
    pub wall: u64,
}

/// A link reached `ready`.
#[derive(Clone, Debug)]
pub struct Linked {
    pub ready: Ready,
    /// The relay this connection proved (what goes in a prologue).
    pub relay: RelayId,
}

/// Why a dial ended before `ready`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialError {
    /// The relay is not the pinned one (origin, key or signature).
    WrongRelay(String),
    /// The relay closed with this code.
    Closed(u16),
    /// The upgrade was refused with this HTTP status (a redirect too).
    Http(u16),
    /// `bye` before `ready`.
    Bye { retry_after: Option<u64> },
    /// TCP, TLS, a timeout, anything else.
    Other(String),
}

pub enum Input {
    /// A message from the page (JSON).
    Page(String),
    /// The native scanner read a QR.
    Scanned {
        qr: String,
        name: String,
    },
    Lifecycle {
        foreground: bool,
    },
    Network {
        online: bool,
    },
    /// Start one background check that may take `budget_ms`.
    Background {
        budget_ms: u64,
    },
    /// Forget everything (no network).
    Wipe,
    Dialed {
        link: u64,
        result: Result<Linked, DialError>,
    },
    /// The relay asked for enrolment while connecting (shown, waited on).
    Enroll {
        link: u64,
        url: String,
        code: String,
    },
    Text {
        link: u64,
        text: String,
    },
    Binary {
        link: u64,
        bytes: Vec<u8>,
    },
    /// A pong (or any websocket control frame) arrived.
    Heard {
        link: u64,
    },
    Closed {
        link: u64,
        code: u16,
    },
    /// A `Save` failed: what was queued after it was not sent.
    SaveFailed {
        reason: String,
    },
    /// Time passed (the driver ticks every second).
    Tick,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Open a link: exactly the pin's one URL, signing with `label`.
    Dial {
        link: u64,
        pin: RelayAddress,
        role: Role,
        target: Vec<u8>,
        label: String,
    },
    Text {
        link: u64,
        json: String,
    },
    Binary {
        link: u64,
        bytes: Vec<u8>,
    },
    Ping {
        link: u64,
    },
    /// Close the link (after what was queued on it).
    Close {
        link: u64,
        code: u16,
    },
    /// Seal and write a host's record (atomically). Whatever comes after
    /// it is sent only once this is on disk.
    Save {
        file: String,
        plain: Vec<u8>,
    },
    Delete {
        file: String,
    },
    ToPage(String),
    Notify {
        kind: String,
        count: u32,
    },
    BackgroundDone(BackgroundOutcome),
}

/// Why the loop stopped until the user acts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    Revoked,
    Suspended,
    UpdateApp,
    UpdateHost,
    WrongRelay,
}

impl Stop {
    fn state(self) -> &'static str {
        match self {
            Stop::Revoked => "revoked",
            Stop::Suspended => "suspended",
            Stop::UpdateApp => "update_app",
            Stop::UpdateHost => "update_host",
            Stop::WrongRelay => "wrong_relay",
        }
    }
}

/// The paired host, parsed from its record.
struct Host {
    static_key: [u8; 32],
    pin: RelayAddress,
    fp: [u8; 32],
}

enum Sess {
    None,
    /// Message 1 sent; message 2 due by `until`.
    Hs {
        ini: Initiator,
        until: u64,
    },
    /// Our hello sent; the host's due by `until`.
    Hello {
        s: Session,
        reasm: Reassembler,
        until: u64,
    },
    Live {
        s: Session,
        reasm: Reassembler,
    },
    /// Pairing: message 1 sent.
    PairHs(Initiator),
    /// Pairing: our first frame sent; `code` is what this side shows.
    Pair {
        s: Session,
        reasm: Reassembler,
        code: String,
    },
}

struct Up {
    id: u64,
    role: Role,
    relay: RelayId,
    boot: String,
    since: u64,
    last_rx: u64,
    last_ping: u64,
    n: u64,
    host_online: bool,
    sess: Sess,
    hs_attempt: u32,
    hs_retry_at: u64,
    /// A mailbox frame waiting for `queued` / `refused`.
    mb_inflight: Option<(String, Vec<u8>, u64)>,
    /// A mailbox being keyed on this session.
    mb_keying: Option<Initiator>,
    /// The one resend pass of this link is done (a link being the host's
    /// link to this device: a new one when the host comes back online;
    /// handshakes retried on it make no second pass).
    resent: bool,
    /// The refused frame was offered again on this link.
    refused_tried: bool,
    /// `bye` said: not before this many seconds.
    bye_retry: Option<u64>,
    /// Mailbox frames the relay said wait for us, not yet taken.
    queued_left: u64,
}

enum Link {
    Idle,
    Dialing { id: u64, role: Role },
    Up(Box<Up>),
}

struct Pairing {
    qr: QrPayload,
    name: String,
    static_key: StaticKey,
    label: String,
    public: Vec<u8>,
    until: u64,
}

/// Where a host's answer goes.
enum Owed {
    /// A page request: the page's own id.
    Page(Value),
}

struct Bg {
    until: u64,
    need_session: bool,
    snapshot: Option<String>,
    checked: bool,
}

pub struct Machine {
    signers: Arc<dyn Signers>,
    tuning: Tuning,
    rng: u64,
    now: Now,
    last_tick: Option<Now>,
    out: VecDeque<Action>,

    rec: Option<Record>,
    static_key: Option<StaticKey>,
    host: Option<Host>,
    mailbox: Option<MailboxSession>,

    foreground: bool,
    online: bool,
    stop: Option<Stop>,
    stop_detail: Option<String>,
    enroll: Option<String>,

    next_link: u64,
    link: Link,
    dial_at: Option<u64>,
    attempt: u32,
    last_reset: Option<u64>,
    replaced: bool,
    bye_retry: Option<u64>,

    scanned: Option<(String, String)>,
    pairing: Option<Pairing>,
    pairing_event: Option<Value>,

    /// Requests sent on the live session, by request id.
    owed: HashMap<String, Owed>,
    last_hello: Option<Value>,
    last_rev: Option<String>,
    last_conn: Option<Value>,
    signal_pending: bool,
    leaving: Option<(String, u64)>,
    /// A mailbox frame refused `full`: sent again before any newer one.
    refused: Option<(String, Vec<u8>, u64)>,
    bg: Option<Bg>,
    bg_result: Option<BackgroundOutcome>,
}

fn page_error(id: &Value, code: &str, message: &str) -> Value {
    json!({"id": id, "error": {"code": code, "message": message}})
}

fn random_id() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("the OS random source");
    b64(&b)
}

impl Machine {
    /// A machine over what was saved (`saved`: the host records found in
    /// the engine's folder, opened). `seed` drives the jitter only.
    pub fn new(signers: Arc<dyn Signers>, saved: Vec<Record>, seed: u64) -> Machine {
        let mut m = Machine {
            signers,
            tuning: Tuning::default(),
            rng: seed | 1,
            now: Now::default(),
            last_tick: None,
            out: VecDeque::new(),
            rec: None,
            static_key: None,
            host: None,
            mailbox: None,
            foreground: false,
            online: true,
            stop: None,
            stop_detail: None,
            enroll: None,
            next_link: 1,
            link: Link::Idle,
            dial_at: None,
            attempt: 0,
            last_reset: None,
            replaced: false,
            bye_retry: None,
            scanned: None,
            pairing: None,
            pairing_event: None,
            owed: HashMap::new(),
            last_hello: None,
            last_rev: None,
            last_conn: None,
            signal_pending: false,
            leaving: None,
            refused: None,
            bg: None,
            bg_result: None,
        };
        // One host in this slice: the first record that opens.
        for r in saved {
            if m.adopt(r) {
                break;
            }
        }
        m
    }

    /// Takes `r` as the paired host, when it parses.
    fn adopt(&mut self, r: Record) -> bool {
        let parsed = (|| {
            let sk: [u8; 32] = unb64(&r.static_key)?.try_into().ok()?;
            let hs: [u8; 32] = unb64(&r.host.static_key)?.try_into().ok()?;
            let pin = RelayAddress::parse(&r.host.pin).ok()?;
            let fp = fingerprint_from_b64(&r.host.fp).ok()?;
            let mb = match &r.mailbox {
                Some(m) => Some(MailboxSession::import(&unb64(m)?).ok()?),
                None => None,
            };
            Some((StaticKey::from_private(sk), hs, pin, fp, mb))
        })();
        let Some((sk, hs, pin, fp, mb)) = parsed else {
            return false;
        };
        self.static_key = Some(sk);
        self.host = Some(Host {
            static_key: hs,
            pin,
            fp,
        });
        self.mailbox = mb;
        self.last_rev = r.rev.clone();
        self.rec = Some(r);
        true
    }

    pub fn set_tuning(&mut self, t: Tuning) {
        self.tuning = t;
    }

    /// The next action for the driver, oldest first.
    pub fn poll(&mut self) -> Option<Action> {
        self.out.pop_front()
    }

    /// When the machine next wants a `Tick` (monotonic ms), besides the
    /// driver's one-second tick.
    pub fn deadline(&self) -> Option<u64> {
        let mut d = self.dial_at;
        let mut at = |t: u64| d = Some(d.map_or(t, |x: u64| x.min(t)));
        if let Link::Up(up) = &self.link {
            if matches!(up.sess, Sess::None) && up.host_online {
                at(up.hs_retry_at);
            }
        }
        if let Some(bg) = &self.bg {
            at(bg.until);
        }
        d
    }

    /// Is there a live session now?
    pub fn live(&self) -> bool {
        matches!(&self.link, Link::Up(up) if matches!(up.sess, Sess::Live { .. }))
    }

    pub fn paired(&self) -> bool {
        self.rec.is_some()
    }

    /// The saved record (tests).
    pub fn record(&self) -> Option<&Record> {
        self.rec.as_ref()
    }

    pub fn handle(&mut self, now: Now, input: Input) {
        self.now = now;
        match input {
            Input::Page(text) => self.page(&text),
            Input::Scanned { qr, name } => self.scanned(qr, name),
            Input::Lifecycle { foreground } => self.lifecycle(foreground),
            Input::Network { online } => self.network(online),
            Input::Background { budget_ms } => self.background(budget_ms),
            Input::Wipe => self.wipe(),
            Input::Dialed { link, result } => self.dialed(link, result),
            Input::Enroll { link, url, code } => {
                if self.is_current(link) {
                    self.enroll = Some(format!("enrol at {url} with code {code}"));
                }
            }
            Input::Text { link, text } => {
                if let Some(up) = self.up_mut(link) {
                    up.last_rx = now.mono;
                    self.text(&text);
                }
            }
            Input::Binary { link, bytes } => {
                if let Some(up) = self.up_mut(link) {
                    up.last_rx = now.mono;
                    self.binary(&bytes);
                }
            }
            Input::Heard { link } => {
                if let Some(up) = self.up_mut(link) {
                    up.last_rx = now.mono;
                }
            }
            Input::Closed { link, code } => {
                if self.is_current(link) {
                    self.closed(code);
                }
            }
            Input::SaveFailed { reason } => self.save_failed(&reason),
            Input::Tick => self.tick(),
        }
        self.after();
    }

    fn is_current(&self, link: u64) -> bool {
        match &self.link {
            Link::Dialing { id, .. } => *id == link,
            Link::Up(up) => up.id == link,
            Link::Idle => false,
        }
    }

    fn up_mut(&mut self, link: u64) -> Option<&mut Up> {
        match &mut self.link {
            Link::Up(up) if up.id == link => Some(up),
            _ => None,
        }
    }

    fn up(&mut self) -> Option<&mut Up> {
        match &mut self.link {
            Link::Up(up) => Some(up),
            _ => None,
        }
    }

    // ---- output helpers ----

    fn send_page(&mut self, v: Value) {
        self.out.push_back(Action::ToPage(v.to_string()));
    }

    fn save(&mut self) {
        if let Some(r) = &self.rec {
            self.out.push_back(Action::Save {
                file: r.file(),
                plain: r.encode(),
            });
        }
    }

    fn emit_outbox(&mut self) {
        let ev = self
            .rec
            .as_ref()
            .map(|r| r.outbox.event())
            .unwrap_or_else(|| Outbox::default().event());
        self.send_page(ev);
    }

    fn pairing_ev(&mut self, v: Value) {
        let done = matches!(v["state"].as_str(), Some("done" | "rejected" | "failed"));
        self.pairing_event = (!done).then(|| v.clone());
        self.send_page(v);
    }

    fn conn(&self) -> Value {
        let state = if let Some(s) = self.stop {
            s.state()
        } else if self.rec.is_none() {
            "unpaired"
        } else if !self.online {
            "offline"
        } else {
            match &self.link {
                Link::Up(up) if up.role == Role::Device => {
                    if matches!(up.sess, Sess::Live { .. }) {
                        "live"
                    } else if !up.host_online {
                        "host_offline"
                    } else {
                        "relay"
                    }
                }
                _ => "connecting",
            }
        };
        let mut v = json!({"event": "conn", "state": state});
        if let Some(r) = &self.rec {
            v["host"] = json!(r.host.name);
        }
        if let Some(h) = &self.host {
            v["relay"] = json!(h.pin.origin);
        }
        let detail = self.stop_detail.clone().or_else(|| self.enroll.clone());
        if let Some(d) = detail {
            v["detail"] = json!(d);
        }
        v
    }

    fn emit_conn(&mut self, force: bool) {
        let c = self.conn();
        if force || self.last_conn.as_ref() != Some(&c) {
            self.last_conn = Some(c.clone());
            self.send_page(c);
        }
    }

    fn status(&self) -> Value {
        let mut v = json!({
            "event": "status",
            "state": self.conn()["state"],
            "keys": if self.signers.hardware() { "hardware" } else { "software" },
            "version": crate::VERSION,
            "api": {"min": API_MIN, "max": API_MAX},
        });
        if let Some(r) = &self.rec {
            v["host"] = json!(r.host.name);
            v["device_name"] = json!(r.device_name);
        }
        if let Some(h) = &self.host {
            v["relay"] = json!(h.pin.origin);
        }
        if let Some(k) = &self.static_key {
            v["device_id"] = json!(device_id(k.public()));
        }
        v
    }

    // ---- randomness and backoff ----

    fn rand(&mut self) -> u64 {
        // xorshift64*: jitter only, never keys.
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn jitter(&mut self, ms: u64) -> u64 {
        let max = ms * JITTER_PERCENT / 100;
        ms + if max == 0 { 0 } else { self.rand() % (max + 1) }
    }

    /// The backoff delay for `attempt` (before jitter).
    pub fn backoff_ms(attempt: u32) -> u64 {
        BACKOFF_MIN_MS
            .saturating_mul(1u64 << attempt.min(20))
            .min(BACKOFF_MAX_MS)
    }

    /// Plan the next dial after a failure.
    fn schedule_backoff(&mut self) {
        let base = if self.replaced {
            BACKOFF_MAX_MS
        } else {
            Self::backoff_ms(self.attempt)
        };
        self.attempt = self.attempt.saturating_add(1);
        let mut delay = self.jitter(base);
        if let Some(s) = self.bye_retry.take() {
            delay = delay.max(self.jitter(s * 1000));
        }
        self.dial_at = Some(self.now.mono + delay);
    }

    fn dial_now(&mut self) {
        self.dial_at = Some(self.now.mono);
    }

    // ---- links ----

    fn want_device_link(&self) -> bool {
        self.rec.is_some()
            && self.stop.is_none()
            && self.online
            && self.pairing.is_none()
            && (self.foreground || self.bg.is_some() || self.leaving.is_some())
    }

    fn want_session(&self) -> bool {
        self.foreground
            || self.leaving.is_some()
            || self.bg.as_ref().is_some_and(|b| b.need_session)
    }

    /// Close the current link (if any) now.
    fn drop_link(&mut self, code: u16) {
        let link = std::mem::replace(&mut self.link, Link::Idle);
        let id = match &link {
            Link::Idle => return,
            Link::Dialing { id, .. } => *id,
            Link::Up(up) => up.id,
        };
        self.out.push_back(Action::Close { link: id, code });
        if let Link::Up(up) = link {
            self.link_lost(&up);
        }
    }

    /// What dies with a link: its session's look-ups, and the fate of a
    /// mailbox frame in flight (sent again later).
    fn link_lost(&mut self, up: &Up) {
        let had_session = matches!(up.sess, Sess::Live { .. });
        if had_session {
            self.fail_lookups();
        }
        if let Some((id, _, _)) = &up.mb_inflight {
            if let Some(e) = self.rec.as_mut().and_then(|r| r.outbox.get_mut(id)) {
                e.queued_boot = None;
            }
        }
        if self.leaving.is_some() && up.role == Role::Device {
            self.finish_unpair(false);
        }
        if let Some(bg) = &self.bg {
            if !bg.checked && up.role == Role::Device {
                self.bg_done(BackgroundOutcome::Failed {
                    reason: "the link ended".into(),
                });
            }
        }
        if up.role == Role::Pair && self.pairing.is_some() {
            self.fail_pairing("failed", "the connection ended before the pairing finished");
        }
    }

    /// Look-ups in flight fail when their session ends (as on the local
    /// page: the store asks again on `hello`).
    fn fail_lookups(&mut self) {
        let owed: Vec<(String, Owed)> = self.owed.drain().collect();
        for (rid, o) in owed {
            let open = self
                .rec
                .as_ref()
                .is_some_and(|r| r.outbox.get(&rid).is_some());
            match o {
                Owed::Page(pid) if !open => self.send_page(page_error(
                    &pid,
                    "disconnected",
                    "the connection to the computer ended; ask again",
                )),
                // An outbox entry keeps waiting for its answer.
                o => {
                    self.owed.insert(rid, o);
                }
            }
        }
    }

    fn dial_device(&mut self) {
        let (Some(h), Some(r)) = (&self.host, &self.rec) else {
            return;
        };
        let id = self.next_link;
        self.next_link += 1;
        let a = Action::Dial {
            link: id,
            pin: h.pin.clone(),
            role: Role::Device,
            target: h.fp.to_vec(),
            label: r.label.clone(),
        };
        self.link = Link::Dialing {
            id,
            role: Role::Device,
        };
        self.dial_at = None;
        self.out.push_back(a);
    }

    fn dialed(&mut self, link: u64, result: Result<Linked, DialError>) {
        let role = match &self.link {
            Link::Dialing { id, role } if *id == link => *role,
            _ => {
                if result.is_ok() {
                    self.out.push_back(Action::Close { link, code: 1000 });
                }
                return;
            }
        };
        self.enroll = None;
        let linked = match result {
            Ok(l) => l,
            Err(e) => {
                self.link = Link::Idle;
                if role == Role::Pair {
                    let reason = match e {
                        DialError::WrongRelay(w) => w,
                        DialError::Closed(close::NOT_ALLOWED) => {
                            "this code was used already or has expired".into()
                        }
                        other => format!("could not reach the relay: {other:?}"),
                    };
                    return self.fail_pairing("failed", &reason);
                }
                return match e {
                    DialError::WrongRelay(w) => self.stop_with(Stop::WrongRelay, w),
                    DialError::Closed(c) => self.close_code(c),
                    DialError::Bye { retry_after } => {
                        self.bye_retry = retry_after;
                        self.schedule_backoff();
                    }
                    DialError::Http(_) | DialError::Other(_) => self.schedule_backoff(),
                };
            }
        };
        let now = self.now.mono;
        let ready = linked.ready;
        self.link = Link::Up(Box::new(Up {
            id: link,
            role,
            relay: linked.relay,
            boot: ready.boot.clone(),
            since: now,
            last_rx: now,
            last_ping: now,
            n: 0,
            host_online: ready.host == Some(HostState::Online),
            sess: Sess::None,
            hs_attempt: 0,
            hs_retry_at: now,
            mb_inflight: None,
            mb_keying: None,
            resent: false,
            refused_tried: false,
            bye_retry: None,
            queued_left: ready.queued.unwrap_or(0),
        }));
        if role == Role::Pair {
            return self.pair_start();
        }
        // The relay restarted: what it had queued for the host is gone.
        let boot_changed = self
            .rec
            .as_ref()
            .is_some_and(|r| r.boot.as_deref() != Some(ready.boot.as_str()));
        if ready.signal == Some(1) {
            self.signal_pending = true;
        }
        if let Some(bg) = &mut self.bg {
            let r = self.rec.as_ref().expect("paired");
            let waiting = r.outbox.entries.iter().any(|e| e.open());
            let nothing = ready.signal.unwrap_or(0) == 0
                && ready.queued.unwrap_or(0) == 0
                && !boot_changed
                && !waiting;
            if nothing {
                bg.checked = true;
                return self.bg_done(BackgroundOutcome::NothingNew);
            }
            bg.need_session = true;
        }
        if boot_changed {
            let r = self.rec.as_mut().expect("paired");
            for e in r.outbox.entries.iter_mut() {
                if e.queued_boot.as_deref() != Some(ready.boot.as_str()) {
                    e.queued_boot = None;
                }
            }
            r.boot = Some(ready.boot.clone());
            self.save();
        }
    }

    fn close_code(&mut self, c: u16) {
        match c {
            close::RESET => {
                let repeat = self
                    .last_reset
                    .is_some_and(|t| self.now.mono.saturating_sub(t) < RESET_REPEAT_MS);
                self.last_reset = Some(self.now.mono);
                if repeat {
                    self.schedule_backoff();
                } else {
                    self.dial_now();
                }
            }
            close::REPLACED => {
                // Another connection of this identity took over: don't
                // fight it; come back at the longest backoff.
                self.replaced = true;
                self.schedule_backoff();
            }
            close::NOT_ALLOWED => self.revoked(),
            close::SUSPENDED => self.stop_with(
                Stop::Suspended,
                "the relay has suspended this computer's account".into(),
            ),
            close::UNSUPPORTED => self.stop_with(
                Stop::UpdateApp,
                "the relay speaks a newer protocol; update the app".into(),
            ),
            close::BAD_SIGNATURE => {
                self.stop_with(Stop::UpdateApp, "the relay refused this app's hello".into())
            }
            _ => self.schedule_backoff(),
        }
    }

    fn stop_with(&mut self, s: Stop, detail: String) {
        self.stop = Some(s);
        self.stop_detail = Some(detail);
        self.dial_at = None;
        self.drop_link(1000);
        if self.bg.is_some() {
            self.bg_done(BackgroundOutcome::Failed {
                reason: s.state().into(),
            });
        }
    }

    /// 4403 on a device link: the host revoked this phone (or it expired).
    /// Final; the old keys are deleted.
    fn revoked(&mut self) {
        let name = self.rec.as_ref().map(|r| r.host.name.clone());
        self.forget();
        self.stop = Some(Stop::Revoked);
        self.stop_detail = Some(format!(
            "this phone is no longer paired with {}; pair again to use it",
            name.as_deref().unwrap_or("that computer")
        ));
        if self.bg.is_some() {
            self.bg_done(BackgroundOutcome::Failed {
                reason: "revoked".into(),
            });
        }
    }

    /// Deletes the host's record, its keys and file; closes the link.
    fn forget(&mut self) {
        self.drop_link(1000);
        if let Some(r) = self.rec.take() {
            self.out.push_back(Action::Delete { file: r.file() });
            self.signers.delete(r.label.clone());
        }
        self.static_key = None;
        self.host = None;
        self.mailbox = None;
        self.last_hello = None;
        self.last_rev = None;
        self.refused = None;
        self.dial_at = None;
        self.leaving = None;
        let owed: Vec<Owed> = self.owed.drain().map(|(_, o)| o).collect();
        for Owed::Page(pid) in owed {
            self.send_page(page_error(&pid, "disconnected", "not paired"));
        }
        self.emit_outbox();
    }

    fn closed(&mut self, code: u16) {
        let link = std::mem::replace(&mut self.link, Link::Idle);
        let Link::Up(up) = link else {
            // A dial that closed before ready is reported as `Dialed`.
            self.link = link;
            return;
        };
        self.bye_retry = up.bye_retry;
        let role = up.role;
        self.link_lost(&up);
        if role == Role::Device && self.rec.is_some() {
            self.close_code(code);
        }
    }

    fn save_failed(&mut self, reason: &str) {
        // Nothing queued after the save was sent. The mailbox in memory
        // may be ahead of the one on disk: drop it (it is keyed again on
        // the next live session) and start the link over.
        self.mailbox = None;
        if let Some(r) = &mut self.rec {
            r.mailbox = None;
        }
        self.stop_detail = None;
        self.enroll = Some(format!("could not save: {reason}"));
        if matches!(self.link, Link::Up(_) | Link::Dialing { .. }) {
            self.drop_link(1000);
            self.schedule_backoff();
        }
    }

    // ---- the relay's control messages ----

    fn text(&mut self, text: &str) {
        let Ok(m) = FromRelay::parse(text) else {
            return; // a newer relay's message: skipped
        };
        let mono = self.now.mono;
        let Some(up) = self.up() else { return };
        match m {
            FromRelay::Host { state } => {
                let online = state == HostState::Online;
                up.host_online = online;
                if online {
                    up.hs_attempt = 0;
                    up.hs_retry_at = mono;
                    // The host's own link to us is a new one: what it may
                    // have lost meanwhile goes again on the next session.
                    up.resent = false;
                } else if !matches!(up.sess, Sess::PairHs(_) | Sess::Pair { .. }) {
                    up.sess = Sess::None;
                    up.mb_keying = None;
                    self.fail_lookups();
                }
            }
            FromRelay::Signal => self.signal_pending = true,
            FromRelay::Queued { .. } => {
                let boot = up.boot.clone();
                if let Some((id, _, _)) = up.mb_inflight.take() {
                    if self.refused.as_ref().is_some_and(|(r, _, _)| *r == id) {
                        self.refused = None;
                    }
                    if let Some(e) = self.rec.as_mut().and_then(|r| r.outbox.get_mut(&id)) {
                        e.queued_boot = Some(boot);
                        if e.state == State::Waiting {
                            e.state = State::Sent;
                        }
                    }
                    self.save();
                    self.emit_outbox();
                }
            }
            FromRelay::Refused { code: c, .. } => {
                let Some((id, bytes, _)) = up.mb_inflight.take() else {
                    return;
                };
                // Offered on this link: the next try is in 30 s.
                up.refused_tried = true;
                match c.as_str() {
                    code::FULL => self.refused = Some((id, bytes, self.now.mono)),
                    code::NO_QUEUE => {
                        if self.refused.as_ref().is_some_and(|(r, _, _)| *r == id) {
                            self.refused = None;
                        }
                        self.answer(
                            &id,
                            page_error(
                                &json!(id),
                                "host_offline",
                                "the computer is offline and its relay keeps no messages",
                            ),
                        )
                    }
                    _ => {
                        // `unknown` or `bad`: this mailbox is no good.
                        self.refused = None;
                        self.discard_mailbox();
                    }
                }
            }
            FromRelay::Undeliverable { .. } => {
                if matches!(up.sess, Sess::Hs { .. } | Sess::Hello { .. }) {
                    self.handshake_failed();
                } else if matches!(up.sess, Sess::Live { .. }) {
                    // Our frame found no host: the session is gone.
                    up.sess = Sess::None;
                    self.fail_lookups();
                    self.handshake_failed();
                }
            }
            FromRelay::Bye { retry_after, .. } => up.bye_retry = retry_after,
            _ => {}
        }
    }

    fn send_text(&mut self, m: impl FnOnce(u64) -> ToRelay) {
        let Some(up) = self.up() else { return };
        up.n += 1;
        let (link, json) = (up.id, m(up.n).to_json());
        self.out.push_back(Action::Text { link, json });
    }

    // ---- binary frames ----

    fn binary(&mut self, bytes: &[u8]) {
        let Ok(f) = ToDevice::decode(bytes) else {
            // Skipping it could skip one of a session's frames.
            self.drop_link(1002);
            return self.schedule_backoff();
        };
        match f {
            ToDevice::Live(p) => self.live_frame(&p),
            ToDevice::Mailbox { seq, payload } => self.mailbox_frame(seq, &payload),
        }
    }

    fn send_live_raw(&mut self, payload: Vec<u8>) {
        if let Some(up) = self.up() {
            let link = up.id;
            self.out.push_back(Action::Binary {
                link,
                bytes: FromDevice::Live(payload).encode(),
            });
        }
    }

    /// Encrypt and send a control message on the session.
    fn send_json(&mut self, v: &Value) {
        let Ok(frames) = stream::fragment(CONTROL, v.to_string().as_bytes()) else {
            return;
        };
        let Some(up) = self.up() else { return };
        let s = match &mut up.sess {
            Sess::Live { s, .. } | Sess::Hello { s, .. } | Sess::Pair { s, .. } => s,
            _ => return,
        };
        let mut out = Vec::new();
        for f in frames {
            match s.encrypt(&f) {
                Ok(w) => out.push(w),
                Err(_) => return self.poisoned(),
            }
        }
        for w in out {
            self.send_live_raw(w);
        }
    }

    /// Any decrypt or encrypt failure ends the session: close and redial.
    fn poisoned(&mut self) {
        self.drop_link(1000);
        self.schedule_backoff();
    }

    fn live_frame(&mut self, p: &[u8]) {
        let now = self.now.mono;
        let Some(up) = self.up() else { return };
        match std::mem::replace(&mut up.sess, Sess::None) {
            Sess::None => {}
            Sess::Hs { ini, .. } => match ini.finish(p).and_then(|e| e.into_session()) {
                Ok(s) => {
                    up.sess = Sess::Hello {
                        s,
                        reasm: Reassembler::new(),
                        until: now + HANDSHAKE_MS,
                    };
                    self.send_json(&json!({"event": "hello"}));
                }
                // A replayed or forged message 2.
                Err(_) => self.poisoned(),
            },
            Sess::Hello {
                mut s,
                mut reasm,
                until,
            } => match open(&mut s, &mut reasm, p) {
                Err(()) => self.poisoned(),
                Ok(None) => up.sess = Sess::Hello { s, reasm, until },
                Ok(Some(v)) if v["event"] == "hello" => {
                    up.sess = Sess::Live { s, reasm };
                    self.hello(v);
                }
                Ok(Some(_)) => self.poisoned(),
            },
            Sess::Live { mut s, mut reasm } => match open(&mut s, &mut reasm, p) {
                Err(()) => self.poisoned(),
                Ok(m) => {
                    up.sess = Sess::Live { s, reasm };
                    if let Some(v) = m {
                        self.control(v);
                    }
                }
            },
            Sess::PairHs(ini) => self.pair_msg2(ini, p),
            Sess::Pair {
                mut s,
                mut reasm,
                code,
            } => match open(&mut s, &mut reasm, p) {
                Err(()) => self.fail_pairing("failed", "the pairing session failed"),
                Ok(None) => up.sess = Sess::Pair { s, reasm, code },
                Ok(Some(v)) => {
                    up.sess = Sess::Pair {
                        s,
                        reasm,
                        code: code.clone(),
                    };
                    self.pair_event(&code, v);
                }
            },
        }
    }

    // ---- the live session ----

    fn start_handshake(&mut self) {
        let (Some(k), Some(h)) = (&self.static_key, &self.host) else {
            return;
        };
        let hs = h.static_key;
        let k = k.clone();
        let now = self.now.mono;
        let Some(up) = self.up() else { return };
        let Ok((ini, m1)) = Initiator::reconnect(&k, &hs, &up.relay, None) else {
            return;
        };
        up.sess = Sess::Hs {
            ini,
            until: now + HANDSHAKE_MS,
        };
        self.send_live_raw(m1);
    }

    /// No message 2 (or hello) in time, or `undeliverable`: try again on
    /// the same link, 1 s doubling to 30 s.
    fn handshake_failed(&mut self) {
        let now = self.now.mono;
        let Some(up) = self.up() else { return };
        up.sess = Sess::None;
        let d = HANDSHAKE_RETRY_MIN_MS
            .saturating_mul(1 << up.hs_attempt.min(16))
            .min(HANDSHAKE_RETRY_MAX_MS);
        up.hs_attempt = up.hs_attempt.saturating_add(1);
        up.hs_retry_at = now + d;
    }

    fn hello(&mut self, mut v: Value) {
        let api = v["api"].as_u64().unwrap_or(1) as u32;
        if api > API_MAX {
            return self.stop_with(
                Stop::UpdateApp,
                format!("the computer's rlord speaks a newer remote API ({api}); update the app"),
            );
        }
        if api < API_MIN {
            return self.stop_with(
                Stop::UpdateHost,
                format!("the computer's rlord is too old for this app (API {api}); update it"),
            );
        }
        if let Some(up) = self.up() {
            up.hs_attempt = 0;
        }
        if v.get("page").is_none() {
            v["page"] = v["device"].clone();
        }
        let rev = v["rev"].as_str().map(str::to_string);
        self.last_hello = Some(v.clone());
        if self.bg.is_none() || self.foreground {
            self.send_page(v);
        }
        if let Some(r) = &rev {
            if self.foreground {
                self.last_rev = Some(r.clone());
            }
        }
        if self.mailbox.is_none() {
            self.key_mailbox();
        }
        self.resend_pass();
        if self.bg.is_some() {
            self.bg_live(rev);
        }
        if let Some((rid, _)) = self.leaving.clone() {
            self.send_json(&json!({"id": rid, "op": "device.leave", "args": {}}));
        }
    }

    /// The one resend pass of this link: every unanswered entry, oldest
    /// first, by its id.
    fn resend_pass(&mut self) {
        let Some(up) = self.up() else { return };
        if up.resent {
            return;
        }
        up.resent = true;
        let wall = self.now.wall;
        let mut reqs = Vec::new();
        if let Some(r) = &mut self.rec {
            for e in r.outbox.entries.iter_mut().filter(|e| e.open()) {
                if e.state == State::Waiting {
                    e.state = State::Sent;
                }
                e.first_sent.get_or_insert(wall);
                reqs.push(e.request());
            }
        }
        if reqs.is_empty() {
            return;
        }
        self.save();
        for r in reqs {
            self.send_json(&r);
        }
        self.emit_outbox();
    }

    fn control(&mut self, v: Value) {
        if let Some(ev) = v["event"].as_str() {
            match ev {
                "mailbox_key" => return self.mailbox_keyed(&v),
                "hello" | "pair" => return,
                "changed" => {
                    let rev = v["rev"].as_str().map(str::to_string);
                    if rev.is_some() && rev == self.last_rev {
                        return;
                    }
                    if self.foreground {
                        self.last_rev = rev;
                    }
                }
                _ => {}
            }
            if self.bg.is_none() || self.foreground {
                self.send_page(v);
            }
            return;
        }
        let Some(rid) = v["id"].as_str().map(str::to_string) else {
            return;
        };
        self.answer(&rid, v);
    }

    /// An answer from the host (live or by mailbox) to request `rid`.
    fn answer(&mut self, rid: &str, v: Value) {
        if self.leaving.as_ref().is_some_and(|(l, _)| l == rid) {
            return self.finish_unpair(true);
        }
        if self
            .bg
            .as_ref()
            .is_some_and(|b| b.snapshot.as_deref() == Some(rid))
        {
            return self.bg_snapshot(&v);
        }
        let in_outbox = self
            .rec
            .as_ref()
            .is_some_and(|r| r.outbox.get(rid).is_some());
        if !in_outbox {
            // A look-up, or an answer for an id already answered (dropped).
            if let Some(Owed::Page(pid)) = self.owed.remove(rid) {
                if self.signal_pending && v.get("ok").is_some() {
                    self.signal_pending = false;
                    self.send_text(|n| ToRelay::AckSignal { n });
                }
                let mut v = v;
                v["id"] = pid;
                self.send_page(v);
            }
            return;
        }
        let code = v["error"]["code"].as_str().map(str::to_string);
        if code.as_deref() == Some("in_progress") {
            if let Some(e) = self.rec.as_mut().and_then(|r| r.outbox.get_mut(rid)) {
                if e.state != State::Running {
                    e.state = State::Running;
                    self.save();
                    self.emit_outbox();
                }
            }
            return;
        }
        let page = self.owed.remove(rid).map(|Owed::Page(pid)| pid);
        let r = self.rec.as_mut().expect("paired");
        if v.get("ok").is_some() || page.is_some() {
            r.outbox.remove(rid);
        } else if let Some(e) = r.outbox.get_mut(rid) {
            let code = code.unwrap_or_else(|| "failed".into());
            e.state = if code == "interrupted" {
                State::Interrupted
            } else {
                State::Failed
            };
            e.error = Some(ErrorInfo {
                code,
                message: v["error"]["message"].as_str().unwrap_or("").to_string(),
            });
        }
        self.save();
        self.emit_outbox();
        if let Some(pid) = page {
            let mut v = v;
            v["id"] = pid;
            self.send_page(v);
        }
    }

    // ---- the mailbox ----

    fn key_mailbox(&mut self) {
        let (Some(k), Some(h)) = (&self.static_key, &self.host) else {
            return;
        };
        let (k, hs) = (k.clone(), h.static_key);
        let Some(up) = self.up() else { return };
        if up.mb_keying.is_some() {
            return;
        }
        let Ok((ini, m1)) = Initiator::mailbox(&k, &hs, &up.relay, None) else {
            return;
        };
        up.mb_keying = Some(ini);
        self.send_json(&json!({"event": "mailbox_key", "msg": b64(&m1)}));
    }

    fn mailbox_keyed(&mut self, v: &Value) {
        let Some(ini) = self.up().and_then(|u| u.mb_keying.take()) else {
            return;
        };
        let Some(m2) = v["msg"].as_str().and_then(unb64) else {
            return;
        };
        let Ok(mut m) = ini.finish(&m2).and_then(|e| e.into_mailbox()) else {
            return; // keyed again on the next live session
        };
        let Ok(f) = stream::fragment(CONTROL, br#"{"event":"mailbox_ready"}"#) else {
            return;
        };
        let Ok(first) = m.encrypt(&f[0]) else { return };
        let Ok(export) = m.export() else { return };
        let mut framed = m.id().to_vec();
        framed.extend_from_slice(&first);
        self.mailbox = Some(m);
        if let Some(r) = &mut self.rec {
            r.mailbox = Some(b64(&export));
            r.mailbox_seen = None;
        }
        // Saved before the frame that advanced it leaves.
        self.save();
        self.send_json(&json!({"event": "mailbox_ready", "frame": b64(&framed)}));
    }

    fn discard_mailbox(&mut self) {
        self.mailbox = None;
        if let Some(r) = &mut self.rec {
            r.mailbox = None;
            r.mailbox_seen = None;
            for e in r.outbox.entries.iter_mut() {
                e.queued_boot = None;
            }
        }
        self.save();
    }

    /// Send what may go by mailbox: one frame at a time, a refused one
    /// first.
    fn pump_mailbox(&mut self) {
        let now = self.now.mono;
        let wall = self.now.wall;
        let Some(up) = self.up() else { return };
        if up.role != Role::Device || up.host_online || up.mb_inflight.is_some() {
            return;
        }
        let (link, boot, tried) = (up.id, up.boot.clone(), up.refused_tried);
        if let Some((id, bytes, at)) = self.refused.clone() {
            if tried && now < at + REFUSED_RETRY_MS {
                return;
            }
            let up = self.up().expect("up");
            up.refused_tried = true;
            up.mb_inflight = Some((id.clone(), bytes.clone(), now));
            self.refused = Some((id, bytes.clone(), now));
            self.out.push_back(Action::Binary {
                link,
                bytes: FromDevice::Mailbox(bytes).encode(),
            });
            return;
        }
        let Some(r) = &mut self.rec else { return };
        let Some(e) = r.outbox.entries.iter_mut().find(|e| {
            e.open() && outbox::may_wait(&e.op) && e.queued_boot.as_deref() != Some(&boot)
        }) else {
            return;
        };
        let Some(m) = &mut self.mailbox else { return };
        let body = e.request().to_string();
        let Ok(f) = stream::fragment(CONTROL, body.as_bytes()) else {
            return;
        };
        if f.len() != 1 {
            return; // too large for one mailbox frame: waits for a session
        }
        let Ok(frame) = m.encrypt(&f[0]) else { return };
        let Ok(export) = m.export() else { return };
        let mut p = m.id().to_vec();
        p.extend_from_slice(&frame);
        let id = e.id.clone();
        e.first_sent.get_or_insert(wall);
        r.mailbox = Some(b64(&export));
        // Advanced, sealed and saved before the frame leaves.
        self.save();
        let up = self.up().expect("up");
        up.mb_inflight = Some((id, p.clone(), now));
        self.out.push_back(Action::Binary {
            link,
            bytes: FromDevice::Mailbox(p).encode(),
        });
    }

    fn mailbox_frame(&mut self, seq: u64, payload: &[u8]) {
        let Some(up) = self.up() else { return };
        up.queued_left = up.queued_left.saturating_sub(1);
        let boot = up.boot.clone();
        let ack = |m: &mut Machine| {
            m.send_text(|n| ToRelay::Ack {
                n,
                seq,
                device: None,
            })
        };
        let seen = self.rec.as_ref().and_then(|r| r.mailbox_seen.clone());
        if seen.is_some_and(|(b, s)| b == boot && seq <= s) {
            return ack(self); // delivered again: opened already
        }
        let Some(m) = &mut self.mailbox else {
            return ack(self); // for a mailbox this device no longer has
        };
        if payload.len() < 8 || payload[..8] != m.id() {
            return ack(self); // for a replaced mailbox
        }
        let opened = m
            .decrypt(&payload[8..])
            .ok()
            .and_then(|p| stream::single(&p).ok())
            .and_then(|msg| serde_json::from_slice::<Value>(&msg.body).ok());
        let Some(v) = opened else {
            // A replayed, skipped-too-far or forged frame: the mailbox is
            // discarded and keyed again on the next live session.
            self.discard_mailbox();
            return ack(self);
        };
        let export = m.export().ok();
        if let Some(r) = &mut self.rec {
            r.mailbox = export.map(|e| b64(&e));
            r.mailbox_seen = Some((boot, seq));
        }
        match v["id"].as_str().map(str::to_string) {
            Some(rid) => self.answer(&rid, v),
            None => self.save(),
        }
        // `answer` saved when it recorded something; make sure the
        // mailbox state is on disk before the relay forgets the frame.
        self.save();
        ack(self);
    }

    // ---- the page ----

    fn page(&mut self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if let Some(app) = v["app"].as_str() {
            return match app {
                "ready" => self.page_ready(),
                "status" => {
                    let s = self.status();
                    self.send_page(s);
                }
                "pair_continue" => {
                    let name = v["name"].as_str().unwrap_or("").to_string();
                    self.pair_continue(name);
                }
                "pair_cancel" => {
                    if self.pairing.is_some() {
                        self.fail_pairing("failed", "cancelled");
                    }
                    self.scanned = None;
                }
                "unpair" => self.unpair(),
                "cancel" => {
                    let id = v["request"].as_str().unwrap_or("").to_string();
                    self.cancel(&id);
                }
                _ => {}
            };
        }
        let id = v["id"].clone();
        let Some(op) = v["op"].as_str().map(str::to_string) else {
            if !id.is_null() {
                self.send_page(page_error(&id, "bad_request", "no op"));
            }
            return;
        };
        let args = v.get("args").cloned().unwrap_or(json!({}));
        self.request(id, &op, args);
    }

    fn page_ready(&mut self) {
        // A new page: its numbers mean nothing to the old one's requests.
        self.owed.clear();
        self.emit_conn(true);
        if let Some(p) = self.pairing_event.clone() {
            self.send_page(p);
        }
        self.emit_outbox();
        if self.live() {
            if let Some(h) = self.last_hello.clone() {
                self.send_page(h);
            }
        }
    }

    fn no_session_error(&self) -> (&'static str, &'static str) {
        match &self.link {
            Link::Up(up) if up.role == Role::Device && !up.host_online => {
                ("host_offline", "the computer is offline")
            }
            _ => ("disconnected", "not connected to the computer"),
        }
    }

    fn request(&mut self, pid: Value, op: &str, args: Value) {
        if self.rec.is_none() {
            return self.send_page(page_error(&pid, "disconnected", "not paired"));
        }
        let live = self.live();
        if outbox::is_lookup(op) {
            if !live {
                let (c, m) = self.no_session_error();
                return self.send_page(page_error(&pid, c, m));
            }
            let rid = random_id();
            self.owed.insert(rid.clone(), Owed::Page(pid));
            return self.send_json(&json!({"id": rid, "op": op, "args": args}));
        }
        if !live && !outbox::may_wait(op) {
            let (c, m) = self.no_session_error();
            return self.send_page(page_error(&pid, c, m));
        }
        let rid = random_id();
        let mut e = Entry::new(rid.clone(), op, args, self.now.wall);
        if live {
            e.state = State::Sent;
            e.first_sent = Some(self.now.wall);
        }
        let req = e.request();
        let r = self.rec.as_mut().expect("paired");
        if r.outbox.add(e).is_err() {
            return self.send_page(page_error(
                &pid,
                "outbox_full",
                "16 messages are waiting already; cancel one or wait for the computer",
            ));
        }
        self.owed.insert(rid, Owed::Page(pid));
        // Saved (with its id) before it is sent.
        self.save();
        if live {
            self.send_json(&req);
        }
        self.emit_outbox();
    }

    fn cancel(&mut self, id: &str) {
        let Some(r) = &mut self.rec else { return };
        if r.outbox.remove(id).is_none() {
            return;
        }
        self.save();
        if let Some(Owed::Page(pid)) = self.owed.remove(id) {
            self.send_page(page_error(&pid, "cancelled", "cancelled"));
        }
        self.emit_outbox();
    }

    // ---- pairing ----

    fn scanned(&mut self, qr: String, name: String) {
        match QrPayload::decode(qr.trim()) {
            Ok(p) => {
                let origin = p.relay.origin.clone();
                self.scanned = Some((qr.trim().to_string(), name.clone()));
                self.pairing_ev(json!({
                    "event": "pairing", "state": "scanned", "relay": origin, "name": clip(&name),
                }));
            }
            Err(_) => self.pairing_ev(json!({
                "event": "pairing", "state": "failed",
                "reason": "that is not an rlord pairing code",
            })),
        }
    }

    fn pair_continue(&mut self, name: String) {
        let Some((qr, default)) = self.scanned.take() else {
            return self.pairing_ev(json!({
                "event": "pairing", "state": "failed", "reason": "scan the code first",
            }));
        };
        if self.rec.is_some() {
            return self.pairing_ev(json!({
                "event": "pairing", "state": "failed",
                "reason": "this phone is paired already; unpair it first",
            }));
        }
        let Ok(qr) = QrPayload::decode(&qr) else {
            return;
        };
        let name = clip(if name.trim().is_empty() {
            &default
        } else {
            &name
        });
        let label = store::label_for(&qr.host_static);
        let public = match self.signers.create(label.clone()) {
            Ok(p) => p,
            Err(e) => {
                return self.pairing_ev(json!({
                    "event": "pairing", "state": "failed",
                    "reason": format!("could not make a key: {e}"),
                }))
            }
        };
        let Ok(static_key) = StaticKey::generate() else {
            return;
        };
        self.stop = None;
        self.stop_detail = None;
        self.drop_link(1000);
        let pin = RelayAddress {
            origin: qr.relay.origin.clone(),
            fingerprint: qr.relay.fingerprint,
        };
        let target = qr.rendezvous_id.to_vec();
        self.pairing = Some(Pairing {
            qr,
            name,
            static_key,
            label: label.clone(),
            public,
            until: self.now.mono + 2 * HANDSHAKE_MS,
        });
        let id = self.next_link;
        self.next_link += 1;
        self.link = Link::Dialing {
            id,
            role: Role::Pair,
        };
        self.out.push_back(Action::Dial {
            link: id,
            pin,
            role: Role::Pair,
            target,
            label,
        });
        self.pairing_ev(json!({"event": "pairing", "state": "connecting"}));
    }

    fn pair_start(&mut self) {
        let Some(p) = &self.pairing else {
            return self.drop_link(1000);
        };
        let Some(up) = (match &mut self.link {
            Link::Up(up) => Some(up),
            _ => None,
        }) else {
            return;
        };
        match Initiator::pair(&p.qr, &p.static_key, &up.relay, None) {
            Ok((ini, m1)) => {
                up.sess = Sess::PairHs(ini);
                self.send_live_raw(m1);
            }
            Err(_) => self.fail_pairing("failed", "could not start the pairing"),
        }
    }

    fn pair_msg2(&mut self, ini: Initiator, m2: &[u8]) {
        let Ok(est) = ini.finish(m2) else {
            return self.fail_pairing(
                "failed",
                "the pairing handshake failed (a wrong or reused code)",
            );
        };
        let code = format!("{:06}", est.short_code());
        let Ok(s) = est.into_session() else { return };
        let Some(p) = &self.pairing else { return };
        let first = json!({"event": "pair", "name": p.name, "key": b64(&p.public)});
        if let Some(up) = self.up() {
            up.sess = Sess::Pair {
                s,
                reasm: Reassembler::new(),
                code,
            };
        }
        self.send_json(&first);
    }

    fn pair_event(&mut self, code: &str, v: Value) {
        if v["event"] != "pair" {
            return;
        }
        match v["state"].as_str() {
            Some("waiting") => {
                if v["code"].as_str() != Some(code) {
                    return self.fail_pairing("failed", "the two codes differ: not paired");
                }
                if let Some(p) = &mut self.pairing {
                    p.until = self.now.mono + PAIR_CONFIRM_MS;
                }
                self.pairing_ev(json!({"event": "pairing", "state": "waiting", "code": code}));
            }
            Some("done") => self.pair_done(&v),
            _ => self.fail_pairing("rejected", "the computer did not confirm this phone"),
        }
    }

    fn pair_done(&mut self, v: &Value) {
        let Some(p) = self.pairing.take() else { return };
        let Ok(fp) = fingerprint_from_b64(v["host_key"].as_str().unwrap_or("")) else {
            self.pairing = Some(p);
            return self.fail_pairing("failed", "the computer's answer was malformed");
        };
        let pin = RelayAddress {
            origin: p.qr.relay.origin.clone(),
            fingerprint: p.qr.relay.fingerprint,
        };
        let r = Record {
            v: RECORD_VERSION,
            device_name: p.name.clone(),
            static_key: b64(p.static_key.private()),
            label: p.label.clone(),
            host: HostRecord {
                static_key: b64(&p.qr.host_static),
                pin: pin.to_string(),
                fp: b64_fingerprint(&fp),
                name: v["host"].as_str().unwrap_or("").to_string(),
            },
            mailbox: None,
            mailbox_seen: None,
            boot: None,
            rev: None,
            outbox: Outbox::default(),
            notified: Vec::new(),
        };
        self.drop_link(1000);
        self.adopt(r);
        self.save();
        self.attempt = 0;
        self.replaced = false;
        self.dial_now();
        self.pairing_ev(json!({"event": "pairing", "state": "done"}));
        self.emit_outbox();
    }

    fn fail_pairing(&mut self, state: &str, reason: &str) {
        let Some(p) = self.pairing.take() else { return };
        self.signers.delete(p.label);
        if matches!(&self.link, Link::Up(u) if u.role == Role::Pair)
            || matches!(
                &self.link,
                Link::Dialing {
                    role: Role::Pair,
                    ..
                }
            )
        {
            let link = std::mem::replace(&mut self.link, Link::Idle);
            let id = match link {
                Link::Up(u) => u.id,
                Link::Dialing { id, .. } => id,
                Link::Idle => 0,
            };
            self.out.push_back(Action::Close {
                link: id,
                code: 1000,
            });
        }
        self.pairing_ev(json!({"event": "pairing", "state": state, "reason": reason}));
    }

    // ---- unpairing ----

    fn unpair(&mut self) {
        if self.rec.is_none() {
            return;
        }
        if self.live() {
            let rid = random_id();
            self.leaving = Some((rid.clone(), self.now.mono + LEAVE_MS));
            self.send_json(&json!({"id": rid, "op": "device.leave", "args": {}}));
        } else {
            self.finish_unpair(false);
        }
    }

    fn finish_unpair(&mut self, left: bool) {
        self.leaving = None;
        let name = self.rec.as_ref().map(|r| r.host.name.clone());
        self.forget();
        self.stop = None;
        self.stop_detail = Some(if left {
            format!(
                "unpaired: {} has removed this phone",
                name.as_deref().unwrap_or("the computer")
            )
        } else {
            "unpaired here; also revoke this phone on the computer (Devices)".into()
        });
        // (The detail stays until a pairing starts.)
        self.emit_conn(true);
    }

    fn wipe(&mut self) {
        if let Some(p) = self.pairing.take() {
            self.signers.delete(p.label);
        }
        self.forget();
        self.scanned = None;
        self.pairing_event = None;
        self.stop = None;
        self.stop_detail = None;
        if self.bg.is_some() {
            self.bg_done(BackgroundOutcome::NotPaired);
        }
    }

    // ---- lifecycle and network ----

    fn lifecycle(&mut self, fg: bool) {
        let was = self.foreground;
        self.foreground = fg;
        if fg {
            // The user is here: one fresh attempt for what may have healed.
            if matches!(self.stop, Some(Stop::Suspended | Stop::UpdateHost)) {
                self.stop = None;
                self.stop_detail = None;
            }
            self.replaced = false;
            self.attempt = 0;
            let silent = match &self.link {
                Link::Up(up) => self.now.mono.saturating_sub(up.last_rx) >= self.tuning.ping_ms,
                _ => false,
            };
            if silent {
                self.drop_link(1000);
            }
            if !was || silent || matches!(self.link, Link::Idle) {
                self.dial_now();
            }
        } else if self.bg.is_none() && self.leaving.is_none() {
            // Going to the background: what is queued goes, then a clean
            // close. No socket is held in the background.
            if matches!(&self.link, Link::Up(u) if u.role == Role::Device)
                || matches!(
                    &self.link,
                    Link::Dialing {
                        role: Role::Device,
                        ..
                    }
                )
            {
                self.drop_link(1000);
            }
            self.dial_at = None;
        }
    }

    fn network(&mut self, online: bool) {
        let was = self.online;
        self.online = online;
        if !online {
            if !matches!(&self.link, Link::Up(u) if u.role == Role::Pair) {
                self.drop_link(1000);
            }
            self.dial_at = None;
            if self.bg.is_some() {
                self.bg_done(BackgroundOutcome::Failed {
                    reason: "no network".into(),
                });
            }
            return;
        }
        // Back, or a changed path: the old socket may be dead.
        if was && matches!(&self.link, Link::Up(u) if u.role == Role::Device) {
            self.drop_link(1000);
        }
        self.attempt = 0;
        self.dial_now();
    }

    // ---- the background check ----

    fn background(&mut self, budget: u64) {
        if self.rec.is_none() {
            self.out
                .push_back(Action::BackgroundDone(BackgroundOutcome::NotPaired));
            return;
        }
        if !self.online || self.stop.is_some() {
            let reason = match self.stop {
                Some(s) => s.state().to_string(),
                None => "no network".into(),
            };
            self.out
                .push_back(Action::BackgroundDone(BackgroundOutcome::Failed { reason }));
            return;
        }
        self.bg = Some(Bg {
            until: self.now.mono + budget,
            need_session: false,
            snapshot: None,
            checked: false,
        });
        if self.live() {
            // Already live (in the foreground): refresh on this session.
            let rev = self
                .last_hello
                .as_ref()
                .and_then(|h| h["rev"].as_str())
                .map(str::to_string);
            if let Some(b) = &mut self.bg {
                b.need_session = true;
            }
            self.bg_live(rev);
        } else if matches!(self.link, Link::Idle) {
            self.dial_now();
        }
    }

    /// Live in a background check: fetch the snapshot when `rev` moved.
    fn bg_live(&mut self, rev: Option<String>) {
        let saved = self.rec.as_ref().and_then(|r| r.rev.clone());
        if rev.is_some() && rev == saved {
            if let Some(b) = &mut self.bg {
                b.checked = true;
            }
            return self.bg_maybe_done();
        }
        let rid = random_id();
        if let Some(b) = &mut self.bg {
            b.snapshot = Some(rid.clone());
        }
        self.send_json(&json!({"id": rid, "op": "snapshot", "args": {}}));
    }

    fn bg_snapshot(&mut self, v: &Value) {
        if let Some(b) = &mut self.bg {
            b.snapshot = None;
            b.checked = true;
        }
        let Some(ok) = v.get("ok") else {
            return self.bg_done(BackgroundOutcome::Failed {
                reason: "the snapshot failed".into(),
            });
        };
        let waiting: Vec<String> = ok["rows"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|r| r["shown_status"] == "waiting_input")
                    .filter_map(|r| r["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let r = self.rec.as_mut().expect("paired");
        let new = waiting.iter().filter(|w| !r.notified.contains(w)).count() as u32;
        r.notified = waiting.clone();
        r.rev = ok["rev"].as_str().map(str::to_string).or(r.rev.take());
        self.save();
        if new > 0 {
            self.out.push_back(Action::Notify {
                kind: "waiting".into(),
                count: new,
            });
        }
        if self.signal_pending {
            self.signal_pending = false;
            self.send_text(|n| ToRelay::AckSignal { n });
        }
        if let Some(b) = &mut self.bg {
            b.need_session = false;
        }
        self.bg_result = Some(BackgroundOutcome::Refreshed {
            waiting: waiting.len() as u32,
            notified: new,
        });
        self.bg_maybe_done();
    }

    fn bg_maybe_done(&mut self) {
        let Some(bg) = &self.bg else { return };
        if !bg.checked {
            return;
        }
        let left = match &self.link {
            Link::Up(up) => up.queued_left > 0 || up.mb_inflight.is_some(),
            _ => false,
        };
        if left {
            return;
        }
        let outcome = self.bg_result.take().unwrap_or_else(|| {
            let waiting = self.rec.as_ref().map_or(0, |r| r.notified.len() as u32);
            BackgroundOutcome::Refreshed {
                waiting,
                notified: 0,
            }
        });
        if self.signal_pending && self.live() {
            self.signal_pending = false;
            self.send_text(|n| ToRelay::AckSignal { n });
        }
        self.bg_done(outcome);
    }

    fn bg_done(&mut self, outcome: BackgroundOutcome) {
        if self.bg.take().is_none() {
            return;
        }
        self.bg_result = None;
        if !self.foreground && self.leaving.is_none() {
            self.drop_link(1000);
            self.dial_at = None;
        }
        self.out.push_back(Action::BackgroundDone(outcome));
    }

    // ---- time ----

    fn tick(&mut self) {
        let now = self.now;
        if let Some(prev) = self.last_tick {
            let dm = now.mono.saturating_sub(prev.mono);
            let dw = now.wall as i128 - prev.wall as i128;
            let jumped = dm >= JUMP_MS || (dw - dm as i128).unsigned_abs() >= JUMP_MS as u128;
            if jumped && matches!(&self.link, Link::Up(u) if u.role == Role::Device) {
                // The phone slept (or the clock moved): the socket is
                // likely dead. Redial at once.
                self.drop_link(1000);
                self.dial_now();
            }
        }
        self.last_tick = Some(now);
        let mono = now.mono;
        if let Some((_, until)) = &self.leaving {
            if mono >= *until {
                self.finish_unpair(false);
            }
        }
        if let Some(p) = &self.pairing {
            if mono >= p.until {
                self.fail_pairing("failed", "timed out");
            }
        }
        if let Some(bg) = &self.bg {
            if mono >= bg.until {
                self.bg_done(BackgroundOutcome::Failed {
                    reason: "out of time".into(),
                });
            }
        }
        let mut give_up = false;
        let mut hs_failed = false;
        let mut ping = None;
        if let Link::Up(up) = &mut self.link {
            if mono.saturating_sub(up.last_rx) >= self.tuning.silence_ms {
                give_up = true;
            } else {
                if mono.saturating_sub(up.since) >= STABLE_MS && !self.replaced {
                    self.attempt = 0;
                }
                if mono.saturating_sub(up.last_ping) >= self.tuning.ping_ms {
                    up.last_ping = mono;
                    ping = Some(up.id);
                }
                match &up.sess {
                    Sess::Hs { until, .. } | Sess::Hello { until, .. } if mono >= *until => {
                        hs_failed = true
                    }
                    _ => {}
                }
                if let Some((_, _, sent)) = &up.mb_inflight {
                    if mono >= sent + MAILBOX_ANSWER_MS {
                        // The relay never answered `queued` or `refused`.
                        give_up = true;
                    }
                }
            }
        }
        if let Some(link) = ping {
            self.out.push_back(Action::Ping { link });
        }
        if give_up {
            self.drop_link(1000);
            self.schedule_backoff();
        } else if hs_failed {
            self.handshake_failed();
        }
    }

    /// What every input ends with: dial or close as wanted, start a
    /// handshake, send by mailbox, end what waited too long, and tell the
    /// page what changed.
    fn after(&mut self) {
        let mono = self.now.mono;
        let want = self.want_device_link();
        match &self.link {
            Link::Idle if want => {
                let at = *self.dial_at.get_or_insert(mono);
                if mono >= at {
                    self.dial_device();
                }
            }
            Link::Up(u) if u.role == Role::Device && !want => self.drop_link(1000),
            Link::Dialing {
                role: Role::Device, ..
            } if !want => self.drop_link(1000),
            _ => {}
        }
        let want_session = self.want_session();
        if let Link::Up(up) = &self.link {
            if up.role == Role::Device {
                if up.host_online
                    && want_session
                    && matches!(up.sess, Sess::None)
                    && mono >= up.hs_retry_at
                {
                    self.start_handshake();
                }
                self.pump_mailbox();
                if self.bg.as_ref().is_some_and(|b| !b.checked) {
                    let offline = matches!(&self.link, Link::Up(u) if !u.host_online);
                    if offline {
                        if let Some(b) = &mut self.bg {
                            b.checked = true;
                        }
                        self.bg_result = Some(BackgroundOutcome::HostOffline);
                    }
                }
                self.bg_maybe_done();
            }
        }
        let running_live = self.live();
        let ended = self
            .rec
            .as_mut()
            .map(|r| r.outbox.expire(self.now.wall, running_live))
            .unwrap_or_default();
        if !ended.is_empty() {
            for id in &ended {
                if let Some(Owed::Page(pid)) = self.owed.remove(id) {
                    // The page hears of it; the entry goes.
                    let e = self.rec.as_mut().and_then(|r| r.outbox.remove(id));
                    if let Some(e) = e {
                        let err = e.error.unwrap_or(ErrorInfo {
                            code: "interrupted".into(),
                            message: String::new(),
                        });
                        self.send_page(page_error(&pid, &err.code, &err.message));
                    }
                }
            }
            self.save();
            self.emit_outbox();
        }
        self.emit_conn(false);
    }
}

/// Decrypt a live frame and reassemble: a whole control message, nothing
/// yet, or an error (a decrypt failure, a non-control stream, not JSON).
fn open(s: &mut Session, reasm: &mut Reassembler, p: &[u8]) -> Result<Option<Value>, ()> {
    let plain = s.decrypt(p).map_err(|_| ())?;
    match reasm.push(&plain).map_err(|_| ())? {
        None => Ok(None),
        Some(m) if m.stream != CONTROL => Err(()),
        Some(m) => serde_json::from_slice(&m.body).map(Some).map_err(|_| ()),
    }
}

fn clip(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(NAME_MAX)
        .collect::<String>()
        .trim()
        .to_string()
}
