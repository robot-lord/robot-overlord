//! One websocket connection: the signed connect sequence, then the read
//! loop. A separate writer task owns the socket's sending half, so a peer
//! that waits for room at another peer never stops its own outgoing
//! messages (two peers forwarding to each other can't deadlock).
//!
//! Backpressure, never dropping: a live frame waits for one of the
//! receiver's [`OUT_FRAMES`](crate::hub::OUT_FRAMES) permits, and while it
//! waits this connection is not read. A receiver that takes nothing for
//! `stall` is closed (`bye` `idle`, then 4408) and the sender told
//! `undeliverable`.

use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use rlord_relay_proto::msg::{close, code, FromRelay, Hello, ToRelay, MAX_CONTROL, VERSION};
use rlord_relay_proto::{auth, short, Role, B64};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::hub::{Hub, Out, Outbox, Peer, Route};
use crate::meter::Tally;
use crate::policy::{Decision, HostRequest};
use crate::registry::{Fp, Status};

/// Holds one of the relay's `max_connections` slots.
pub struct ConnGuard(pub Arc<Hub>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.conns.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A peer the connect sequence let in.
struct Admitted {
    role: Role,
    fp: Fp,
    host: Fp,
    rendezvous: Option<[u8; 16]>,
    account: String,
    limits: crate::policy::Limits,
}

/// Why the connect sequence ended without `ready`.
struct Refused {
    close: u16,
    reason: &'static str,
    bye: Option<(&'static str, Option<u64>)>,
}

impl Refused {
    fn new(close: u16, reason: &'static str) -> Refused {
        Refused {
            close,
            reason,
            bye: None,
        }
    }
}

type Stream = SplitStream<WebSocket>;

pub async fn run(hub: Arc<Hub>, ws: WebSocket, ip: IpAddr, guard: ConnGuard) {
    let (sink, mut stream) = ws.split();
    let (out, ends) = Outbox::new(hub.next_id());
    let tally: Arc<OnceLock<Tally>> = Arc::default();
    let mut kill = ends.kill.clone();
    let writer = tokio::spawn(write_loop(
        sink,
        ends.rx,
        ends.pending,
        hub.cfg.timeouts.ping,
        tally.clone(),
        out.clone(),
    ));
    session(&hub, &mut stream, &out, &mut kill, ip, &tally).await;
    // Ends the writer after what is queued (a no-op after a kill, whose
    // close frame is already queued).
    out.kill(1000, "", None);
    // Let the writer send what is queued (the close frame last), then
    // wait a moment for the peer's close reply before dropping the socket:
    // a peer still writing (a pong) to a closed socket would get a reset
    // and lose the `bye` and close code unread.
    let abort = writer.abort_handle();
    let linger = Duration::from_secs(1);
    if tokio::time::timeout(linger, writer).await.is_err() {
        abort.abort();
    }
    let _ = tokio::time::timeout(linger, async {
        while let Some(Ok(m)) = stream.next().await {
            if matches!(m, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
    drop(guard);
}

async fn session(
    hub: &Arc<Hub>,
    stream: &mut Stream,
    out: &Outbox,
    kill: &mut watch::Receiver<bool>,
    ip: IpAddr,
    tally: &OnceLock<Tally>,
) {
    let retry = Some(hub.cfg.timeouts.retry_after);
    let Some(unauth) = hub.unauth_enter(ip) else {
        tracing::debug!(%ip, "too many unauthenticated connections");
        out.kill(
            close::LIMIT,
            "too many connections from this address",
            Some((code::LIMIT, retry)),
        );
        return;
    };
    let mut shutdown = hub.shutdown.clone();
    let admitted = tokio::select! {
        biased;
        _ = shutdown.wait_for(|s| *s) => Err(Refused {
            close: close::OVERLOADED,
            reason: "restarting",
            bye: Some((code::OVERLOADED, retry)),
        }),
        _ = kill.changed() => return,
        r = connect(hub, stream, out, ip) => r,
    };
    let a = match admitted {
        Ok(a) => a,
        Err(r) => {
            out.kill(r.close, r.reason, r.bye);
            return;
        }
    };
    let peer = Peer {
        tally: hub.meter.open(&a.account, &a.fp),
        role: a.role,
        fp: a.fp,
        host: a.host,
        rendezvous: a.rendezvous,
        account: a.account,
        limits: a.limits,
        out: out.clone(),
    };
    let _ = tally.set(peer.tally.clone());
    if let Err(c) = hub.register(&peer) {
        tracing::info!(role = peer.role.as_str(), key = %short(&peer.fp), code = c, "refused");
        out.kill(c, "refused", None);
        peer.tally.closed();
        return;
    }
    drop(unauth);
    tracing::info!(
        role = peer.role.as_str(),
        key = %short(&peer.fp),
        host = %short(&peer.host),
        account = %peer.account,
        "connected"
    );
    serve(hub, &peer, stream, kill).await;
    hub.unregister(&peer);
    peer.tally.closed();
    tracing::info!(role = peer.role.as_str(), key = %short(&peer.fp), "gone");
}

/// Reads the next text message, skipping pings; `None` when the peer went
/// or sent something else.
async fn next_text(stream: &mut Stream) -> Result<String, Refused> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(t))) => {
                if t.len() > MAX_CONTROL {
                    return Err(Refused::new(close::BAD_SIGNATURE, "message too large"));
                }
                return Ok(t.to_string());
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(Message::Binary(_))) => {
                return Err(Refused::new(close::BAD_SIGNATURE, "not authenticated"))
            }
            _ => return Err(Refused::new(1000, "gone")),
        }
    }
}

/// The connect sequence: `hello`, `challenge`, `auth`, then admission. The
/// signed part has `timeouts.auth`; a pending enrolment may wait longer.
async fn connect(
    hub: &Arc<Hub>,
    stream: &mut Stream,
    out: &Outbox,
    ip: IpAddr,
) -> Result<Admitted, Refused> {
    let hello = tokio::time::timeout(hub.cfg.timeouts.auth, prove(hub, stream, out))
        .await
        .map_err(|_| Refused::new(close::TOO_SLOW, "too slow"))??;
    let fp = auth::fingerprint(&hello.key.0);
    let retry = Some(hub.cfg.timeouts.retry_after);
    let store_failed = |e: anyhow::Error| {
        tracing::warn!("store: {e:#}");
        Refused {
            close: close::OVERLOADED,
            reason: "relay error",
            bye: Some((code::OVERLOADED, retry)),
        }
    };
    let role = hello.role;
    let peer = |host: Fp, rendezvous, account: String| Admitted {
        role,
        fp,
        host,
        rendezvous,
        limits: hub.auth.limits(&account),
        account,
    };
    match hello.role {
        Role::Host => {
            let req = HostRequest {
                fingerprint: &fp,
                enroll: hello.enroll.as_ref(),
                addr: ip,
            };
            let mut shown = None;
            loop {
                match hub.auth.host(&req).map_err(store_failed)? {
                    Decision::Admit { account, limits } => {
                        // Whatever the authorizer, a host has a record:
                        // its devices are admitted against it.
                        if hub.registry.host(&fp).map_err(store_failed)?.is_none() {
                            hub.registry.add_host(&fp, &account).map_err(store_failed)?;
                        }
                        return Ok(Admitted {
                            limits,
                            ..peer(fp, None, account)
                        });
                    }
                    Decision::Refuse(c) => {
                        tracing::info!(key = %short(&fp), code = c, "host refused");
                        return Err(Refused {
                            close: c,
                            reason: "refused",
                            bye: (c == close::SUSPENDED).then_some((code::SUSPENDED, None)),
                        });
                    }
                    Decision::Pending { url, code, expires } => {
                        if crate::store::now() >= expires {
                            return Err(Refused::new(close::TOO_SLOW, "enrolment expired"));
                        }
                        let m = FromRelay::Enroll { url, code, expires };
                        if shown.as_ref() != Some(&m) {
                            out.send(&m);
                            shown = Some(m);
                        }
                        // Wait, but notice a peer that leaves.
                        tokio::select! {
                            _ = tokio::time::sleep(hub.cfg.timeouts.enroll_poll) => {}
                            m = stream.next() => match m {
                                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                                _ => return Err(Refused::new(1000, "gone")),
                            },
                        }
                    }
                }
            }
        }
        Role::Device => {
            let host: Fp = hello
                .host
                .as_ref()
                .and_then(|h| h.as_array())
                .ok_or(Refused::new(close::BAD_SIGNATURE, "bad hello"))?;
            let Some((rec, _)) = hub.registry.host(&host).map_err(store_failed)? else {
                return Err(Refused::new(close::NOT_ALLOWED, "unknown host"));
            };
            if rec.status == Status::Suspended {
                return Err(Refused {
                    close: close::SUSPENDED,
                    reason: "suspended",
                    bye: Some((code::SUSPENDED, None)),
                });
            }
            if !hub.registry.is_device(&host, &fp).map_err(store_failed)? {
                tracing::info!(key = %short(&fp), host = %short(&host), "device not on the allow-list");
                return Err(Refused::new(close::NOT_ALLOWED, "not on the host's list"));
            }
            Ok(peer(host, None, rec.account))
        }
        Role::Pair => {
            let rid: [u8; 16] = hello
                .rendezvous
                .as_ref()
                .and_then(|r| r.as_array())
                .ok_or(Refused::new(close::BAD_SIGNATURE, "bad hello"))?;
            // The rendezvous names the host; `register` checks it is open.
            let Some(host) = hub.rendezvous_host(&rid) else {
                return Err(Refused::new(close::NOT_ALLOWED, "no such rendezvous"));
            };
            let Some((rec, _)) = hub.registry.host(&host).map_err(store_failed)? else {
                return Err(Refused::new(close::NOT_ALLOWED, "no such rendezvous"));
            };
            if rec.status == Status::Suspended {
                return Err(Refused {
                    close: close::SUSPENDED,
                    reason: "suspended",
                    bye: Some((code::SUSPENDED, None)),
                });
            }
            Ok(peer(host, Some(rid), rec.account))
        }
    }
}

/// `hello`, `challenge`, `auth`: the peer's checked hello.
async fn prove(hub: &Hub, stream: &mut Stream, out: &Outbox) -> Result<Hello, Refused> {
    let bad = || Refused::new(close::BAD_SIGNATURE, "bad hello");
    let text = next_text(stream).await?;
    // The version first, so a newer client gets 4426 rather than a parse
    // error over fields this relay doesn't know.
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| bad())?;
    if v.get("t").and_then(|t| t.as_str()) != Some("hello") {
        return Err(bad());
    }
    match v.get("v").and_then(|v| v.as_u64()) {
        Some(n) if n == VERSION as u64 => {}
        Some(_) => {
            return Err(Refused::new(
                close::UNSUPPORTED,
                "unsupported protocol version",
            ))
        }
        None => return Err(bad()),
    }
    let Ok(ToRelay::Hello(hello)) = ToRelay::parse(&text) else {
        return Err(bad());
    };
    if auth::check_public(&hello.key.0).is_err() || hello.nonce.0.len() != auth::NONCE_LEN {
        return Err(bad());
    }
    let target: Vec<u8> = match hello.role {
        Role::Host => Vec::new(),
        Role::Device => match &hello.host {
            Some(h) if h.0.len() == 32 => h.0.clone(),
            _ => return Err(bad()),
        },
        Role::Pair => match &hello.rendezvous {
            Some(r) if r.0.len() == 16 => r.0.clone(),
            _ => return Err(bad()),
        },
    };
    let relay_key = hub.identity.public();
    let nonce: [u8; auth::NONCE_LEN] = crate::key::random().map_err(|_| bad())?;
    let sig = hub.identity.sign(&auth::relay_transcript(
        &hub.origin,
        &relay_key,
        &hello.key.0,
        &hello.nonce.0,
        &nonce,
    ));
    out.send(&FromRelay::Challenge {
        origin: hub.origin.clone(),
        key: B64(relay_key.to_vec()),
        nonce: B64(nonce.to_vec()),
        sig: B64(sig.to_vec()),
    });
    let text = next_text(stream).await?;
    let Ok(ToRelay::Auth { sig }) = ToRelay::parse(&text) else {
        return Err(Refused::new(close::BAD_SIGNATURE, "expected auth"));
    };
    let t = auth::peer_transcript(
        hello.role,
        &hub.origin,
        &relay_key,
        &hello.key.0,
        &nonce,
        &hello.nonce.0,
        &target,
    );
    if !auth::verify(&hello.key.0, &t, &sig.0) {
        tracing::info!(key = %short(&auth::fingerprint(&hello.key.0)), "bad signature");
        return Err(Refused::new(close::BAD_SIGNATURE, "bad signature"));
    }
    Ok(hello)
}

