//! The relay's memory: who is connected, links, rendezvous, signal flags
//! and mailbox queues. None of it is ever written to disk; a restart
//! forgets it (and `ready.boot` changes, which a device reads as "fetch").
//!
//! Every operation takes the one lock briefly and never awaits under it.
//! What must wait (a device's live frame for a host whose share for that
//! link is full) is done by the connection task after the lock is released
//! (`conn.rs`).
//!
//! **Flow control is per link.** A device's frames to its host take one of
//! that link's [`LINK_FRAMES`] permits, so one device can fill only its own
//! share of the host's buffer; the device's reader waits for room, and a
//! link that stays full for `stall` closes the device ([`close::RESET`]),
//! never the host. A host's frames to a device take one of the device
//! connection's [`DEVICE_FRAMES`] permits without waiting: the host's
//! connection carries all its links, so it is never held up by one; a
//! device whose buffer is full is closed ([`close::RESET`]) and the host told
//! `undeliverable`. Nothing is dropped silently: a link that can't keep up
//! is closed, and both ends start a fresh session.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rlord_relay_proto::frame::{FromDevice, FromHost, ToDevice, ToHost};
use rlord_relay_proto::msg::{close, code, FromRelay, HostState, LinkKind, Ready, ToRelay};
use rlord_relay_proto::{short, Identity, Role, B64};
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};

use crate::config::Config;
use crate::meter::{Meter, Tally};
use crate::policy::{Authorizer, Limits};
use crate::registry::{Fp, Registry, Status};

/// Live frames a device's (or pairing device's) connection buffers from
/// its host; one more closes it. Room for a whole 1 MiB control message
/// (about 65 frames) and more.
pub const DEVICE_FRAMES: usize = 128;
/// Live frames one link may have waiting in its host's buffer; its device
/// is not read while they are all taken.
pub const LINK_FRAMES: usize = 64;
/// Control messages queued to a peer that doesn't read before it is closed.
const MAX_PENDING_TEXT: usize = 1024;

pub type RendezvousId = [u8; 16];

/// What a connection's writer sends.
pub enum Out {
    Text(String),
    /// A binary frame; a live frame holds one permit (the link's, or the
    /// device connection's) until it is written.
    Frame(Vec<u8>, Option<OwnedSemaphorePermit>),
    Close(u16, &'static str),
}

/// The sending side of one connection.
#[derive(Clone)]
pub struct Outbox {
    pub id: u64,
    tx: mpsc::UnboundedSender<Out>,
    pub permits: Arc<Semaphore>,
    pending: Arc<AtomicUsize>,
    kill: Arc<watch::Sender<bool>>,
}

pub struct OutboxEnds {
    pub rx: mpsc::UnboundedReceiver<Out>,
    pub pending: Arc<AtomicUsize>,
    pub kill: watch::Receiver<bool>,
}

impl Outbox {
    pub fn new(id: u64) -> (Outbox, OutboxEnds) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (kill_tx, kill_rx) = watch::channel(false);
        let pending = Arc::new(AtomicUsize::new(0));
        (
            Outbox {
                id,
                tx,
                permits: Arc::new(Semaphore::new(DEVICE_FRAMES)),
                pending: pending.clone(),
                kill: Arc::new(kill_tx),
            },
            OutboxEnds {
                rx,
                pending,
                kill: kill_rx,
            },
        )
    }

    pub fn send(&self, m: &FromRelay) {
        if self.pending.fetch_add(1, Ordering::Relaxed) >= MAX_PENDING_TEXT {
            self.kill(close::TOO_SLOW, "not reading", None);
            return;
        }
        let _ = self.tx.send(Out::Text(m.to_json()));
    }

    pub fn frame(&self, b: Vec<u8>, permit: Option<OwnedSemaphorePermit>) {
        let _ = self.tx.send(Out::Frame(b, permit));
    }

    /// Stops the connection's reader (the writer has ended).
    pub fn kill_quietly(&self) {
        self.kill.send_replace(true);
    }

    /// Closes the connection: `bye` first when given (`code`, seconds to
    /// wait), then the close frame; the connection's reader stops.
    pub fn kill(&self, close_code: u16, reason: &'static str, bye: Option<(&str, Option<u64>)>) {
        if let Some((c, retry_after)) = bye {
            let _ = self.tx.send(Out::Text(
                FromRelay::Bye {
                    code: c.to_string(),
                    retry_after,
                }
                .to_json(),
            ));
            self.pending.fetch_add(1, Ordering::Relaxed);
        }
        let _ = self.tx.send(Out::Close(close_code, reason));
        self.kill.send_replace(true);
    }
}

/// An admitted connection.
pub struct Peer {
    pub role: Role,
    pub fp: Fp,
    /// The host it belongs to (itself for a host).
    pub host: Fp,
    pub rendezvous: Option<RendezvousId>,
    pub account: String,
    pub limits: Limits,
    pub out: Outbox,
    pub tally: Tally,
}

struct LinkEnd {
    device: Fp,
    kind: LinkKind,
    out: Outbox,
    /// This link's share of the host's buffer ([`LINK_FRAMES`]).
    to_host: Arc<Semaphore>,
}

