//! A small raw relay-link client for the tests, built on relay-proto: the
//! connect sequence (checking the relay against its pin), numbered
//! requests and binary frames. Every relay here is the real one on
//! 127.0.0.1.
#![allow(dead_code, clippy::result_large_err)]

use std::collections::VecDeque;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rlord_relay::{Config, Running};
use rlord_relay_proto::msg::{Enroll, FromRelay, Hello, Ready, ToRelay, VERSION};
use rlord_relay_proto::{auth, check_challenge, Identity, RelayAddress, Role, B64};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub type Fp = [u8; 32];
pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long a test waits for something that should come.
pub const WAIT: Duration = Duration::from_secs(5);

pub struct Relay {
    pub running: Running,
    pub cfg: Config,
    pub dir: tempfile::TempDir,
}

impl Relay {
    pub fn pin(&self) -> RelayAddress {
        self.running.address().clone()
    }

    pub fn url(&self) -> String {
        self.running.ws_url()
    }

    /// A token for one host.
    pub fn token(&self) -> String {
        self.running
            .admin()
            .token_new(1, Duration::from_secs(600), "local")
            .unwrap()
    }

    /// Stops it and starts it again on the same port and data dir (a new
    /// `boot`).
    pub async fn restart(self) -> Relay {
        let Relay {
            running,
            mut cfg,
            dir,
        } = self;
        cfg.listen = running.addr();
        running.shutdown().await;
        let running = rlord_relay::start(cfg.clone()).await.unwrap();
        Relay { running, cfg, dir }
    }
}

/// A relay on 127.0.0.1:0 in a scratch data dir; `f` adjusts the config.
pub async fn relay(f: impl FnOnce(&mut Config)) -> Relay {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::new(dir.path().join("data"));
    // Fast tests: high rate limits unless a test lowers them.
    cfg.limits.bytes_per_sec = 64 * 1024 * 1024;
    cfg.limits.burst_bytes = 64 * 1024 * 1024;
    f(&mut cfg);
    let running = rlord_relay::start(cfg.clone()).await.unwrap();
    Relay { running, cfg, dir }
}

pub fn id() -> Identity {
    rlord_relay::key::generate().unwrap()
}

pub fn nonce() -> [u8; 32] {
    rlord_relay::key::random().unwrap()
}

/// Who connects, and to what.
#[derive(Clone)]
pub enum As {
    Host(Option<String>),
    Device(Fp),
    Pair([u8; 16]),
}

impl As {
    pub fn role(&self) -> Role {
        match self {
            As::Host(_) => Role::Host,
            As::Device(_) => Role::Device,
            As::Pair(_) => Role::Pair,
        }
    }

    fn target(&self) -> Vec<u8> {
        match self {
            As::Host(_) => vec![],
            As::Device(h) => h.to_vec(),
            As::Pair(r) => r.to_vec(),
        }
    }

    pub fn hello(&self, key: &Identity, nonce: [u8; 32]) -> Hello {
        Hello {
            v: VERSION,
            role: self.role(),
            key: B64(key.public().to_vec()),
            nonce: B64(nonce.to_vec()),
            host: match self {
                As::Device(h) => Some(B64(h.to_vec())),
                _ => None,
            },
            rendezvous: match self {
                As::Pair(r) => Some(B64(r.to_vec())),
                _ => None,
            },
            enroll: match self {
                As::Host(Some(t)) => Some(Enroll::Token { token: t.clone() }),
                _ => None,
            },
        }
    }
}

/// What a client read.
#[derive(Debug, Clone, PartialEq)]
pub enum Got {
    Ctl(FromRelay),
    Bin(Vec<u8>),
    /// The close code, if any.
    Closed(Option<u16>),
}

/// Why a connection didn't reach `ready`.
#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    pub code: Option<u16>,
    /// A `bye` before the close.
    pub bye: Option<FromRelay>,
    /// `enroll` messages shown on the way.
    pub enroll: Vec<FromRelay>,
}

