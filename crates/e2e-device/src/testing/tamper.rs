//! [`Tamper`]: a relay in the middle that holds the pinned relay's own
//! key (and [`Flaky`], the same proxy used for bad networks).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use rlord_relay_proto::frame::{FromDevice, FromHost, ToDevice, ToHost};
use rlord_relay_proto::msg::{HostState, LinkKind, Ready};
use rlord_relay_proto::{auth, FromRelay, Identity, Role, ToRelay, B64};

use super::challenge;

/// Which way a message goes through a [`Tamper`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// From the peer (host or device) to the relay.
    Up,
    /// From the relay to the peer.
    Down,
}

/// A connection through a [`Tamper`].
#[derive(Clone, Debug)]
pub struct Peer {
    pub conn: u64,
    pub role: Role,
    /// The peer's relay key fingerprint.
    pub fp: [u8; 32],
}

/// Decides what passes: the messages to send on in the same direction
/// (none: dropped).
pub type Filter = Box<dyn FnMut(&Peer, Dir, Message) -> Vec<Message> + Send>;

enum Ctl {
    Inject(Dir, Message),
    /// Drop the connection without a close frame (a cut cable).
    Cut,
    /// Stop reading and writing both ways (a stalled path: not even a
    /// pong).
    Freeze,
}

#[derive(Default)]
struct State {
    upstream: Option<String>,
    filter: Option<Filter>,
    away: bool,
    cut_next: Option<(Role, usize)>,
    log: Vec<(Role, [u8; 32], Instant)>,
    peers: HashMap<u64, (Peer, mpsc::UnboundedSender<Ctl>)>,
    terminate: HashSet<[u8; 32]>,
    /// Links this proxy invented (id → the device connection it serves).
    fake: HashMap<u32, u64>,
    next_conn: u64,
    next_fake: u32,
    /// Messages each connection sent up.
    up_count: HashMap<u64, usize>,
}

/// A relay in the middle: it listens at the pinned origin, forwards every
/// connection to the real relay unchanged (so a peer's pin check passes:
/// the real relay signs the challenge), and holds the relay's own key so
/// it can also answer a device itself. A test makes it misbehave: a
/// filter that rewrites, drops, replays or swaps what passes; messages
/// injected either way; links it invents for the host; connections cut
/// or frozen; the device's network "away". The host and the devices both
/// reach the relay through it (the relay's `origin` is this proxy's), so
/// it sees everything a hostile relay would.
pub struct Tamper {
    pub addr: SocketAddr,
    /// `http://127.0.0.1:<port>`: give it to the relay as its `origin`.
    pub origin: String,
    key: Identity,
    st: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}

/// A [`Tamper`] used only for cuts, freezes and outages: a bad network
/// rather than a hostile relay.
pub type Flaky = Tamper;

impl Drop for Tamper {
    fn drop(&mut self) {
        self.task.abort();
        for (_, (_, tx)) in self.st.lock().unwrap().peers.drain() {
            let _ = tx.send(Ctl::Cut);
        }
    }
}

