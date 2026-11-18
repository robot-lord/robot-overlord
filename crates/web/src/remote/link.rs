//! The host's connection to its pinned relay: one websocket, dialled out,
//! authenticated by the signed challenge (`docs/working/e2e-protocol.md`,
//! "Connecting").
//!
//! Exactly one URL, built from the pin; tungstenite never follows a
//! redirect (a 3xx fails the upgrade) and offers no `permessage-deflate`.
//! TLS validates the origin's host against `webpki-roots` plus `[remote]
//! ca_file`. Plain `ws://` only for a loopback `http` origin (the address
//! parser refuses any other).

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use rlord_relay_proto::msg::{close, Enroll, Hello, Ready, VERSION};
use rlord_relay_proto::{
    auth, check_challenge, FromRelay, Identity, RelayAddress, Role, ToRelay, B64,
};

pub type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// From upgrade to `ready` (an `enroll` from the relay pauses it).
const AUTH_LIMIT: Duration = Duration::from_secs(5);
/// Frames on the relay link: 20 KiB binary, 4 KiB text; with room.
const MAX_MESSAGE: usize = 64 * 1024;

/// Why a connection didn't come up.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// Not the pinned relay (origin, key or signature). Never retried
    /// until the configuration changes.
    WrongRelay(rlord_relay_proto::WrongRelay),
    /// Refused for good (bad signature, not registered, suspended, another
    /// protocol version): retried only after a change.
    Final { code: u16, why: String },
    /// Try again later (no sooner than `after` when the relay said so).
    Retry {
        after: Option<Duration>,
        why: String,
    },
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::WrongRelay(w) => write!(f, "{w}"),
            Failure::Final { why, .. } | Failure::Retry { why, .. } => f.write_str(why),
        }
    }
}

fn retry(why: impl Into<String>) -> Failure {
    Failure::Retry {
        after: None,
        why: why.into(),
    }
}

/// A close code from the relay, as a failure.
pub fn closed(code: u16, reason: &str) -> Failure {
    let why = match code {
        close::BAD_SIGNATURE => "the relay refused our signature".to_string(),
        close::SUSPENDED => "the relay suspended this host's account".to_string(),
        close::NOT_ALLOWED => "the relay doesn't know this host (it needs an enrolment token: \
             `rlord relay setup --reset <address> --token <token>`)"
            .to_string(),
        close::UNSUPPORTED => "the relay speaks another protocol version".to_string(),
        close::REPLACED => "another connection with this host's key replaced this one".to_string(),
        close::OVERLOADED => "the relay is overloaded".to_string(),
        close::LIMIT => "over a limit at the relay".to_string(),
        close::TOO_SLOW => "the relay timed the connection out".to_string(),
        close::RESET => "the relay reset the connection's link".to_string(),
        _ if reason.is_empty() => format!("the relay closed the connection ({code})"),
        _ => format!("the relay closed the connection ({code}: {reason})"),
    };
    if close::is_final(code) {
        Failure::Final { code, why }
    } else {
        retry(why)
    }
}

/// The TLS setup: ring, webpki's roots, plus the PEM certificates in
/// `ca_file` (a private CA) when set.
pub fn tls_config(ca_file: &str) -> Result<rustls::ClientConfig, String> {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if !ca_file.trim().is_empty() {
        let certs = rustls::pki_types::CertificateDer::pem_file_iter(ca_file.trim())
            .map_err(|e| format!("[remote] ca_file {ca_file}: {e}"))?;
        for c in certs {
            let c = c.map_err(|e| format!("[remote] ca_file {ca_file}: {e}"))?;
            roots
                .add(c)
                .map_err(|e| format!("[remote] ca_file {ca_file}: {e}"))?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(cfg)
}

/// Open the websocket to the pinned relay's one URL.
pub async fn open(pin: &RelayAddress, ca_file: &str) -> Result<Ws, Failure> {
    let url = pin.ws_url();
    let connector = if url.starts_with("wss://") {
        Connector::Rustls(Arc::new(
            tls_config(ca_file).map_err(|e| Failure::Final { code: 0, why: e })?,
        ))
    } else {
        Connector::Plain
    };
    let cfg = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let open =
        tokio_tungstenite::connect_async_tls_with_config(&url, Some(cfg), true, Some(connector));
    match tokio::time::timeout(Duration::from_secs(15), open).await {
        Err(_) => Err(retry(format!("{url}: no answer"))),
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(r))) => {
            let status = r.status();
            let after = r
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok())
                .map(Duration::from_secs);
            Err(Failure::Retry {
                after,
                why: if status.is_redirection() {
                    format!("{url} answered with a redirect, which is never followed")
                } else {
                    format!("{url} answered HTTP {status}")
                },
            })
        }
        Ok(Err(e)) => Err(retry(format!("{url}: {e}"))),
        Ok(Ok((ws, resp))) => {
            if resp.headers().contains_key("sec-websocket-extensions") {
                return Err(retry(format!(
                    "{url} negotiated an extension; none is allowed"
                )));
            }
            Ok(ws)
        }
    }
}