/// Bytes per second with a burst: a peer over it is read more slowly.
struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn new(rate: u64, burst: u64) -> Bucket {
        Bucket {
            rate: rate as f64,
            burst: burst as f64,
            tokens: burst as f64,
            at: Instant::now(),
        }
    }

    /// Takes `n` bytes; how long to wait before reading on.
    fn take(&mut self, n: usize) -> Duration {
        let now = Instant::now();
        self.tokens =
            (self.tokens + now.duration_since(self.at).as_secs_f64() * self.rate).min(self.burst);
        self.at = now;
        self.tokens -= n as f64;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.rate)
        }
    }
}

/// After `ready`: read until the peer goes, is closed or idles out.
async fn serve(hub: &Hub, p: &Peer, stream: &mut Stream, kill: &mut watch::Receiver<bool>) {
    let t = &hub.cfg.timeouts;
    let mut shutdown = hub.shutdown.clone();
    let mut bucket = Bucket::new(p.limits.bytes_per_sec, p.limits.burst_bytes);
    let mut deadline = Instant::now() + t.idle;
    loop {
        let msg = tokio::select! {
            biased;
            _ = kill.changed() => return,
            _ = shutdown.wait_for(|s| *s) => {
                p.out.kill(close::OVERLOADED, "restarting", Some((code::OVERLOADED, Some(t.retry_after))));
                return;
            }
            _ = tokio::time::sleep_until(deadline) => {
                tracing::debug!(key = %short(&p.fp), "idle");
                p.out.kill(close::TOO_SLOW, "idle", Some((code::IDLE, None)));
                return;
            }
            m = stream.next() => m,
        };
        let msg = match msg {
            Some(Ok(m)) => m,
            Some(Err(e)) => {
                // Over the frame limit (20 KiB) or not websocket at all.
                let too_big = e.to_string().contains("too long");
                p.out
                    .kill(if too_big { 1009 } else { 1002 }, "bad frame", None);
                return;
            }
            None => return,
        };
        let len = match &msg {
            Message::Text(t) => t.len(),
            Message::Binary(b) => b.len(),
            Message::Close(_) => return,
            _ => 0,
        };
        if len > 0 {
            p.tally.received(len);
            let wait = bucket.take(len);
            if !wait.is_zero() {
                tokio::select! {
                    _ = kill.changed() => return,
                    _ = tokio::time::sleep(wait) => {}
                }
            }
        }
        match msg {
            Message::Text(text) => {
                let reply = match ToRelay::parse(&text) {
                    Ok(m) => hub.control(p, m),
                    Err(_) => FromRelay::Err {
                        n: 0,
                        code: code::BAD.into(),
                    },
                };
                p.out.send(&reply);
            }
            Message::Binary(b) => match hub.binary(p, &b) {
                Route::Nothing => {}
                Route::Reply(r) => p.out.send(&r),
                Route::Forward {
                    to,
                    frame,
                    undeliverable,
                } => {
                    let permit = tokio::select! {
                        biased;
                        _ = kill.changed() => return,
                        r = tokio::time::timeout(t.stall, to.permits.clone().acquire_owned()) => r,
                    };
                    match permit {
                        Ok(Ok(permit)) => to.frame(frame, Some(permit)),
                        _ => {
                            tracing::info!(from = %short(&p.fp), "link stalled; closing its receiver");
                            to.kill(close::TOO_SLOW, "stalled", Some((code::IDLE, None)));
                            p.out.send(&undeliverable);
                        }
                    }
                }
            },
            _ => {}
        }
        deadline = Instant::now() + t.idle;
    }
}

async fn write_loop(
    mut sink: SplitSink<WebSocket, Message>,
    mut rx: mpsc::UnboundedReceiver<Out>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    ping: Duration,
    tally: Arc<OnceLock<Tally>>,
    out: Outbox,
) {
    let mut tick = tokio::time::interval_at(Instant::now() + ping, ping);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            m = rx.recv() => match m {
                None => break,
                Some(Out::Text(s)) => {
                    pending.fetch_sub(1, Ordering::Relaxed);
                    let n = s.len();
                    if sink.send(Message::Text(s.into())).await.is_err() {
                        break;
                    }
                    if let Some(t) = tally.get() {
                        t.sent(n);
                    }
                }
                Some(Out::Frame(b, permit)) => {
                    let n = b.len();
                    if sink.send(Message::Binary(b.into())).await.is_err() {
                        break;
                    }
                    drop(permit);
                    if let Some(t) = tally.get() {
                        t.sent(n);
                    }
                }
                Some(Out::Close(code, reason)) => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code,
                            reason: reason.into(),
                        })))
                        .await;
                    break;
                }
            },
            _ = tick.tick() => {
                if sink.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = sink.close().await;
    // The reader stops too.
    out.kill_quietly();
}