impl LinkEnd {
    /// Whether this link belongs to the connection `conn` of `device` (in
    /// role `kind`). A link id a connection remembers may have been given
    /// to someone else since (its host reconnected), so every use of a
    /// remembered id checks it is still that connection's.
    fn is(&self, device: &Fp, kind: LinkKind, conn: u64) -> bool {
        self.device == *device && self.kind == kind && self.out.id == conn
    }
}

struct HostConn {
    out: Outbox,
    links: HashMap<u32, LinkEnd>,
}

struct DeviceConn {
    out: Outbox,
    host: Fp,
    link: Option<u32>,
}

struct PairConn {
    out: Outbox,
    host: Fp,
    rendezvous: RendezvousId,
    link: Option<u32>,
}

struct Rendezvous {
    host: Fp,
    expires: Instant,
    attempts: u32,
    max_attempts: u32,
    active: Option<u64>,
}

struct Queued {
    seq: u64,
    payload: Vec<u8>,
    expires: Instant,
}

#[derive(Default)]
struct Queue {
    next_seq: u64,
    msgs: VecDeque<Queued>,
    bytes: usize,
}

impl Queue {
    fn expire(&mut self, now: Instant) {
        while self.msgs.front().is_some_and(|m| m.expires <= now) {
            let m = self.msgs.pop_front().unwrap();
            self.bytes -= m.payload.len();
        }
    }

    fn ack(&mut self, seq: u64) {
        while self.msgs.front().is_some_and(|m| m.seq <= seq) {
            let m = self.msgs.pop_front().unwrap();
            self.bytes -= m.payload.len();
        }
    }
}

/// (host, device).
type Pair = (Fp, Fp);

#[derive(Default)]
struct Inner {
    hosts: HashMap<Fp, HostConn>,
    devices: HashMap<Fp, DeviceConn>,
    pairs: HashMap<Fp, PairConn>,
    rendezvous: HashMap<RendezvousId, Rendezvous>,
    flags: HashSet<Pair>,
    to_device: HashMap<Pair, Queue>,
    to_host: HashMap<Pair, Queue>,
    unauth: HashMap<IpAddr, usize>,
    /// The last link id given. One counter for the whole relay, so a host
    /// that reconnects never sees an id its last connection used (a stale
    /// id held by a dying connection can't name a new link).
    next_link: u32,
}

impl Inner {
    /// Gives a host and one of its peers a link, and tells the host.
    fn link(
        &mut self,
        host: &Fp,
        device: &Fp,
        kind: LinkKind,
        out: Outbox,
        rendezvous: Option<RendezvousId>,
    ) -> Option<u32> {
        let h = self.hosts.get_mut(host)?;
        let id = loop {
            self.next_link = self.next_link.wrapping_add(1).max(1);
            if !h.links.contains_key(&self.next_link) {
                break self.next_link;
            }
        };
        h.out.send(&FromRelay::Link {
            id,
            device: B64(device.to_vec()),
            kind,
            rendezvous: rendezvous.map(|r| B64(r.to_vec())),
        });
        h.links.insert(
            id,
            LinkEnd {
                device: *device,
                kind,
                out,
                to_host: Arc::new(Semaphore::new(LINK_FRAMES)),
            },
        );
        Some(id)
    }

    /// Ends link `id` of `host` if it is still the one of `device`'s
    /// connection `conn` (as [`LinkEnd::is`]), and tells the host.
    fn unlink(&mut self, host: &Fp, id: u32, device: &Fp, kind: LinkKind, conn: u64) {
        if let Some(h) = self.hosts.get_mut(host) {
            if h.links.get(&id).is_some_and(|l| l.is(device, kind, conn)) {
                h.links.remove(&id);
                h.out.send(&FromRelay::Unlink { id });
            }
        }
    }

    /// A host's connection ended or was replaced: its pairing devices'
    /// links went with it (they are being closed).
    fn forget_pair_links(&mut self, host: &Fp) {
        for pc in self.pairs.values_mut().filter(|pc| pc.host == *host) {
            pc.link = None;
        }
    }

    fn queued(&mut self, to_device: bool, pair: &Pair, now: Instant) -> u64 {
        let q = if to_device {
            self.to_device.get_mut(pair)
        } else {
            self.to_host.get_mut(pair)
        };
        q.map_or(0, |q| {
            q.expire(now);
            q.msgs.len() as u64
        })
    }

    /// Forgets a device that left its host's allow-list.
    fn forget_device(&mut self, host: &Fp, device: &Fp) {
        let pair = (*host, *device);
        self.flags.remove(&pair);
        self.to_device.remove(&pair);
        self.to_host.remove(&pair);
        if self.devices.get(device).is_some_and(|d| d.host == *host) {
            let d = self.devices.remove(device).unwrap();
            d.out
                .kill(close::NOT_ALLOWED, "not on the host's list", None);
            if let Some(l) = d.link {
                self.unlink(host, l, device, LinkKind::Device, d.out.id);
            }
        }
    }
}

/// What to do with a binary frame.
pub enum Route {
    Nothing,
    Reply(FromRelay),
    /// A device's live frame: forward `frame` to its host `to` once one of
    /// the link's `permits` is free; if none frees within `stall`, close the
    /// device ([`close::RESET`]).
    ToHost {
        to: Outbox,
        permits: Arc<Semaphore>,
        frame: Vec<u8>,
    },
}