pub struct Client {
    pub ws: Ws,
    pub key: Identity,
    pub ready: Ready,
    /// `enroll` messages that came before `ready`.
    pub enroll: Vec<FromRelay>,
    n: u64,
    backlog: VecDeque<Got>,
}

pub async fn open(url: &str) -> Ws {
    tokio_tungstenite::connect_async(url).await.unwrap().0
}

async fn read(ws: &mut Ws) -> Got {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                return Got::Ctl(FromRelay::parse(&t).expect("a relay message"))
            }
            Some(Ok(Message::Binary(b))) => return Got::Bin(b.to_vec()),
            Some(Ok(Message::Close(c))) => {
                return Got::Closed(c.map(|c| u16::from(c.code)));
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return Got::Closed(None),
        }
    }
}

async fn read_timeout(ws: &mut Ws, d: Duration) -> Got {
    tokio::time::timeout(d, read(ws))
        .await
        .expect("nothing came from the relay")
}

/// The connect sequence over an open socket, checking the relay against
/// `pin`; `ready`, or how it was refused.
pub async fn handshake(
    mut ws: Ws,
    pin: &RelayAddress,
    key: &Identity,
    who: &As,
) -> Result<Client, Refused> {
    let n = nonce();
    ws.send(Message::text(ToRelay::Hello(who.hello(key, n)).to_json()))
        .await
        .unwrap();
    let refused = |code, bye, enroll| Refused { code, bye, enroll };
    let (origin, rkey, rnonce, sig) = match read_timeout(&mut ws, WAIT).await {
        Got::Ctl(FromRelay::Challenge {
            origin,
            key,
            nonce,
            sig,
        }) => (origin, key, nonce, sig),
        Got::Closed(c) => return Err(refused(c, None, vec![])),
        Got::Ctl(b @ FromRelay::Bye { .. }) => {
            let c = match read_timeout(&mut ws, WAIT).await {
                Got::Closed(c) => c,
                other => panic!("after bye: {other:?}"),
            };
            return Err(refused(c, Some(b), vec![]));
        }
        other => panic!("expected challenge, got {other:?}"),
    };
    check_challenge(pin, &key.public(), &n, &origin, &rkey.0, &rnonce.0, &sig.0)
        .expect("the relay proves the pinned identity");
    let t = auth::peer_transcript(
        who.role(),
        &origin,
        &rkey.0,
        &key.public(),
        &rnonce.0,
        &n,
        &who.target(),
    );
    ws.send(Message::text(
        ToRelay::Auth {
            sig: B64(key.sign(&t).to_vec()),
        }
        .to_json(),
    ))
    .await
    .unwrap();
    let mut enroll = Vec::new();
    let mut bye = None;
    loop {
        match read_timeout(&mut ws, WAIT * 2).await {
            Got::Ctl(FromRelay::Ready(ready)) => {
                return Ok(Client {
                    ws,
                    key: key.clone(),
                    ready,
                    enroll,
                    n: 0,
                    backlog: VecDeque::new(),
                })
            }
            Got::Ctl(e @ FromRelay::Enroll { .. }) => enroll.push(e),
            Got::Ctl(b @ FromRelay::Bye { .. }) => bye = Some(b),
            Got::Closed(c) => return Err(refused(c, bye, enroll)),
            other => panic!("expected ready, got {other:?}"),
        }
    }
}

pub async fn connect(r: &Relay, key: &Identity, who: As) -> Result<Client, Refused> {
    handshake(open(&r.url()).await, &r.pin(), key, &who).await
}

/// A registered host (by a token), connected.
pub async fn host(r: &Relay) -> Client {
    let key = id();
    connect(r, &key, As::Host(Some(r.token()))).await.unwrap()
}

impl Client {
    pub fn fp(&self) -> Fp {
        self.key.fingerprint()
    }

