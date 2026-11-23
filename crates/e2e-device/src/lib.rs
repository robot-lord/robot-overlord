//! rlord's test device, for the remote tests and for pairing by hand
//! (`rlord-device`).
//!
//! - [`Phone`]: the device engine the phone app runs
//!   (`rlord_e2e_client`), with software keys, driven as the page drives
//!   it. The CLI and `crates/web/tests/engine.rs` use it.
//! - [`Device`], [`Live`], [`Conn`]: `docs/working/e2e-protocol.md` from
//!   the device's side one step at a time (pairing from a QR, live
//!   sessions, the cheap check, the mailbox), for tests that need to do
//!   what the engine never would (`crates/web/tests/remote.rs`).
//! - `testing`: the hostile relays of the adversarial tests.
//! - [`bridge`]: `rlord-device bridge`, the engine behind a loopback
//!   websocket that plays the phone shell's message bridge (debug only).

pub mod bridge;
pub mod client;
pub mod phone;
pub mod testing;

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};

use rlord_e2e::stream::{self, Reassembler, CONTROL};
use rlord_e2e::{device_id, Initiator, MailboxSession, QrPayload, Session, StaticKey};
use rlord_relay_proto::address::fingerprint_from_b64;
use rlord_relay_proto::{Identity, RelayAddress, Role};

pub use client::{connect, Conn, ConnectError, Opts};
pub use phone::Phone;

const WAIT: Duration = Duration::from_secs(10);

pub fn random32() -> Result<[u8; 32]> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| anyhow!("random: {e}"))?;
    Ok(b)
}

pub fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub fn unb64(s: &str) -> Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s)?)
}

/// The host a device paired with.
#[derive(Clone, Debug)]
pub struct Host {
    pub static_key: [u8; 32],
    pub pin: RelayAddress,
    /// The host's relay key fingerprint (`hello.host`).
    pub fp: [u8; 32],
    pub name: String,
}

pub struct Device {
    pub name: String,
    pub static_key: StaticKey,
    pub relay_key: Identity,
    pub host: Option<Host>,
    pub mailbox: Option<MailboxSession>,
    /// The relay boot and queue number of the last mailbox frame opened:
    /// one the relay delivers again (we didn't acknowledge it in time) is
    /// recognised and not decrypted twice, as the host does.
    pub mailbox_seen: Option<(String, u64)>,
    pub opts: Opts,
}

impl Device {
    pub fn new(name: &str) -> Result<Device> {
        let relay_key = loop {
            if let Some(k) = Identity::from_seed(random32()?) {
                break k;
            }
        };
        Ok(Device {
            name: name.to_string(),
            static_key: StaticKey::generate().map_err(|e| anyhow!("{e}"))?,
            relay_key,
            host: None,
            mailbox: None,
            mailbox_seen: None,
            opts: Opts::default(),
        })
    }

    /// What the host calls this device.
    pub fn id(&self) -> String {
        device_id(self.static_key.public())
    }

    fn host(&self) -> Result<&Host> {
        self.host.as_ref().context("not paired")
    }

    /// Scan `qr`: dial its relay as a pairing device, run the pairing
    /// handshake, send the first frame, and wait for the host's `waiting`.
    pub async fn start_pairing(&mut self, qr: &str) -> Result<PairingRun> {
        let qr = QrPayload::decode(qr).map_err(|e| anyhow!("{e}"))?;
        let pin = RelayAddress {
            origin: qr.relay.origin.clone(),
            fingerprint: qr.relay.fingerprint,
        };
        let mut conn = connect(
            &pin,
            &self.relay_key,
            Role::Pair,
            &qr.rendezvous_id,
            &self.opts,
        )
        .await
        .map_err(|e| anyhow!("{e}"))?;
        let (ini, msg1) = Initiator::pair(&qr, &self.static_key, &conn.relay, None)
            .map_err(|e| anyhow!("{e}"))?;
        conn.send_live(msg1).await?;
        let msg2 = conn.recv_live(WAIT).await?;
        let est = ini.finish(&msg2).map_err(|e| anyhow!("pairing: {e}"))?;
        let code = format!("{:06}", est.short_code());
        let mut live = Live::new(est.into_session().map_err(|e| anyhow!("{e}"))?);
        live.send_json(
            &mut conn,
            &json!({"event": "pair", "name": self.name, "key": b64(&self.relay_key.public())}),
        )
        .await?;
        let waiting = live.event(&mut conn, "pair", WAIT).await?;
        if waiting["state"] != "waiting" {
            bail!("the host answered {waiting}");
        }
        Ok(PairingRun {
            conn,
            live,
            code,
            host_code: waiting["code"].as_str().unwrap_or("").to_string(),
            pin,
            host_static: qr.host_static,
        })
    }