pub struct Hub {
    pub cfg: Config,
    pub identity: Identity,
    pub origin: String,
    pub boot: String,
    pub registry: Registry,
    pub auth: Arc<dyn Authorizer>,
    pub meter: Meter,
    /// Open websocket connections (and upgrades in progress).
    pub conns: AtomicUsize,
    pub shutdown: watch::Receiver<bool>,
    next_id: AtomicU64,
    inner: Mutex<Inner>,
}

fn err(n: u64, c: &str) -> FromRelay {
    FromRelay::Err {
        n,
        code: c.to_string(),
    }
}

/// A lane-2 frame's refusal.
fn refused(c: &str, device: Option<&Fp>) -> FromRelay {
    FromRelay::Refused {
        code: c.to_string(),
        device: device.map(|d| B64(d.to_vec())),
    }
}

fn fp_of(b: &B64) -> Option<Fp> {
    b.as_array()
}

impl Hub {
    pub fn new(
        cfg: Config,
        identity: Identity,
        origin: String,
        boot: String,
        registry: Registry,
        auth: Arc<dyn Authorizer>,
        shutdown: watch::Receiver<bool>,
    ) -> Hub {
        Hub {
            cfg,
            identity,
            origin,
            boot,
            registry,
            auth,
            meter: Meter::default(),
            conns: AtomicUsize::new(0),
            shutdown,
            next_id: AtomicU64::new(1),
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Counts an unauthenticated connection from `ip`; `None` over the
    /// limit.
    pub fn unauth_enter(self: &Arc<Self>, ip: IpAddr) -> Option<UnauthGuard> {
        let mut g = self.lock();
        let n = g.unauth.entry(ip).or_insert(0);
        if *n >= self.cfg.unauthenticated_per_address {
            return None;
        }
        *n += 1;
        Some(UnauthGuard {
            hub: self.clone(),
            ip,
        })
    }

    /// The host of an open rendezvous.
    pub fn rendezvous_host(&self, id: &RendezvousId) -> Option<Fp> {
        let now = Instant::now();
        self.lock()
            .rendezvous
            .get(id)
            .filter(|r| r.expires > now)
            .map(|r| r.host)
    }

    fn ready(&self) -> Ready {
        Ready {
            boot: self.boot.clone(),
            host: None,
            signal: None,
            queued: None,
            limits: None,
        }
    }

    /// Admission checks that need the live state, then registration: the
    /// peer gets `ready` and (for a device) its queue; its host gets `link`.
    /// A close code when it can't be admitted.
    pub fn register(&self, p: &Peer) -> Result<(), u16> {
        let now = Instant::now();
        let mut guard = self.lock();
        let g = &mut *guard;
        match p.role {
            Role::Host => {
                let replacing = match g.hosts.remove(&p.fp) {
                    Some(old) => {
                        old.out.kill(close::REPLACED, "replaced", None);
                        for (_, l) in old.links {
                            if l.kind == LinkKind::Pair {
                                l.out.kill(1000, "host reconnected", None);
                            }
                        }
                        true
                    }
                    None => false,
                };
                g.forget_pair_links(&p.fp);
                p.out.send(&FromRelay::Ready(Ready {
                    limits: Some(p.limits.wire()),
                    ..self.ready()
                }));
                g.hosts.insert(
                    p.fp,
                    HostConn {
                        out: p.out.clone(),
                        links: HashMap::new(),
                    },
                );
                let devices: Vec<(Fp, Outbox)> = g
                    .devices
                    .iter()
                    .filter(|(_, d)| d.host == p.fp)
                    .map(|(fp, d)| (*fp, d.out.clone()))
                    .collect();
                for (fp, out) in devices {
                    if replacing {
                        out.send(&FromRelay::Host {
                            state: HostState::Offline,
                        });
                    }
                    out.send(&FromRelay::Host {
                        state: HostState::Online,
                    });
                    let id = g.link(&p.fp, &fp, LinkKind::Device, out, None);
                    if let Some(d) = g.devices.get_mut(&fp) {
                        d.link = id;
                    }
                }
                for ((host, device), q) in g.to_host.iter_mut() {
                    if *host != p.fp {
                        continue;
                    }
                    q.expire(now);
                    for m in &q.msgs {
                        p.out.frame(
                            ToHost::Mailbox {
                                device: *device,
                                seq: m.seq,
                                payload: m.payload.clone(),
                            }
                            .encode(),
                            None,
                        );
                    }
                }
            }
            Role::Device => {
                // Checked again under the lock: an allow-list change that
                // removed this device since `connect` checked it either is
                // seen here, or comes after and closes it (`devices` writes
                // the store before it takes the lock).
                match self.registry.is_device(&p.host, &p.fp) {
                    Ok(true) => {}
                    Ok(false) => return Err(close::NOT_ALLOWED),
                    Err(e) => {
                        tracing::warn!("store: {e:#}");
                        return Err(close::OVERLOADED);
                    }
                }
                if let Some(old) = g.devices.remove(&p.fp) {
                    old.out.kill(close::REPLACED, "replaced", None);
                    if let Some(l) = old.link {
                        g.unlink(&old.host, l, &p.fp, LinkKind::Device, old.out.id);
                    }
                }
                let pair = (p.host, p.fp);
                let online = g.hosts.contains_key(&p.host);
                let queued = g.queued(true, &pair, now);
                p.out.send(&FromRelay::Ready(Ready {
                    host: Some(if online {
                        HostState::Online
                    } else {
                        HostState::Offline
                    }),
                    signal: Some(g.flags.contains(&pair) as u64),
                    queued: Some(queued),
                    ..self.ready()
                }));
                let link = g.link(&p.host, &p.fp, LinkKind::Device, p.out.clone(), None);
                g.devices.insert(
                    p.fp,
                    DeviceConn {
                        out: p.out.clone(),
                        host: p.host,
                        link,
                    },
                );
                if let Some(q) = g.to_device.get(&pair) {
                    for m in &q.msgs {
                        p.out.frame(
                            ToDevice::Mailbox {
                                seq: m.seq,
                                payload: m.payload.clone(),
                            }
                            .encode(),
                            None,
                        );
                    }
                }
            }
            Role::Pair => {
                let rid = p.rendezvous.expect("a pairing peer has a rendezvous");
                if let Some(old) = g.pairs.remove(&p.fp) {
                    old.out.kill(close::REPLACED, "replaced", None);
                    if let Some(l) = old.link {
                        g.unlink(&old.host, l, &p.fp, LinkKind::Pair, old.out.id);
                    }
                    if let Some(r) = g.rendezvous.get_mut(&old.rendezvous) {
                        if r.active == Some(old.out.id) {
                            r.active = None;
                        }
                    }
                }
                let Some(r) = g.rendezvous.get_mut(&rid) else {
                    return Err(close::NOT_ALLOWED);
                };
                if r.expires <= now || r.attempts >= r.max_attempts {
                    g.rendezvous.remove(&rid);
                    return Err(close::NOT_ALLOWED);
                }
                if r.active.is_some() {
                    return Err(close::LIMIT);
                }
                let host = r.host;
                if host != p.host || !g.hosts.contains_key(&host) {
                    return Err(close::NOT_ALLOWED);
                }
                r.attempts += 1;
                r.active = Some(p.out.id);
                p.out.send(&FromRelay::Ready(self.ready()));
                let link = g.link(&host, &p.fp, LinkKind::Pair, p.out.clone(), Some(rid));
                g.pairs.insert(
                    p.fp,
                    PairConn {
                        out: p.out.clone(),
                        host,
                        rendezvous: rid,
                        link,
                    },
                );
            }
        }
        Ok(())
    }

    /// The connection ended: unlink it and tell whoever needs to know.
    pub fn unregister(&self, p: &Peer) {
        let mut g = self.lock();
        match p.role {
            Role::Host => {
                if g.hosts.get(&p.fp).map(|h| h.out.id) != Some(p.out.id) {
                    return;
                }
                let h = g.hosts.remove(&p.fp).unwrap();
                for (_, l) in h.links {
                    match l.kind {
                        LinkKind::Device => {
                            if let Some(d) = g.devices.get_mut(&l.device) {
                                d.link = None;
                                d.out.send(&FromRelay::Host {
                                    state: HostState::Offline,
                                });
                            }
                        }
                        LinkKind::Pair => l.out.kill(1000, "host gone", None),
                    }
                }
                g.forget_pair_links(&p.fp);
                g.rendezvous.retain(|_, r| r.host != p.fp);
                drop(g);
                let _ = self.registry.touch_host(&p.fp);
            }
            Role::Device => {
                if g.devices.get(&p.fp).map(|d| d.out.id) != Some(p.out.id) {
                    return;
                }
                let d = g.devices.remove(&p.fp).unwrap();
                if let Some(l) = d.link {
                    g.unlink(&d.host, l, &p.fp, LinkKind::Device, d.out.id);
                }
            }
            Role::Pair => {
                if g.pairs.get(&p.fp).map(|d| d.out.id) != Some(p.out.id) {
                    return;
                }
                let pc = g.pairs.remove(&p.fp).unwrap();
                if let Some(l) = pc.link {
                    g.unlink(&pc.host, l, &p.fp, LinkKind::Pair, pc.out.id);
                }
                if let Some(r) = g.rendezvous.get_mut(&pc.rendezvous) {
                    if r.active == Some(p.out.id) {
                        r.active = None;
                    }
                    if r.attempts >= r.max_attempts {
                        g.rendezvous.remove(&pc.rendezvous);
                    }
                }
            }
        }
    }

    /// A control message after `ready`; the answer to send back, if any.
    pub fn control(&self, p: &Peer, m: ToRelay) -> FromRelay {
        match (p.role, m) {
            (Role::Host, ToRelay::Devices { n, allow }) => self.devices(p, n, &allow),
            (Role::Host, ToRelay::Rendezvous { n, id, ttl }) => {
                let Some(id) = id.as_array::<16>() else {
                    return err(n, code::BAD);
                };
                if ttl == 0 {
                    return err(n, code::BAD);
                }
                if ttl > p.limits.rendezvous_ttl.as_secs() {
                    return err(n, code::LIMIT);
                }
                let now = Instant::now();
                let mut g = self.lock();
                g.rendezvous.retain(|_, r| r.expires > now);
                let expires = now + Duration::from_secs(ttl);
                if let Some(r) = g.rendezvous.get_mut(&id) {
                    if r.host != p.fp {
                        return err(n, code::BAD);
                    }
                    r.expires = expires;
                    return FromRelay::Ok { n };
                }
                let open = g.rendezvous.values().filter(|r| r.host == p.fp).count();
                if open >= p.limits.rendezvous_per_host {
                    return err(n, code::LIMIT);
                }
                g.rendezvous.insert(
                    id,
                    Rendezvous {
                        host: p.fp,
                        expires,
                        attempts: 0,
                        max_attempts: p.limits.rendezvous_attempts,
                        active: None,
                    },
                );
                FromRelay::Ok { n }
            }
            (Role::Host, ToRelay::RendezvousClose { n, id }) => {
                let Some(id) = id.as_array::<16>() else {
                    return err(n, code::BAD);
                };
                let mut g = self.lock();
                if g.rendezvous.get(&id).is_some_and(|r| r.host == p.fp) {
                    g.rendezvous.remove(&id);
                    FromRelay::Ok { n }
                } else {
                    err(n, code::UNKNOWN)
                }
            }
            (Role::Host, ToRelay::Signal { n, device }) => {
                let Some(device) = fp_of(&device) else {
                    return err(n, code::BAD);
                };
                match self.registry.is_device(&p.fp, &device) {
                    Ok(true) => {}
                    Ok(false) => return err(n, code::UNKNOWN),
                    Err(e) => {
                        tracing::warn!("store: {e:#}");
                        return err(n, code::BAD);
                    }
                }
                let mut g = self.lock();
                g.flags.insert((p.fp, device));
                if let Some(d) = g.devices.get(&device).filter(|d| d.host == p.fp) {
                    d.out.send(&FromRelay::Signal);
                }
                FromRelay::Ok { n }
            }
            (Role::Host, ToRelay::Drop { n, link }) => {
                let g = self.lock();
                match g.hosts.get(&p.fp).and_then(|h| h.links.get(&link)) {
                    Some(l) => {
                        // Its session is gone: the device reconnects at once.
                        l.out.kill(close::RESET, "dropped by the host", None);
                        FromRelay::Ok { n }
                    }
                    None => err(n, code::UNKNOWN),
                }
            }
            (Role::Host, ToRelay::Ack { n, seq, device }) => {
                let Some(device) = device.as_ref().and_then(fp_of) else {
                    return err(n, code::BAD);
                };
                if let Some(q) = self.lock().to_host.get_mut(&(p.fp, device)) {
                    q.ack(seq);
                }
                FromRelay::Ok { n }
            }
            (Role::Device, ToRelay::AckSignal { n }) => {
                self.lock().flags.remove(&(p.host, p.fp));
                FromRelay::Ok { n }
            }
            (
                Role::Device,
                ToRelay::Ack {
                    n,
                    seq,
                    device: None,
                },
            ) => {
                if let Some(q) = self.lock().to_device.get_mut(&(p.host, p.fp)) {
                    q.ack(seq);
                }
                FromRelay::Ok { n }
            }
            (_, m) => err(request_number(&m), code::BAD),
        }
    }

    fn devices(&self, p: &Peer, n: u64, allow: &[B64]) -> FromRelay {
        let mut list = Vec::with_capacity(allow.len());
        for d in allow {
            let Some(fp) = fp_of(d) else {
                return err(n, code::BAD);
            };
            if !list.contains(&fp) {
                list.push(fp);
            }
        }
        if list.len() > p.limits.devices_per_host {
            return err(n, code::LIMIT);
        }
        let removed = match self.registry.set_devices(&p.fp, &list) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("store: {e:#}");
                return err(n, code::BAD);
            }
        };
        let mut g = self.lock();
        for d in &removed {
            tracing::info!(host = %short(&p.fp), device = %short(d), "device removed from the allow-list");
            g.forget_device(&p.fp, d);
        }
        FromRelay::Ok { n }
    }

