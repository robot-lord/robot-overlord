//! A device's relay connection: one websocket to the pinned relay's one
//! URL, the signed challenge checked against the pin before anything else.
//! (The host's own client is `crates/web/src/remote/link.rs`; this one
//! also lets a test turn the pin check off or dial another URL, to show
//! what the other side then does.)

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use rlord_relay_proto::frame::{FromDevice, ToDevice};
use rlord_relay_proto::msg::{Hello, Ready, VERSION};
use rlord_relay_proto::{
    auth, check_challenge, FromRelay, Identity, RelayAddress, Role, ToRelay, WrongRelay, B64,
};

pub type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// How a test device dials.
#[derive(Clone, Debug, Default)]
pub struct Opts {
    /// Extra root certificate (PEM) for a relay with a private CA.
    pub ca_file: String,
    /// Off: accept whatever relay answers (adversarial tests only).
    pub skip_pin_check: bool,
    /// Dial this URL instead of the pin's (a DNS hijack, in a test).
    pub url: Option<String>,
}

/// Why a connection failed, when it matters to a test.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectError {
    WrongRelay(WrongRelay),
    /// The relay closed the socket with this code.
    Closed(u16, String),
    /// The upgrade was refused with this HTTP status (a redirect included:
    /// never followed).
    Http(u16),
    Bye(String, Option<u64>),
    Other(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ConnectError {}

fn tls(ca_file: &str) -> Result<rustls::ClientConfig> {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if !ca_file.is_empty() {
        for c in rustls::pki_types::CertificateDer::pem_file_iter(ca_file)? {
            roots.add(c?)?;
        }
    }
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

/// A live connection to a relay, after `ready`.
pub struct Conn {
    ws: Ws,
    pub ready: Ready,
    /// The relay this connection proved (what goes in a prologue).
    pub relay: rlord_e2e::RelayId,
    n: u64,
    /// Relay control messages not yet looked at (`host`, `signal`, `ok`…).
    pub control: VecDeque<FromRelay>,
    live: VecDeque<Vec<u8>>,
    mailbox: VecDeque<(u64, Vec<u8>)>,
    /// The close code, once the relay closed.
    pub closed: Option<u16>,
}

/// Dial `pin` as `role` with `id`, naming `target` (the host fingerprint
/// or the rendezvous id).
pub async fn connect(
    pin: &RelayAddress,
    id: &Identity,
    role: Role,
    target: &[u8],
    opts: &Opts,
) -> std::result::Result<Conn, ConnectError> {
    let other = |e: &dyn std::fmt::Display| ConnectError::Other(e.to_string());
    let url = opts.url.clone().unwrap_or_else(|| pin.ws_url());
    let connector = if url.starts_with("wss://") {
        Connector::Rustls(Arc::new(tls(&opts.ca_file).map_err(|e| other(&e))?))
    } else {
        Connector::Plain
    };
    let cfg = WebSocketConfig::default()
        .max_message_size(Some(64 * 1024))
        .max_frame_size(Some(64 * 1024));
    let (mut ws, resp) = match tokio_tungstenite::connect_async_tls_with_config(
        &url,
        Some(cfg),
        true,
        Some(connector),
    )
    .await
    {
        Ok(x) => x,
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => {
            return Err(ConnectError::Http(r.status().as_u16()))
        }
        Err(e) => return Err(other(&e)),
    };
    if resp.headers().contains_key("sec-websocket-extensions") {
        return Err(ConnectError::Other("an extension was negotiated".into()));
    }
    let nonce = crate::random32().map_err(|e| other(&e))?;
    let hello = Hello {
        v: VERSION,
        role,
        key: B64(id.public().to_vec()),
        nonce: B64(nonce.to_vec()),
        host: (role == Role::Device).then(|| B64(target.to_vec())),
        rendezvous: (role == Role::Pair).then(|| B64(target.to_vec())),
        enroll: None,
    };
    send_text(&mut ws, &ToRelay::Hello(hello))
        .await
        .map_err(|e| other(&e))?;
    let FromRelay::Challenge {
        origin,
        key,
        nonce: rn,
        sig,
    } = next_text(&mut ws, Duration::from_secs(5)).await?
    else {
        return Err(ConnectError::Other("no challenge".into()));
    };
    if !opts.skip_pin_check {
        if let Err(w) = check_challenge(pin, &id.public(), &nonce, &origin, &key.0, &rn.0, &sig.0) {
            let _ = ws.close(None).await;
            return Err(ConnectError::WrongRelay(w));
        }
    }
    let t = auth::peer_transcript(role, &origin, &key.0, &id.public(), &rn.0, &nonce, target);
    send_text(
        &mut ws,
        &ToRelay::Auth {
            sig: B64(id.sign(&t).to_vec()),
        },
    )
    .await
    .map_err(|e| other(&e))?;
    let ready = loop {
        match next_text(&mut ws, Duration::from_secs(5)).await? {
            FromRelay::Ready(r) => break r,
            FromRelay::Enroll { .. } => continue,
            FromRelay::Bye { code, retry_after } => {
                return Err(ConnectError::Bye(code, retry_after))
            }
            other => return Err(ConnectError::Other(format!("before ready: {other:?}"))),
        }
    };
    Ok(Conn {
        ws,
        ready,
        relay: rlord_e2e::RelayId::new(origin, auth::fingerprint(&key.0)),
        n: 0,
        control: VecDeque::new(),
        live: VecDeque::new(),
        mailbox: VecDeque::new(),
        closed: None,
    })
}

async fn send_text(ws: &mut Ws, m: &ToRelay) -> Result<()> {
    ws.send(Message::Text(m.to_json().into())).await?;
    Ok(())
}

async fn next_text(ws: &mut Ws, limit: Duration) -> std::result::Result<FromRelay, ConnectError> {
    loop {
        let m = tokio::time::timeout(limit, ws.next())
            .await
            .map_err(|_| ConnectError::Other("timed out".into()))?;
        match m {
            Some(Ok(Message::Text(t))) => {
                return FromRelay::parse(&t).map_err(|e| ConnectError::Other(e.to_string()))
            }
            Some(Ok(Message::Close(c))) => {
                let (code, why) = c
                    .map(|c| (u16::from(c.code), c.reason.to_string()))
                    .unwrap_or((1005, String::new()));
                return Err(ConnectError::Closed(code, why));
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(ConnectError::Other(e.to_string())),
            None => return Err(ConnectError::Closed(1006, String::new())),
        }
    }
}

impl Conn {
    /// Read one message from the relay into the queues. False once closed.
    async fn pump(&mut self, limit: Duration) -> Result<bool> {
        if self.closed.is_some() {
            return Ok(false);
        }
        let m = match tokio::time::timeout(limit, self.ws.next()).await {
            Err(_) => bail!("timed out waiting for the relay"),
            Ok(m) => m,
        };
        match m {
            Some(Ok(Message::Text(t))) => self.control.push_back(FromRelay::parse(&t)?),
            Some(Ok(Message::Binary(b))) => match ToDevice::decode(&b)? {
                ToDevice::Live(p) => self.live.push_back(p),
                ToDevice::Mailbox { seq, payload } => self.mailbox.push_back((seq, payload)),
            },
            Some(Ok(Message::Close(c))) => {
                self.closed = Some(c.map(|c| u16::from(c.code)).unwrap_or(1005));
                return Ok(false);
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => {
                self.closed = Some(1006);
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub async fn send(&mut self, m: impl FnOnce(u64) -> ToRelay) -> Result<u64> {
        self.n += 1;
        let n = self.n;
        send_text(&mut self.ws, &m(n)).await?;
        Ok(n)
    }

    pub async fn send_live(&mut self, payload: Vec<u8>) -> Result<()> {
        self.ws
            .send(Message::Binary(FromDevice::Live(payload).encode().into()))
            .await?;
        Ok(())
    }

    pub async fn send_mailbox(&mut self, payload: Vec<u8>) -> Result<()> {
        self.ws
            .send(Message::Binary(
                FromDevice::Mailbox(payload).encode().into(),
            ))
            .await?;
        Ok(())
    }

    /// The next live frame.
    pub async fn recv_live(&mut self, limit: Duration) -> Result<Vec<u8>> {
        let until = tokio::time::Instant::now() + limit;
        loop {
            if let Some(p) = self.live.pop_front() {
                return Ok(p);
            }
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            if !self.pump(left).await? {
                bail!("the relay closed the connection ({:?})", self.closed);
            }
        }
    }

    /// The next queued mailbox message: its sequence number and payload.
    pub async fn recv_mailbox(&mut self, limit: Duration) -> Result<(u64, Vec<u8>)> {
        let until = tokio::time::Instant::now() + limit;
        loop {
            if let Some(p) = self.mailbox.pop_front() {
                return Ok(p);
            }
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            if !self.pump(left).await? {
                bail!("the relay closed the connection ({:?})", self.closed);
            }
        }
    }

    /// The first relay control message that `pick` accepts (others stay
    /// queued).
    pub async fn wait_for(
        &mut self,
        limit: Duration,
        pick: impl Fn(&FromRelay) -> bool,
    ) -> Result<FromRelay> {
        let until = tokio::time::Instant::now() + limit;
        loop {
            if let Some(i) = self.control.iter().position(&pick) {
                return Ok(self.control.remove(i).expect("found"));
            }
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            if !self.pump(left).await? {
                bail!("the relay closed the connection ({:?})", self.closed);
            }
        }
    }

    /// Waits for the relay to close the connection; its code.
    pub async fn wait_closed(&mut self, limit: Duration) -> Result<u16> {
        let until = tokio::time::Instant::now() + limit;
        while self.closed.is_none() {
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            self.pump(left).await?;
        }
        self.closed.ok_or_else(|| anyhow!("not closed"))
    }

    /// Nothing more arrives within `limit` (the queues aside).
    pub async fn quiet(&mut self, limit: Duration) -> bool {
        let before = (self.live.len(), self.mailbox.len());
        let _ = self.pump(limit).await;
        before == (self.live.len(), self.mailbox.len())
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    /// The websocket itself (a forwarder pipes it).
    pub fn into_ws(self) -> Ws {
        self.ws
    }
}