impl Tamper {
    /// Listens on 127.0.0.1; holds `key`, the relay's own. Connections
    /// wait until [`Tamper::upstream`] names the real relay.
    pub async fn new(key: Identity) -> Tamper {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let origin = format!("http://{addr}");
        let st = Arc::new(Mutex::new(State::default()));
        let (st2, key2, origin2) = (st.clone(), key.clone(), origin.clone());
        let task = tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                let (st, key, origin) = (st2.clone(), key2.clone(), origin2.clone());
                tokio::spawn(async move {
                    let _ = serve(s, st, key, origin).await;
                });
            }
        });
        Tamper {
            addr,
            origin,
            key,
            st,
            task,
        }
    }

    /// The real relay's websocket URL.
    pub fn upstream(&self, url: &str) {
        self.st.lock().unwrap().upstream = Some(url.to_string());
    }

    /// The relay key it holds.
    pub fn key(&self) -> &Identity {
        &self.key
    }

    pub fn set_filter(&self, f: Filter) {
        self.st.lock().unwrap().filter = Some(f);
    }

    pub fn clear_filter(&self) {
        self.st.lock().unwrap().filter = None;
    }

    fn each(&self, role: Role, c: impl Fn() -> Ctl) {
        let st = self.st.lock().unwrap();
        for (p, tx) in st.peers.values() {
            if p.role == role {
                let _ = tx.send(c());
            }
        }
    }

    /// Sends `m` to every connected peer of `role`, as if from the relay.
    pub fn inject(&self, role: Role, m: Message) {
        self.each(role, || Ctl::Inject(Dir::Down, m.clone()));
    }

    /// Sends `m` to the relay on every connection of `role`, as if from
    /// the peer.
    pub fn inject_up(&self, role: Role, m: Message) {
        self.each(role, || Ctl::Inject(Dir::Up, m.clone()));
    }

    /// Cuts every connection of `role` (no close frame).
    pub fn cut(&self, role: Role) {
        self.each(role, || Ctl::Cut);
    }

    /// Stalls every connection of `role`: nothing passes either way, not
    /// even a pong.
    pub fn freeze(&self, role: Role) {
        self.each(role, || Ctl::Freeze);
    }

    /// The next connection of `role` is cut once it has sent `n`
    /// messages (its hello is the first).
    pub fn cut_next(&self, role: Role, n: usize) {
        self.st.lock().unwrap().cut_next = Some((role, n));
    }

    /// Whether a `cut_next` is still waiting for its connection.
    pub fn cut_pending(&self) -> bool {
        self.st.lock().unwrap().cut_next.is_some()
    }

    /// The device's network is away (`true`): its connections are cut and
    /// new ones dropped. The host's are untouched.
    pub fn away(&self, away: bool) {
        self.st.lock().unwrap().away = away;
        if away {
            self.cut(Role::Device);
        }
    }

    /// Answer the device with this relay key fingerprint as the relay
    /// itself (never forwarded): `ready` with its host online, and a link
    /// to the host invented for its frames.
    pub fn terminate(&self, fp: [u8; 32]) {
        self.st.lock().unwrap().terminate.insert(fp);
    }

    /// When each connection (or attempt, while away) of `role` came.
    pub fn connections(&self, role: Role) -> Vec<Instant> {
        self.st
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|(r, _, _)| *r == role)
            .map(|(_, _, t)| *t)
            .collect()
    }

    /// Messages the connected peers of `role` have sent up.
    pub fn sent_up(&self, role: Role) -> usize {
        let st = self.st.lock().unwrap();
        st.peers
            .values()
            .filter(|(p, _)| p.role == role)
            .map(|(p, _)| st.up_count.get(&p.conn).copied().unwrap_or(0))
            .sum()
    }

    /// The peers connected now.
    pub fn peers(&self) -> Vec<Peer> {
        self.st
            .lock()
            .unwrap()
            .peers
            .values()
            .map(|(p, _)| p.clone())
            .collect()
    }
}

type Down = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn serve(
    s: tokio::net::TcpStream,
    st: Arc<Mutex<State>>,
    key: Identity,
    origin: String,
) -> anyhow::Result<()> {
    let mut down = tokio_tungstenite::accept_async(s).await?;
    let Some(Ok(Message::Text(t))) = down.next().await else {
        return Ok(());
    };
    let ToRelay::Hello(h) = ToRelay::parse(&t)? else {
        return Ok(());
    };
    let fp = auth::fingerprint(&h.key.0);
    let (conn, terminate, upstream) = {
        let mut g = st.lock().unwrap();
        g.log.push((h.role, fp, Instant::now()));
        if g.away && h.role == Role::Device {
            return Ok(()); // dropped: no network
        }
        g.next_conn += 1;
        (
            g.next_conn,
            h.role == Role::Device && g.terminate.contains(&fp),
            g.upstream.clone(),
        )
    };
    let peer = Peer {
        conn,
        role: h.role,
        fp,
    };
    let (tx, rx) = mpsc::unbounded_channel();
    st.lock().unwrap().peers.insert(conn, (peer.clone(), tx));
    let r = if terminate {
        serve_as_relay(&mut down, &peer, &t, rx, &st, &key, &origin).await
    } else {
        match upstream {
            Some(u) => forward(down, &peer, t.to_string(), rx, &st, &u).await,
            None => Ok(()),
        }
    };
    let mut g = st.lock().unwrap();
    g.peers.remove(&conn);
    g.fake.retain(|_, c| *c != conn);
    r
}