    /// Dial the host's relay as this device.
    pub async fn connect(&self) -> Result<Conn> {
        let h = self.host()?;
        connect(&h.pin, &self.relay_key, Role::Device, &h.fp, &self.opts)
            .await
            .map_err(|e| anyhow!(e))
    }

    /// Dial, keeping the typed error (for tests of refusals).
    pub async fn try_connect(&self) -> std::result::Result<Conn, ConnectError> {
        let h = self
            .host()
            .map_err(|e| ConnectError::Other(e.to_string()))?;
        connect(&h.pin, &self.relay_key, Role::Device, &h.fp, &self.opts).await
    }

    /// A live session over `conn`: reconnect handshake, then hello.
    pub async fn live(&self, conn: &mut Conn) -> Result<Live> {
        let h = self.host()?;
        let (ini, msg1) = Initiator::reconnect(&self.static_key, &h.static_key, &conn.relay, None)
            .map_err(|e| anyhow!("{e}"))?;
        conn.send_live(msg1).await?;
        let msg2 = conn.recv_live(WAIT).await?;
        let est = ini.finish(&msg2).map_err(|e| anyhow!("reconnect: {e}"))?;
        let mut live = Live::new(est.into_session().map_err(|e| anyhow!("{e}"))?);
        live.send_json(conn, &json!({"event": "hello"})).await?;
        let hello = live.event(conn, "hello", WAIT).await?;
        live.hello = hello;
        Ok(live)
    }

    /// Key a mailbox over a live session (the device keeps it).
    pub async fn key_mailbox(&mut self, conn: &mut Conn, live: &mut Live) -> Result<()> {
        let h = self.host()?.clone();
        let (ini, msg1) = Initiator::mailbox(&self.static_key, &h.static_key, &conn.relay, None)
            .map_err(|e| anyhow!("{e}"))?;
        live.send_json(conn, &json!({"event": "mailbox_key", "msg": b64(&msg1)}))
            .await?;
        let ans = live.event(conn, "mailbox_key", WAIT).await?;
        let msg2 = unb64(ans["msg"].as_str().unwrap_or(""))?;
        let mut m = ini
            .finish(&msg2)
            .and_then(|e| e.into_mailbox())
            .map_err(|e| anyhow!("mailbox keying: {e}"))?;
        let first = m
            .encrypt(
                &stream::fragment(CONTROL, br#"{"event":"mailbox_ready"}"#)
                    .map_err(|e| anyhow!("{e}"))?[0],
            )
            .map_err(|e| anyhow!("{e}"))?;
        let mut framed = m.id().to_vec();
        framed.extend_from_slice(&first);
        live.send_json(
            conn,
            &json!({"event": "mailbox_ready", "frame": b64(&framed)}),
        )
        .await?;
        self.mailbox = Some(m);
        self.mailbox_seen = None;
        Ok(())
    }

    /// A request as a lane-2 payload (the mailbox advances).
    pub fn mailbox_request(&mut self, id: &str, op: &str, args: Value) -> Result<Vec<u8>> {
        let m = self.mailbox.as_mut().context("no mailbox")?;
        let body = json!({"id": id, "op": op, "args": args}).to_string();
        let f = stream::fragment(CONTROL, body.as_bytes()).map_err(|e| anyhow!("{e}"))?;
        if f.len() != 1 {
            bail!("a mailbox request must fit one frame");
        }
        let frame = m.encrypt(&f[0]).map_err(|e| anyhow!("{e}"))?;
        let mut p = m.id().to_vec();
        p.extend_from_slice(&frame);
        Ok(p)
    }

    /// A lane-2 frame the relay delivered (`boot` from this connection's
    /// `ready`): None when it was opened already (delivered again), else
    /// the host's message.
    pub fn mailbox_receive(
        &mut self,
        boot: &str,
        seq: u64,
        payload: &[u8],
    ) -> Result<Option<Value>> {
        if self
            .mailbox_seen
            .as_ref()
            .is_some_and(|(b, s)| !boot.is_empty() && b == boot && seq <= *s)
        {
            return Ok(None);
        }
        let v = self.mailbox_open(payload)?;
        self.mailbox_seen = Some((boot.to_string(), seq));
        Ok(Some(v))
    }

    /// A lane-2 payload from the host, opened.
    pub fn mailbox_open(&mut self, payload: &[u8]) -> Result<Value> {
        let m = self.mailbox.as_mut().context("no mailbox")?;
        if payload.len() < 8 || payload[..8] != m.id() {
            bail!("a frame for another mailbox");
        }
        let plain = m.decrypt(&payload[8..]).map_err(|e| anyhow!("{e}"))?;
        let msg = stream::single(&plain).map_err(|e| anyhow!("{e}"))?;
        Ok(serde_json::from_slice(&msg.body)?)
    }
}

/// A pairing waiting for the host's user.
pub struct PairingRun {
    pub conn: Conn,
    pub live: Live,
    /// The code this device shows.
    pub code: String,
    /// The code the host said it shows.
    pub host_code: String,
    pin: RelayAddress,
    host_static: [u8; 32],
}

impl PairingRun {
    /// The host's answer: `done` stores the host on `device`; anything else
    /// is an error naming it.
    pub async fn outcome(mut self, device: &mut Device, limit: Duration) -> Result<Host> {
        let ev = self.live.event(&mut self.conn, "pair", limit).await?;
        if ev["state"] != "done" {
            bail!("pairing {}", ev["state"].as_str().unwrap_or("failed"));
        }
        let fp = fingerprint_from_b64(ev["host_key"].as_str().unwrap_or(""))
            .map_err(|e| anyhow!("{e}"))?;
        let host = Host {
            static_key: self.host_static,
            pin: self.pin,
            fp,
            name: ev["host"].as_str().unwrap_or("").to_string(),
        };
        device.host = Some(host.clone());
        self.conn.close().await;
        Ok(host)
    }
}

/// A live session's control stream, from the device's side.
pub struct Live {
    pub session: Session,
    reasm: Reassembler,
    /// Events (and answers) not yet taken.
    pub inbox: VecDeque<Value>,
    next_id: u64,
    /// The host's hello (after `Device::live`).
    pub hello: Value,
}

impl Live {
    pub fn new(session: Session) -> Live {
        Live {
            session,
            reasm: Reassembler::new(),
            inbox: VecDeque::new(),
            next_id: 0,
            hello: Value::Null,
        }
    }