    /// A binary frame from `p`.
    pub fn binary(&self, p: &Peer, b: &[u8]) -> Route {
        match p.role {
            Role::Host => match FromHost::decode(b) {
                Err(_) => Route::Reply(err(0, code::BAD)),
                Ok(FromHost::Live { link, payload }) => {
                    let g = self.lock();
                    let undeliverable = FromRelay::Undeliverable { link: Some(link) };
                    let Some(l) = g.hosts.get(&p.fp).and_then(|h| h.links.get(&link)) else {
                        return Route::Reply(undeliverable);
                    };
                    // Never wait here: the host's connection carries every
                    // link, and one slow device must not hold up the rest.
                    match l.out.permits.clone().try_acquire_owned() {
                        Ok(permit) => {
                            l.out.frame(ToDevice::Live(payload).encode(), Some(permit));
                            Route::Nothing
                        }
                        Err(_) => {
                            tracing::info!(
                                host = %short(&p.fp),
                                device = %short(&l.device),
                                "a device isn't keeping up; closing its link"
                            );
                            l.out.kill(close::RESET, "stalled", None);
                            Route::Reply(undeliverable)
                        }
                    }
                }
                Ok(FromHost::Mailbox { device, payload }) => {
                    match self.registry.is_device(&p.fp, &device) {
                        Ok(true) => {}
                        Ok(false) => return Route::Reply(refused(code::UNKNOWN, Some(&device))),
                        Err(e) => {
                            tracing::warn!("store: {e:#}");
                            return Route::Reply(refused(code::BAD, Some(&device)));
                        }
                    }
                    let mut g = self.lock();
                    let pair = (p.fp, device);
                    let seq =
                        match enqueue(g.to_device.entry(pair).or_default(), &p.limits, &payload) {
                            Ok(seq) => seq,
                            Err(c) => return Route::Reply(refused(c, Some(&device))),
                        };
                    if let Some(d) = g.devices.get(&device).filter(|d| d.host == p.fp) {
                        d.out
                            .frame(ToDevice::Mailbox { seq, payload }.encode(), None);
                    }
                    Route::Reply(FromRelay::Queued {
                        seq,
                        device: Some(B64(device.to_vec())),
                    })
                }
            },
            Role::Device | Role::Pair => match FromDevice::decode(b) {
                Err(_) => Route::Reply(err(0, code::BAD)),
                Ok(FromDevice::Live(payload)) => {
                    let g = self.lock();
                    let link = if p.role == Role::Device {
                        g.devices
                            .get(&p.fp)
                            .filter(|d| d.out.id == p.out.id)
                            .and_then(|d| d.link)
                    } else {
                        g.pairs
                            .get(&p.fp)
                            .filter(|d| d.out.id == p.out.id)
                            .and_then(|d| d.link)
                    };
                    let undeliverable = FromRelay::Undeliverable { link: None };
                    let kind = if p.role == Role::Device {
                        LinkKind::Device
                    } else {
                        LinkKind::Pair
                    };
                    match link.and_then(|l| {
                        let h = g.hosts.get(&p.host)?;
                        let end = h.links.get(&l).filter(|e| e.is(&p.fp, kind, p.out.id))?;
                        Some((l, h, end))
                    }) {
                        Some((link, h, end)) => Route::ToHost {
                            to: h.out.clone(),
                            permits: end.to_host.clone(),
                            frame: ToHost::Live { link, payload }.encode(),
                        },
                        None => Route::Reply(undeliverable),
                    }
                }
                Ok(FromDevice::Mailbox(payload)) => {
                    if p.role == Role::Pair {
                        return Route::Reply(refused(code::BAD, None));
                    }
                    let mut g = self.lock();
                    // Still this host's device: one taken off its list is
                    // forgotten (under this lock) before its connection
                    // closes, and must not queue anything after that.
                    if !g
                        .devices
                        .get(&p.fp)
                        .is_some_and(|d| d.out.id == p.out.id && d.host == p.host)
                    {
                        return Route::Reply(refused(code::UNKNOWN, None));
                    }
                    let pair = (p.host, p.fp);
                    let seq = match enqueue(g.to_host.entry(pair).or_default(), &p.limits, &payload)
                    {
                        Ok(seq) => seq,
                        Err(c) => return Route::Reply(refused(c, None)),
                    };
                    if let Some(h) = g.hosts.get(&p.host) {
                        h.out.frame(
                            ToHost::Mailbox {
                                device: p.fp,
                                seq,
                                payload,
                            }
                            .encode(),
                            None,
                        );
                    }
                    Route::Reply(FromRelay::Queued { seq, device: None })
                }
            },
        }
    }