fn send_to(st: &Mutex<State>, conn: u64, c: Ctl) {
    if let Some((_, tx)) = st.lock().unwrap().peers.get(&conn) {
        let _ = tx.send(c);
    }
}

enum Next {
    Send(Vec<Message>),
    Routed(u64, Ctl),
    Cut,
}

/// `RLORD_TAMPER_TRACE=1`: every message through, on stderr (debugging a
/// test).
fn trace(peer: &Peer, dir: Dir, m: &Message) {
    if std::env::var_os("RLORD_TAMPER_TRACE").is_none() {
        return;
    }
    let what = match m {
        Message::Text(t) => t.chars().take(120).collect::<String>(),
        Message::Binary(b) => format!("binary lane {} len {}", b.first().unwrap_or(&0), b.len()),
        Message::Close(c) => format!("close {c:?}"),
        _ => return,
    };
    eprintln!("tamper {} {:?} {dir:?} {what}", peer.conn, peer.role);
}

async fn forward(
    mut down: Down,
    peer: &Peer,
    hello: String,
    mut rx: mpsc::UnboundedReceiver<Ctl>,
    st: &Mutex<State>,
    upstream: &str,
) -> anyhow::Result<()> {
    let (mut up, _) = tokio_tungstenite::connect_async(upstream).await?;
    up.send(Message::Text(hello.into())).await?;
    *st.lock().unwrap().up_count.entry(peer.conn).or_default() += 1;
    let mut frozen = false;
    loop {
        if frozen {
            match rx.recv().await {
                Some(Ctl::Cut) | None => return Ok(()),
                _ => continue,
            }
        }
        tokio::select! {
            m = down.next() => {
                let Some(Ok(m)) = m else { return Ok(()) };
                trace(peer, Dir::Up, &m);
                if let Message::Close(c) = m {
                    let _ = up.send(Message::Close(c)).await;
                    return Ok(());
                }
                if !matches!(m, Message::Text(_) | Message::Binary(_)) {
                    continue;
                }
                let next = {
                    let mut g = st.lock().unwrap();
                    let n = {
                        let c = g.up_count.entry(peer.conn).or_default();
                        *c += 1;
                        *c
                    };
                    match g.cut_next {
                        Some((role, after)) if role == peer.role && n >= after => {
                            g.cut_next = None;
                            Next::Cut
                        }
                        _ => match (peer.role == Role::Host).then(|| route_fake(&g, &m)).flatten() {
                            Some((dev, c)) => Next::Routed(dev, c),
                            None => Next::Send(match g.filter.as_mut() {
                                Some(f) => f(peer, Dir::Up, m),
                                None => vec![m],
                            }),
                        },
                    }
                };
                match next {
                    Next::Cut => return Ok(()), // both sockets dropped
                    Next::Routed(dev, c) => send_to(st, dev, c),
                    Next::Send(out) => {
                        for m in out {
                            up.send(m).await?;
                        }
                    }
                }
            }
            m = up.next() => {
                let Some(Ok(m)) = m else { return Ok(()) };
                trace(peer, Dir::Down, &m);
                if let Message::Close(c) = m {
                    let _ = down.send(Message::Close(c)).await;
                    return Ok(());
                }
                if !matches!(m, Message::Text(_) | Message::Binary(_)) {
                    continue;
                }
                let out = {
                    let mut g = st.lock().unwrap();
                    match g.filter.as_mut() {
                        Some(f) => f(peer, Dir::Down, m),
                        None => vec![m],
                    }
                };
                for m in out {
                    down.send(m).await?;
                }
            }
            c = rx.recv() => match c {
                Some(Ctl::Inject(Dir::Down, m)) => down.send(m).await?,
                Some(Ctl::Inject(Dir::Up, m)) => up.send(m).await?,
                Some(Ctl::Freeze) => frozen = true,
                Some(Ctl::Cut) | None => return Ok(()),
            }
        }
    }
}