    pub async fn send_json(&mut self, conn: &mut Conn, v: &Value) -> Result<()> {
        for f in stream::fragment(CONTROL, v.to_string().as_bytes()).map_err(|e| anyhow!("{e}"))? {
            let w = self.session.encrypt(&f).map_err(|e| anyhow!("{e}"))?;
            conn.send_live(w).await?;
        }
        Ok(())
    }

    /// The next whole control message.
    pub async fn recv_json(&mut self, conn: &mut Conn, limit: Duration) -> Result<Value> {
        let until = tokio::time::Instant::now() + limit;
        loop {
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            let w = conn.recv_live(left).await?;
            let p = self.session.decrypt(&w).map_err(|e| anyhow!("{e}"))?;
            if let Some(m) = self.reasm.push(&p).map_err(|e| anyhow!("{e}"))? {
                if m.stream != CONTROL {
                    bail!("a message on stream {}", m.stream);
                }
                return Ok(serde_json::from_slice(&m.body)?);
            }
        }
    }

    /// The next event named `name` (others stay in the inbox).
    pub async fn event(&mut self, conn: &mut Conn, name: &str, limit: Duration) -> Result<Value> {
        let until = tokio::time::Instant::now() + limit;
        loop {
            if let Some(i) = self.inbox.iter().position(|v| v["event"] == name) {
                return Ok(self.inbox.remove(i).expect("found"));
            }
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            let v = self.recv_json(conn, left).await?;
            self.inbox.push_back(v);
        }
    }

    /// Send a request; its answer (`{id, ok}` or `{id, error}`).
    pub async fn request(&mut self, conn: &mut Conn, op: &str, args: Value) -> Result<Value> {
        self.next_id += 1;
        let id = format!("r{}", self.next_id);
        self.send_json(conn, &json!({"id": id, "op": op, "args": args}))
            .await?;
        self.answer(conn, &id, Duration::from_secs(30)).await
    }

    /// The answer to request `id`.
    pub async fn answer(&mut self, conn: &mut Conn, id: &str, limit: Duration) -> Result<Value> {
        let until = tokio::time::Instant::now() + limit;
        loop {
            if let Some(i) = self.inbox.iter().position(|v| v["id"] == id) {
                return Ok(self.inbox.remove(i).expect("found"));
            }
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            let v = self.recv_json(conn, left).await?;
            self.inbox.push_back(v);
        }
    }
}