    /// How many links a host's connection `out` carries now (0 when it is
    /// not the host's current connection).
    pub fn host_links(&self, host: &Fp, out: &Outbox) -> usize {
        self.lock()
            .hosts
            .get(host)
            .filter(|h| h.out.id == out.id)
            .map_or(0, |h| h.links.len())
    }

    /// Housekeeping: expired queue messages and rendezvous, and hosts
    /// whose record says suspended (set by `rlord-relay host suspend`
    /// while they were connected) are closed with 4402 with their devices.
    pub fn sweep(&self) {
        let now = Instant::now();
        let hosts: Vec<Fp> = {
            let mut guard = self.lock();
            let g = &mut *guard;
            g.rendezvous.retain(|_, r| r.expires > now);
            for q in g.to_device.values_mut().chain(g.to_host.values_mut()) {
                q.expire(now);
            }
            g.to_device.retain(|_, q| !q.msgs.is_empty());
            g.to_host.retain(|_, q| !q.msgs.is_empty());
            g.hosts.keys().copied().collect()
        };
        for h in &hosts {
            match self.registry.host(h) {
                Ok(Some((rec, _))) if rec.status == Status::Suspended => {
                    self.close_host(h, close::SUSPENDED);
                }
                // Every admitted host has a record, so a missing one was
                // removed (`rlord-relay host remove`).
                Ok(None) => self.close_host(h, close::NOT_ALLOWED),
                _ => {}
            }
        }
        if let Err(e) = self.expire_hosts(&hosts) {
            tracing::warn!("store: {e:#}");
        }
        if let Err(e) = self.registry.store.purge_expired() {
            tracing::warn!("store: {e:#}");
        }
    }