/// What the relay said instead of `ready`, worth showing the user.
#[derive(Debug, Clone, PartialEq)]
pub struct EnrollPrompt {
    pub url: String,
    pub code: String,
    pub expires: u64,
}

/// Read the next control message (pings answered by tungstenite).
async fn next_control(ws: &mut Ws) -> Result<FromRelay, Failure> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                return FromRelay::parse(&t).map_err(|e| retry(format!("relay: {e}")))
            }
            Some(Ok(Message::Close(c))) => {
                return Err(match c {
                    Some(c) => closed(u16::from(c.code), &c.reason),
                    None => retry("the relay closed the connection"),
                })
            }
            Some(Ok(Message::Binary(_))) => return Err(retry("relay: binary before ready")),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(retry(format!("relay: {e}"))),
            None => return Err(retry("the relay closed the connection")),
        }
    }
}

/// The connect sequence for any role: hello, check the challenge against
/// the pin before anything else, auth, ready. `on_enroll` is told when the
/// relay asks the user to approve this host first.
#[allow(clippy::too_many_arguments)]
pub async fn authenticate(
    ws: &mut Ws,
    pin: &RelayAddress,
    id: &Identity,
    nonce: [u8; 32],
    role: Role,
    target: &[u8],
    enroll: Option<Enroll>,
    on_enroll: &mut (dyn FnMut(EnrollPrompt) + Send),
) -> Result<Ready, Failure> {
    let hello = Hello {
        v: VERSION,
        role,
        key: B64(id.public().to_vec()),
        nonce: B64(nonce.to_vec()),
        host: (role == Role::Device).then(|| B64(target.to_vec())),
        rendezvous: (role == Role::Pair).then(|| B64(target.to_vec())),
        enroll,
    };
    let send = |m: ToRelay| Message::Text(m.to_json().into());
    ws.send(send(ToRelay::Hello(hello)))
        .await
        .map_err(|e| retry(format!("relay: {e}")))?;
    let challenge = tokio::time::timeout(AUTH_LIMIT, next_control(ws))
        .await
        .map_err(|_| retry("the relay sent no challenge"))??;
    let FromRelay::Challenge {
        origin,
        key,
        nonce: relay_nonce,
        sig,
    } = challenge
    else {
        return Err(retry("the relay didn't start with a challenge"));
    };
    if let Err(w) = check_challenge(
        pin,
        &id.public(),
        &nonce,
        &origin,
        &key.0,
        &relay_nonce.0,
        &sig.0,
    ) {
        let _ = ws.close(None).await;
        return Err(Failure::WrongRelay(w));
    }
    let t = auth::peer_transcript(
        role,
        &origin,
        &key.0,
        &id.public(),
        &relay_nonce.0,
        &nonce,
        target,
    );
    ws.send(send(ToRelay::Auth {
        sig: B64(id.sign(&t).to_vec()),
    }))
    .await
    .map_err(|e| retry(format!("relay: {e}")))?;
    let mut limit = AUTH_LIMIT;
    loop {
        let m = tokio::time::timeout(limit, next_control(ws))
            .await
            .map_err(|_| retry("the relay never said ready"))??;
        match m {
            FromRelay::Ready(r) => return Ok(r),
            FromRelay::Enroll { url, code, expires } => {
                on_enroll(EnrollPrompt { url, code, expires });
                // Wait while the user approves (until it expires, at most
                // 15 minutes).
                limit = Duration::from_secs(900);
            }
            FromRelay::Bye { code, retry_after } => {
                return Err(Failure::Retry {
                    after: retry_after.map(Duration::from_secs),
                    why: format!("the relay said bye ({code})"),
                })
            }
            _ => return Err(retry("the relay sent something before ready")),
        }
    }
}

/// A close frame for the relay link.
pub fn close_frame() -> Message {
    Message::Close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
        code: CloseCode::Normal,
        reason: "".into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_map() {
        assert!(matches!(
            closed(4403, ""),
            Failure::Final { code: 4403, .. }
        ));
        assert!(matches!(closed(4426, ""), Failure::Final { .. }));
        assert!(matches!(closed(4409, ""), Failure::Retry { .. }));
        assert!(matches!(closed(4503, ""), Failure::Retry { .. }));
        assert!(closed(4000, "x").to_string().contains("4000: x"));
    }

    #[test]
    fn tls_config_takes_a_ca_file() {
        assert!(tls_config("").is_ok());
        assert!(tls_config("/nonexistent/ca.pem").is_err());
    }
}