/// A host's frame for a link this proxy invented: where it goes instead
/// (None: not one of ours). A `drop` of such a link cuts its device.
fn route_fake(g: &State, m: &Message) -> Option<(u64, Ctl)> {
    match m {
        Message::Binary(b) => match FromHost::decode(b) {
            Ok(FromHost::Live { link, payload }) => {
                let dev = *g.fake.get(&link)?;
                Some((
                    dev,
                    Ctl::Inject(
                        Dir::Down,
                        Message::Binary(ToDevice::Live(payload).encode().into()),
                    ),
                ))
            }
            _ => None,
        },
        Message::Text(t) => match ToRelay::parse(t) {
            Ok(ToRelay::Drop { link, .. }) => Some((*g.fake.get(&link)?, Ctl::Cut)),
            _ => None,
        },
        _ => None,
    }
}

/// Plays the relay to a device itself (it holds the relay's key): signs
/// the challenge, takes any `auth`, says `ready` with the host online,
/// and invents a link to the host for it.
async fn serve_as_relay(
    down: &mut Down,
    peer: &Peer,
    hello: &str,
    mut rx: mpsc::UnboundedReceiver<Ctl>,
    st: &Mutex<State>,
    key: &Identity,
    origin: &str,
) -> anyhow::Result<()> {
    let hello = ToRelay::parse(hello)?;
    let Some((c, _)) = challenge(origin, &key.public(), key, &hello) else {
        return Ok(());
    };
    down.send(Message::Text(c.to_json().into())).await?;
    let _auth = down.next().await;
    let ready = FromRelay::Ready(Ready {
        boot: "tamper".into(),
        host: Some(HostState::Online),
        signal: Some(0),
        queued: Some(0),
        limits: None,
    });
    down.send(Message::Text(ready.to_json().into())).await?;
    let link = {
        let mut g = st.lock().unwrap();
        g.next_fake += 1;
        let link = 0x7000_0000 + g.next_fake;
        g.fake.insert(link, peer.conn);
        link
    };
    let hosts: Vec<_> = {
        let g = st.lock().unwrap();
        g.peers
            .values()
            .filter(|(p, _)| p.role == Role::Host)
            .map(|(_, tx)| tx.clone())
            .collect()
    };
    let to_hosts = |m: Message| {
        for h in &hosts {
            let _ = h.send(Ctl::Inject(Dir::Down, m.clone()));
        }
    };
    let invented = FromRelay::Link {
        id: link,
        device: B64(peer.fp.to_vec()),
        kind: LinkKind::Device,
        rendezvous: None,
    };
    to_hosts(Message::Text(invented.to_json().into()));
    loop {
        tokio::select! {
            m = down.next() => {
                let Some(Ok(m)) = m else { break };
                if let Message::Binary(b) = m {
                    if let Ok(FromDevice::Live(p)) = FromDevice::decode(&b) {
                        to_hosts(Message::Binary(
                            ToHost::Live { link, payload: p }.encode().into(),
                        ));
                    }
                }
            }
            c = rx.recv() => match c {
                Some(Ctl::Inject(Dir::Down, m)) => down.send(m).await?,
                Some(Ctl::Inject(Dir::Up, _)) | Some(Ctl::Freeze) => {}
                Some(Ctl::Cut) | None => break,
            }
        }
    }
    to_hosts(Message::Text(
        FromRelay::Unlink { id: link }.to_json().into(),
    ));
    Ok(())
}