    /// Forgets hosts not seen for `host_expiry`, and their device lists.
    /// Connected hosts are seen now.
    fn expire_hosts(&self, connected: &[Fp]) -> anyhow::Result<()> {
        let Some(expiry) = self.cfg.host_expiry else {
            return Ok(());
        };
        let now = crate::store::now();
        for (fp, h) in self.registry.hosts()? {
            if !connected.contains(&fp) && now.saturating_sub(h.last_seen) > expiry.as_secs() {
                tracing::info!(host = %short(&fp), "forgetting an unused host");
                self.registry.remove_host(&fp)?;
            }
        }
        Ok(())
    }

    /// Closes a host's connection and its devices' and pairing devices':
    /// 4402 (with `bye` `suspended`) for a suspension, 4403 for a removal.
    pub fn close_host(&self, host: &Fp, close_code: u16) {
        let g = self.lock();
        let suspended = close_code == close::SUSPENDED;
        let bye = suspended.then_some((code::SUSPENDED, None));
        let reason = if suspended { "suspended" } else { "removed" };
        if let Some(h) = g.hosts.get(host) {
            h.out.kill(close_code, reason, bye);
        }
        for d in g.devices.values().filter(|d| d.host == *host) {
            d.out.kill(close_code, reason, bye);
        }
        for d in g.pairs.values().filter(|d| d.host == *host) {
            d.out.kill(close_code, reason, bye);
        }
    }
}

