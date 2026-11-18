//! The relay's memory: who is connected, links, rendezvous, signal flags
//! and mailbox queues. None of it is ever written to disk; a restart
//! forgets it (and `ready.boot` changes, which a device reads as "fetch").
//!
//! Every operation takes the one lock briefly and never awaits under it.
//! What must wait (forwarding a live frame to a full receiver) is done by
//! the connection task after the lock is released (`conn.rs`).

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

/// Frames a connection's outgoing buffer holds before a sender waits.
pub const OUT_FRAMES: usize = 64;
/// Control messages queued to a peer that doesn't read before it is closed.
const MAX_PENDING_TEXT: usize = 1024;

pub type RendezvousId = [u8; 16];

/// What a connection's writer sends.
pub enum Out {
    Text(String),
    /// A binary frame; a live frame holds one of the receiver's
    /// [`OUT_FRAMES`] permits until it is written.
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
                permits: Arc::new(Semaphore::new(OUT_FRAMES)),
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
}

struct HostConn {
    out: Outbox,
    next_link: u32,
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
        let id = h.next_link;
        h.next_link = h.next_link.wrapping_add(1).max(1);
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
            },
        );
        Some(id)
    }

    fn unlink(&mut self, host: &Fp, id: u32) {
        if let Some(h) = self.hosts.get_mut(host) {
            if h.links.remove(&id).is_some() {
                h.out.send(&FromRelay::Unlink { id });
            }
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
                self.unlink(host, l);
            }
        }
    }
}

/// What to do with a binary frame.
pub enum Route {
    Nothing,
    Reply(FromRelay),
    /// Forward `frame` to `to` (waiting for room); answer `undeliverable`
    /// if the link stalls.
    Forward {
        to: Outbox,
        frame: Vec<u8>,
        undeliverable: FromRelay,
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
                p.out.send(&FromRelay::Ready(Ready {
                    limits: Some(p.limits.wire()),
                    ..self.ready()
                }));
                g.hosts.insert(
                    p.fp,
                    HostConn {
                        out: p.out.clone(),
                        next_link: 1,
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
                if let Some(old) = g.devices.remove(&p.fp) {
                    old.out.kill(close::REPLACED, "replaced", None);
                    if let Some(l) = old.link {
                        g.unlink(&old.host, l);
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
                        g.unlink(&old.host, l);
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
                    g.unlink(&d.host, l);
                }
            }
            Role::Pair => {
                if g.pairs.get(&p.fp).map(|d| d.out.id) != Some(p.out.id) {
                    return;
                }
                let pc = g.pairs.remove(&p.fp).unwrap();
                if let Some(l) = pc.link {
                    g.unlink(&pc.host, l);
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
                        l.out.kill(1000, "dropped by the host", None);
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
                    match g.hosts.get(&p.fp).and_then(|h| h.links.get(&link)) {
                        Some(l) => Route::Forward {
                            to: l.out.clone(),
                            frame: ToDevice::Live(payload).encode(),
                            undeliverable,
                        },
                        None => Route::Reply(undeliverable),
                    }
                }
                Ok(FromHost::Mailbox { device, payload }) => {
                    match self.registry.is_device(&p.fp, &device) {
                        Ok(true) => {}
                        Ok(false) => return Route::Reply(err(0, code::UNKNOWN)),
                        Err(e) => {
                            tracing::warn!("store: {e:#}");
                            return Route::Reply(err(0, code::BAD));
                        }
                    }
                    let mut g = self.lock();
                    let pair = (p.fp, device);
                    let seq =
                        match enqueue(g.to_device.entry(pair).or_default(), &p.limits, &payload) {
                            Ok(seq) => seq,
                            Err(c) => return Route::Reply(err(0, c)),
                        };
                    if let Some(d) = g.devices.get(&device).filter(|d| d.host == p.fp) {
                        d.out
                            .frame(ToDevice::Mailbox { seq, payload }.encode(), None);
                    }
                    Route::Nothing
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
                    match link.and_then(|l| Some((l, g.hosts.get(&p.host)?))) {
                        Some((link, h)) => Route::Forward {
                            to: h.out.clone(),
                            frame: ToHost::Live { link, payload }.encode(),
                            undeliverable,
                        },
                        None => Route::Reply(undeliverable),
                    }
                }
                Ok(FromDevice::Mailbox(payload)) => {
                    if p.role == Role::Pair {
                        return Route::Reply(err(0, code::BAD));
                    }
                    let mut g = self.lock();
                    let pair = (p.host, p.fp);
                    let seq = match enqueue(g.to_host.entry(pair).or_default(), &p.limits, &payload)
                    {
                        Ok(seq) => seq,
                        Err(c) => return Route::Reply(err(0, c)),
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
                    Route::Nothing
                }
            },
        }
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
        for h in hosts {
            if let Ok(Some((rec, _))) = self.registry.host(&h) {
                if rec.status == Status::Suspended {
                    self.close_host(&h, close::SUSPENDED);
                }
            }
        }
        if let Err(e) = self.registry.store.purge_expired() {
            tracing::warn!("store: {e:#}");
        }
    }

    /// Closes a host's connection and its devices' and pairing devices':
    /// 4402 (with `bye` `suspended`) for a suspension, 4403 for a removal.
    pub fn close_host(&self, host: &Fp, close_code: u16) {
        let g = self.lock();
        let bye = (close_code == close::SUSPENDED).then_some((code::SUSPENDED, None));
        if let Some(h) = g.hosts.get(host) {
            h.out.kill(close_code, "suspended", bye);
        }
        for d in g.devices.values().filter(|d| d.host == *host) {
            d.out.kill(close_code, "suspended", bye);
        }
        for d in g.pairs.values().filter(|d| d.host == *host) {
            d.out.kill(close_code, "suspended", bye);
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