    /// The next thing from the relay (backlog first).
    pub async fn recv(&mut self) -> Got {
        if let Some(g) = self.backlog.pop_front() {
            return g;
        }
        read_timeout(&mut self.ws, WAIT).await
    }

    /// The next thing within `d`, if anything comes.
    pub async fn recv_within(&mut self, d: Duration) -> Option<Got> {
        if let Some(g) = self.backlog.pop_front() {
            return Some(g);
        }
        tokio::time::timeout(d, read(&mut self.ws)).await.ok()
    }

    /// Nothing comes within `d`.
    pub async fn quiet(&mut self, d: Duration) {
        if let Some(g) = self.recv_within(d).await {
            panic!("expected nothing, got {g:?}");
        }
    }

    pub async fn ctl(&mut self) -> FromRelay {
        match self.recv().await {
            Got::Ctl(m) => m,
            other => panic!("expected a control message, got {other:?}"),
        }
    }

    pub async fn bin(&mut self) -> Vec<u8> {
        match self.recv().await {
            Got::Bin(b) => b,
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    /// Reads until the close; its code (`bye` and anything else skipped).
    pub async fn closed(&mut self) -> Option<u16> {
        loop {
            if let Got::Closed(c) = self.recv().await {
                return c;
            }
        }
    }

    /// Reads until the close; the `bye` before it and the code.
    pub async fn bye_and_close(&mut self) -> (Option<FromRelay>, Option<u16>) {
        let mut bye = None;
        loop {
            match self.recv().await {
                Got::Closed(c) => return (bye, c),
                Got::Ctl(b @ FromRelay::Bye { .. }) => bye = Some(b),
                _ => {}
            }
        }
    }

    pub async fn send(&mut self, m: &ToRelay) {
        self.ws.send(Message::text(m.to_json())).await.unwrap();
    }

    pub async fn send_bin(&mut self, b: Vec<u8>) {
        self.ws.send(Message::binary(b)).await.unwrap();
    }

    /// Sends a numbered request and waits for its `ok` or `err` (anything
    /// else that comes meanwhile is kept for `recv`).
    pub async fn req(&mut self, f: impl FnOnce(u64) -> ToRelay) -> FromRelay {
        self.n += 1;
        let n = self.n;
        self.send(&f(n)).await;
        let mut held = Vec::new();
        let answer = loop {
            match read_timeout(&mut self.ws, WAIT).await {
                Got::Ctl(m @ FromRelay::Ok { n: k }) if k == n => break m,
                Got::Ctl(m @ FromRelay::Err { n: k, .. }) if k == n => break m,
                other => held.push(other),
            }
        };
        self.backlog.extend(held);
        answer
    }

    /// A request that must succeed.
    pub async fn ok(&mut self, f: impl FnOnce(u64) -> ToRelay) {
        let a = self.req(f).await;
        assert!(matches!(a, FromRelay::Ok { .. }), "{a:?}");
    }

    /// A request that must fail with `code`.
    pub async fn refused(&mut self, code: &str, f: impl FnOnce(u64) -> ToRelay) {
        match self.req(f).await {
            FromRelay::Err { code: c, .. } => assert_eq!(c, code),
            a => panic!("expected err {code}, got {a:?}"),
        }
    }

    /// The host's allow-list.
    pub async fn allow(&mut self, devices: &[Fp]) {
        let allow = devices.iter().map(|d| B64(d.to_vec())).collect();
        self.ok(|n| ToRelay::Devices { n, allow }).await;
    }

    pub async fn close(mut self) {
        let _ = self
            .ws
            .close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: CloseCode::Normal,
                reason: "".into(),
            }))
            .await;
    }
}

/// Waits until `f` holds (polling), or panics after [`WAIT`].
pub async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let until = tokio::time::Instant::now() + WAIT;
    while !f() {
        if tokio::time::Instant::now() > until {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