fn request_number(m: &ToRelay) -> u64 {
    match m {
        ToRelay::Devices { n, .. }
        | ToRelay::Rendezvous { n, .. }
        | ToRelay::RendezvousClose { n, .. }
        | ToRelay::Signal { n, .. }
        | ToRelay::Drop { n, .. }
        | ToRelay::AckSignal { n }
        | ToRelay::Ack { n, .. } => *n,
        ToRelay::Hello(_) | ToRelay::Auth { .. } => 0,
    }
}

/// Queues a mailbox message: its sequence number, or the `err` code.
fn enqueue(q: &mut Queue, limits: &Limits, payload: &[u8]) -> Result<u64, &'static str> {
    if limits.mailbox_ttl.is_zero() {
        return Err(code::NO_QUEUE);
    }
    let now = Instant::now();
    q.expire(now);
    if q.msgs.len() >= limits.mailbox_messages || q.bytes + payload.len() > limits.mailbox_bytes {
        return Err(code::FULL);
    }
    q.next_seq += 1;
    q.bytes += payload.len();
    q.msgs.push_back(Queued {
        seq: q.next_seq,
        payload: payload.to_vec(),
        expires: now + limits.mailbox_ttl,
    });
    Ok(q.next_seq)
}

/// Releases one unauthenticated slot of an address.
pub struct UnauthGuard {
    hub: Arc<Hub>,
    ip: IpAddr,
}

impl Drop for UnauthGuard {
    fn drop(&mut self) {
        let mut g = self.hub.lock();
        if let Some(n) = g.unauth.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                g.unauth.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Registration, StaticAuthorizer};
    use crate::store::{SqliteStore, Store};

    fn hub() -> (Hub, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::new(dir.path());
        let store: Arc<dyn Store> =
            Arc::new(SqliteStore::open(&dir.path().join("relay.db")).unwrap());
        let registry = Registry::new(store);
        let auth = Arc::new(StaticAuthorizer {
            registry: registry.clone(),
            registration: Registration::Open,
            limits: cfg.limits.clone(),
            max_hosts: 10,
            host_expiry: None,
        });
        let (_, shutdown) = watch::channel(false);
        let hub = Hub::new(
            cfg,
            crate::key::generate().unwrap(),
            "http://127.0.0.1:1".into(),
            "boot".into(),
            registry,
            auth,
            shutdown,
        );
        (hub, dir)
    }

    struct Conn {
        peer: Peer,
        ends: OutboxEnds,
    }

    fn conn(hub: &Hub, role: Role, fp: Fp, host: Fp, rendezvous: Option<RendezvousId>) -> Conn {
        let (out, ends) = Outbox::new(hub.next_id());
        Conn {
            peer: Peer {
                role,
                fp,
                host,
                rendezvous,
                account: "local".into(),
                limits: hub.cfg.limits.clone(),
                out,
                tally: hub.meter.open("local", &fp),
            },
            ends,
        }
    }

    impl Conn {
        /// The control messages sent to it so far.
        fn texts(&mut self) -> Vec<FromRelay> {
            let mut v = Vec::new();
            while let Ok(o) = self.ends.rx.try_recv() {
                if let Out::Text(t) = o {
                    v.push(FromRelay::parse(&t).unwrap());
                }
            }
            v
        }

        fn links(&mut self) -> Vec<u32> {
            self.texts()
                .into_iter()
                .filter_map(|m| match m {
                    FromRelay::Link { id, .. } => Some(id),
                    _ => None,
                })
                .collect()
        }

        fn killed(&self) -> bool {
            *self.ends.kill.borrow()
        }
    }

    /// The link a device's live frame reaches its host on, if any.
    fn live_link(hub: &Hub, c: &Conn) -> Option<u32> {
        match hub.binary(&c.peer, &FromDevice::Live(b"x".to_vec()).encode()) {
            Route::ToHost { frame, .. } => match ToHost::decode(&frame).unwrap() {
                ToHost::Live { link, .. } => Some(link),
                _ => panic!("not a live frame"),
            },
            Route::Reply(FromRelay::Undeliverable { .. }) => None,
            _ => panic!("unexpected route"),
        }
    }

