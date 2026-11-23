//! The relay link from the device's side: one websocket to the pinned
//! relay's one URL (`RelayAddress::ws_url`, never another), no redirects
//! (a 3xx is a failed connection), no `permessage-deflate` (none is
//! offered; one negotiated anyway is refused), TLS by rustls with ring
//! against `webpki-roots` plus an optional extra CA. Then the connect
//! sequence: `hello`, the relay's `challenge` checked by `relay-proto`'s
//! `check_challenge` before anything else is sent, `auth` signed by the
//! platform's key, `ready`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use rlord_relay_proto::msg::{Hello, VERSION};
use rlord_relay_proto::{
    auth, check_challenge, FromRelay, RelayAddress, Role, ToRelay, TranscriptSigner, B64,
};

use crate::machine::{DialError, Linked};
use crate::{PlatformError, Signers};

pub type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// From the upgrade to `ready`.
pub const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// TCP, TLS and the upgrade.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest websocket message accepted (a binary frame is at most 20 KiB).
pub const MAX_MESSAGE: usize = 64 * 1024;

/// TLS for the relay link: rustls with ring, `webpki-roots`, and `ca_pem`
/// (one or more PEM certificates) when given.
pub fn tls_config(ca_pem: Option<&str>) -> Result<rustls::ClientConfig, String> {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(pem) = ca_pem.filter(|p| !p.trim().is_empty()) {
        for c in rustls::pki_types::CertificateDer::pem_slice_iter(pem.as_bytes()) {
            let c = c.map_err(|e| format!("extra CA: {e}"))?;
            roots.add(c).map_err(|e| format!("extra CA: {e}"))?;
        }
    }
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

/// A platform key as a [`TranscriptSigner`].
pub struct KeySigner {
    pub signers: Arc<dyn Signers>,
    pub label: String,
    pub public: Vec<u8>,
}

impl KeySigner {
    pub fn new(signers: Arc<dyn Signers>, label: &str) -> Result<KeySigner, PlatformError> {
        let public = signers.public_key(label.to_string())?;
        Ok(KeySigner {
            signers,
            label: label.to_string(),
            public,
        })
    }
}

impl TranscriptSigner for KeySigner {
    fn public_key(&self) -> Vec<u8> {
        self.public.clone()
    }

    fn sign_message(&self, msg: &[u8]) -> Result<Vec<u8>, String> {
        self.signers
            .sign(self.label.clone(), msg.to_vec())
            .map_err(|e| e.message)
    }
}

fn random32() -> Result<[u8; 32], DialError> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| DialError::Other(format!("random: {e}")))?;
    Ok(b)
}

/// Opens the websocket at `url`: no redirect followed, no extension.
pub async fn open(url: &str, tls: Arc<rustls::ClientConfig>) -> Result<Ws, DialError> {
    let connector = if url.starts_with("wss://") {
        Connector::Rustls(tls)
    } else {
        Connector::Plain
    };
    let cfg = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let r = tokio::time::timeout(
        OPEN_TIMEOUT,
        tokio_tungstenite::connect_async_tls_with_config(url, Some(cfg), true, Some(connector)),
    )
    .await
    .map_err(|_| DialError::Other("timed out connecting".into()))?;
    let (ws, resp) = match r {
        Ok(x) => x,
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => {
            return Err(DialError::Http(r.status().as_u16()))
        }
        Err(e) => return Err(DialError::Other(e.to_string())),
    };
    if resp.headers().contains_key("sec-websocket-extensions") {
        return Err(DialError::Other("the relay negotiated an extension".into()));
    }
    Ok(ws)
}

/// Dials `pin` as `role` naming `target` (the host's fingerprint, or the
/// rendezvous id), signing with `signer`. `enroll` hears an `enroll` from
/// the relay (shown while waiting for `ready`).
pub async fn connect(
    pin: &RelayAddress,
    signer: &(dyn TranscriptSigner + Sync),
    role: Role,
    target: &[u8],
    tls: Arc<rustls::ClientConfig>,
    mut enroll: impl FnMut(String, String) + Send,
) -> Result<(Ws, Linked), DialError> {
    let mut ws = open(&pin.ws_url(), tls).await?;
    let public = signer.public_key();
    let nonce = random32()?;
    let hello = ToRelay::Hello(Hello {
        v: VERSION,
        role,
        key: B64(public.clone()),
        nonce: B64(nonce.to_vec()),
        host: (role == Role::Device).then(|| B64(target.to_vec())),
        rendezvous: (role == Role::Pair).then(|| B64(target.to_vec())),
        enroll: None,
    });
    send(&mut ws, &hello).await?;
    let until = tokio::time::Instant::now() + READY_TIMEOUT;
    let FromRelay::Challenge {
        origin,
        key,
        nonce: rn,
        sig,
    } = next(&mut ws, until).await?
    else {
        return Err(DialError::Other("no challenge".into()));
    };
    if let Err(w) = check_challenge(pin, &public, &nonce, &origin, &key.0, &rn.0, &sig.0) {
        let _ = ws.close(None).await;
        return Err(DialError::WrongRelay(w.to_string()));
    }
    let t = auth::peer_transcript(role, &origin, &key.0, &public, &rn.0, &nonce, target);
    let sig = auth::sign_with(signer, &t)
        .map_err(|_| DialError::Other("the key store could not sign".into()))?;
    send(
        &mut ws,
        &ToRelay::Auth {
            sig: B64(sig.to_vec()),
        },
    )
    .await?;
    let mut until = tokio::time::Instant::now() + READY_TIMEOUT;
    let ready = loop {
        match next(&mut ws, until).await? {
            FromRelay::Ready(r) => break r,
            FromRelay::Enroll { url, code, expires } => {
                enroll(url, code);
                let now_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                until = tokio::time::Instant::now()
                    + Duration::from_secs(expires.saturating_sub(now_unix).min(15 * 60));
            }
            FromRelay::Bye { retry_after, .. } => return Err(DialError::Bye { retry_after }),
            other => return Err(DialError::Other(format!("before ready: {other:?}"))),
        }
    };
    let relay = rlord_e2e::RelayId::new(origin, auth::fingerprint(&key.0));
    Ok((ws, Linked { ready, relay }))
}

async fn send(ws: &mut Ws, m: &ToRelay) -> Result<(), DialError> {
    ws.send(Message::Text(m.to_json().into()))
        .await
        .map_err(|e| DialError::Other(e.to_string()))
}

async fn next(ws: &mut Ws, until: tokio::time::Instant) -> Result<FromRelay, DialError> {
    loop {
        let m = tokio::time::timeout_at(until, ws.next())
            .await
            .map_err(|_| DialError::Other("timed out waiting for the relay".into()))?;
        match m {
            Some(Ok(Message::Text(t))) => {
                return FromRelay::parse(&t).map_err(|e| DialError::Other(e.to_string()))
            }
            Some(Ok(Message::Close(c))) => {
                return Err(DialError::Closed(
                    c.map(|c| u16::from(c.code)).unwrap_or(1005),
                ))
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(DialError::Other(e.to_string())),
            None => return Err(DialError::Closed(1006)),
        }
    }
}
