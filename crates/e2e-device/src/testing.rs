//! Hostile relays for the adversarial tests, built on `relay-proto`. Each
//! listens on 127.0.0.1 only and counts what it sees, so a test can show
//! that nothing was sent after a failed pin check and that no other
//! address was tried.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use rlord_relay_proto::msg::Ready;
use rlord_relay_proto::{auth, FromRelay, Identity, RelayAddress, Role, ToRelay, B64};

use crate::client::{connect, Opts};

/// A hostile relay running in the background.
pub struct Hostile {
    pub addr: SocketAddr,
    /// Connections accepted.
    pub connections: Arc<AtomicUsize>,
    /// Messages a peer sent after the challenge.
    pub after_challenge: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Hostile {
    pub fn url(&self) -> String {
        format!("ws://{}/v1", self.addr)
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub fn after_challenge(&self) -> usize {
        self.after_challenge.load(Ordering::SeqCst)
    }
}

impl Drop for Hostile {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn challenge(
    origin: &str,
    shown_key: &[u8],
    signer: &Identity,
    hello: &ToRelay,
) -> Option<(FromRelay, Vec<u8>)> {
    let ToRelay::Hello(h) = hello else {
        return None;
    };
    let nonce = crate::random32().ok()?;
    let t = auth::relay_transcript(origin, shown_key, &h.key.0, &h.nonce.0, &nonce);
    Some((
        FromRelay::Challenge {
            origin: origin.to_string(),
            key: B64(shown_key.to_vec()),
            nonce: B64(nonce.to_vec()),
            sig: B64(signer.sign(&t).to_vec()),
        },
        nonce.to_vec(),
    ))
}

/// A relay at `origin` that shows `shown_key` (say, the pinned key) but
/// signs with `signer` (it lacks the private key).
pub async fn claiming(origin: &str, shown_key: Vec<u8>, signer: Identity) -> Hostile {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (connections, after) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (c2, a2, origin) = (connections.clone(), after.clone(), origin.to_string());
    let task = tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            c2.fetch_add(1, Ordering::SeqCst);
            let (a2, origin, shown, signer) = (
                a2.clone(),
                origin.clone(),
                shown_key.clone(),
                signer.clone(),
            );
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(s).await else {
                    return;
                };
                let Some(Ok(Message::Text(t))) = ws.next().await else {
                    return;
                };
                let Ok(hello) = ToRelay::parse(&t) else {
                    return;
                };
                let Some((c, _)) = challenge(&origin, &shown, &signer, &hello) else {
                    return;
                };
                let _ = ws.send(Message::Text(c.to_json().into())).await;
                while let Ok(Some(Ok(m))) =
                    tokio::time::timeout(Duration::from_secs(5), ws.next()).await
                {
                    if matches!(m, Message::Text(_) | Message::Binary(_)) {
                        a2.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
        }
    });
    Hostile {
        addr,
        connections,
        after_challenge: after,
        task,
    }
}

/// A faithful forwarder under its own identity: it answers a device as a
/// relay at `origin` with `own`'s key, dials the real relay (`upstream`)
/// in the same role with that same key, and passes every frame through
/// untouched. A device that skipped its pin check reaches the host this
/// way, and the handshake must still fail on both ends.
pub async fn forwarder(origin: &str, own: Identity, upstream: RelayAddress) -> Hostile {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (connections, after) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (c2, a2, origin) = (connections.clone(), after.clone(), origin.to_string());
    let task = tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            c2.fetch_add(1, Ordering::SeqCst);
            let (a2, origin, own, upstream) =
                (a2.clone(), origin.clone(), own.clone(), upstream.clone());
            tokio::spawn(async move {
                let Ok(mut down) = tokio_tungstenite::accept_async(s).await else {
                    return;
                };
                let Some(Ok(Message::Text(t))) = down.next().await else {
                    return;
                };
                let Ok(hello) = ToRelay::parse(&t) else {
                    return;
                };
                let ToRelay::Hello(h) = &hello else {
                    return;
                };
                let (role, target) = match h.role {
                    Role::Pair => (Role::Pair, h.rendezvous.clone().unwrap_or_default().0),
                    r => (r, h.host.clone().unwrap_or_default().0),
                };
                let Some((c, _)) = challenge(&origin, &own.public(), &own, &hello) else {
                    return;
                };
                let _ = down.send(Message::Text(c.to_json().into())).await;
                // The device's auth (not checked: we are not the relay).
                let _ = down.next().await;
                a2.fetch_add(1, Ordering::SeqCst);
                let Ok(up) = connect(&upstream, &own, role, &target, &Opts::default()).await else {
                    let _ = down.close(None).await;
                    return;
                };
                let ready = FromRelay::Ready(Ready {
                    boot: up.ready.boot.clone(),
                    ..up.ready.clone()
                });
                let _ = down.send(Message::Text(ready.to_json().into())).await;
                let mut up = up.into_ws();
                loop {
                    tokio::select! {
                        m = down.next() => match m {
                            Some(Ok(m @ (Message::Binary(_) | Message::Text(_)))) => {
                                if up.send(m).await.is_err() { break; }
                            }
                            Some(Ok(_)) => {}
                            _ => break,
                        },
                        m = up.next() => match m {
                            Some(Ok(m @ (Message::Binary(_) | Message::Text(_)))) => {
                                if down.send(m).await.is_err() { break; }
                            }
                            Some(Ok(_)) => {}
                            _ => break,
                        },
                    }
                }
            });
        }
    });
    Hostile {
        addr,
        connections,
        after_challenge: after,
        task,
    }
}

/// An HTTP server that answers every upgrade with a redirect to `to`.
pub async fn redirect(to: &str) -> Hostile {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let (connections, after) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (c2, to) = (connections.clone(), to.to_string());
    let task = tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            c2.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let _ = s
                .write_all(
                    format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n")
                        .as_bytes(),
                )
                .await;
        }
    });
    Hostile {
        addr,
        connections,
        after_challenge: after,
        task,
    }
}