    #[test]
    fn a_pairing_devices_old_link_never_touches_a_reconnected_hosts_links() {
        let (hub, _dir) = hub();
        let (hfp, dfp, pfp, rid) = ([1; 32], [2; 32], [3; 32], [9; 16]);
        hub.registry.set_devices(&hfp, &[dfp]).unwrap();
        let mut h1 = conn(&hub, Role::Host, hfp, hfp, None);
        hub.register(&h1.peer).unwrap();
        let ttl = 60;
        let ok = hub.control(
            &h1.peer,
            ToRelay::Rendezvous {
                n: 1,
                id: B64(rid.to_vec()),
                ttl,
            },
        );
        assert_eq!(ok, FromRelay::Ok { n: 1 });
        let p = conn(&hub, Role::Pair, pfp, hfp, Some(rid));
        hub.register(&p.peer).unwrap();
        let d = conn(&hub, Role::Device, dfp, hfp, None);
        hub.register(&d.peer).unwrap();
        let old = h1.links();
        assert_eq!(old.len(), 2);

        // The host reconnects: the pairing device is being closed, the
        // device relinked under an id the old connection never used.
        let mut h2 = conn(&hub, Role::Host, hfp, hfp, None);
        hub.register(&h2.peer).unwrap();
        assert!(p.killed());
        let relinked = h2.links();
        assert_eq!(relinked.len(), 1);
        assert!(!old.contains(&relinked[0]), "{old:?} {relinked:?}");
        assert_eq!(live_link(&hub, &d), Some(relinked[0]));

        // A frame the pairing device sent before its close is not
        // delivered; its unregister leaves the device's link alone.
        assert_eq!(live_link(&hub, &p), None);
        hub.unregister(&p.peer);
        assert!(h2.texts().is_empty());
        assert_eq!(live_link(&hub, &d), Some(relinked[0]));
    }

    #[test]
    fn a_stale_link_id_that_names_another_link_is_ignored() {
        // Defence in depth: even if a pairing device's remembered id were
        // reused for a device's link, it neither unlinks nor speaks for it.
        let (hub, _dir) = hub();
        let (hfp, dfp, pfp, rid) = ([1; 32], [2; 32], [3; 32], [9; 16]);
        hub.registry.set_devices(&hfp, &[dfp]).unwrap();
        let mut h = conn(&hub, Role::Host, hfp, hfp, None);
        hub.register(&h.peer).unwrap();
        hub.control(
            &h.peer,
            ToRelay::Rendezvous {
                n: 1,
                id: B64(rid.to_vec()),
                ttl: 60,
            },
        );
        let p = conn(&hub, Role::Pair, pfp, hfp, Some(rid));
        hub.register(&p.peer).unwrap();
        let d = conn(&hub, Role::Device, dfp, hfp, None);
        hub.register(&d.peer).unwrap();
        let links = h.links();
        let (pair_link, device_link) = (links[0], links[1]);
        {
            let mut g = hub.lock();
            g.hosts.get_mut(&hfp).unwrap().links.remove(&pair_link);
            g.pairs.get_mut(&pfp).unwrap().link = Some(device_link);
        }
        assert_eq!(live_link(&hub, &p), None);
        hub.unregister(&p.peer);
        assert!(h.texts().is_empty());
        assert_eq!(live_link(&hub, &d), Some(device_link));
    }

    #[test]
    fn a_device_removed_while_it_connects_is_not_admitted() {
        let (hub, _dir) = hub();
        let (hfp, dfp) = ([1; 32], [2; 32]);
        hub.registry.set_devices(&hfp, &[dfp]).unwrap();
        let mut h = conn(&hub, Role::Host, hfp, hfp, None);
        hub.register(&h.peer).unwrap();
        // `connect` saw the device on the list; then the host removed it
        // before the device registered.
        assert!(hub.registry.is_device(&hfp, &dfp).unwrap());
        let ok = hub.control(
            &h.peer,
            ToRelay::Devices {
                n: 1,
                allow: vec![],
            },
        );
        assert_eq!(ok, FromRelay::Ok { n: 1 });
        let d = conn(&hub, Role::Device, dfp, hfp, None);
        assert_eq!(hub.register(&d.peer), Err(close::NOT_ALLOWED));
        assert!(h.links().is_empty());
        assert!(hub.lock().devices.is_empty());
    }

    #[test]
    fn a_removed_devices_late_mailbox_frame_is_refused() {
        let (hub, _dir) = hub();
        let (hfp, dfp) = ([1; 32], [2; 32]);
        hub.registry.set_devices(&hfp, &[dfp]).unwrap();
        let h = conn(&hub, Role::Host, hfp, hfp, None);
        hub.register(&h.peer).unwrap();
        let d = conn(&hub, Role::Device, dfp, hfp, None);
        hub.register(&d.peer).unwrap();
        hub.control(
            &h.peer,
            ToRelay::Devices {
                n: 1,
                allow: vec![],
            },
        );
        assert!(d.killed());
        // A frame its reader had already taken in.
        let r = hub.binary(&d.peer, &FromDevice::Mailbox(b"m".to_vec()).encode());
        assert!(
            matches!(r, Route::Reply(FromRelay::Refused { ref code, .. }) if code == code::UNKNOWN)
        );
        assert!(hub.lock().to_host.is_empty());
    }
}
