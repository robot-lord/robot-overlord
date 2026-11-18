//! The one HTTP route, `GET /v1` (a websocket upgrade), on plain HTTP for a
//! proxy or on TLS the relay terminates itself.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::extract::connect_info::Connected;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::{IncomingStream, Listener};
use axum::Router;
use rlord_relay_proto::frame::MAX_BINARY;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tokio_rustls::server::TlsStream;

use crate::config::Tls;
use crate::conn::{self, ConnGuard};
use crate::hub::Hub;

/// The client's socket address.
#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub SocketAddr);

impl Connected<IncomingStream<'_, TcpListener>> for PeerAddr {
    fn connect_info(s: IncomingStream<'_, TcpListener>) -> PeerAddr {
        PeerAddr(*s.remote_addr())
    }
}

impl Connected<IncomingStream<'_, TlsListener>> for PeerAddr {
    fn connect_info(s: IncomingStream<'_, TlsListener>) -> PeerAddr {
        PeerAddr(*s.remote_addr())
    }
}

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route(rlord_relay_proto::address::PATH, get(upgrade))
        .with_state(hub)
}

/// Behind a proxy, the client is the last `X-Forwarded-For` entry (the one
/// the proxy itself added); otherwise the socket's peer.
fn client_ip(hub: &Hub, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if hub.cfg.tls == Tls::Proxy {
        // Only the last entry: earlier ones are whatever the client sent.
        let last = headers
            .get_all("x-forwarded-for")
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|s| s.trim().parse::<IpAddr>().ok());
        if let Some(ip) = last {
            return ip;
        }
    }
    peer.ip()
}

async fn upgrade(
    State(hub): State<Arc<Hub>>,
    ConnectInfo(PeerAddr(peer)): ConnectInfo<PeerAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // At the cap, refuse before any signature work.
    if hub.conns.fetch_add(1, Ordering::Relaxed) >= hub.cfg.max_connections {
        hub.conns.fetch_sub(1, Ordering::Relaxed);
        tracing::debug!("at max_connections; refusing an upgrade");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                header::RETRY_AFTER,
                hub.cfg.timeouts.retry_after.to_string(),
            )],
            "overloaded\n",
        )
            .into_response();
    }
    let guard = ConnGuard(hub.clone());
    let ip = client_ip(&hub, peer, &headers);
    // No permessage-deflate: axum never offers it, so the answer has no
    // Sec-WebSocket-Extensions.
    ws.max_message_size(MAX_BINARY)
        .max_frame_size(MAX_BINARY)
        .on_upgrade(move |socket| conn::run(hub, socket, ip, guard))
}

/// The certificate files, re-read on SIGHUP and every `tls_reload`.
pub struct TlsFiles {
    cert: PathBuf,
    key: PathBuf,
    current: RwLock<Arc<rustls::ServerConfig>>,
}

impl TlsFiles {
    pub fn load(cert: &Path, key: &Path) -> Result<TlsFiles> {
        Ok(TlsFiles {
            current: RwLock::new(server_config(cert, key)?),
            cert: cert.to_path_buf(),
            key: key.to_path_buf(),
        })
    }

    /// Re-reads the files; on an error the old ones stay in use.
    pub fn reload(&self) -> Result<()> {
        let c = server_config(&self.cert, &self.key)?;
        *self.current.write().unwrap() = c;
        tracing::info!("certificate reloaded");
        Ok(())
    }

    fn current(&self) -> Arc<rustls::ServerConfig> {
        self.current.read().unwrap().clone()
    }
}

/// A rustls server config (ring, TLS 1.3 and 1.2) from PEM files.
pub fn server_config(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("reading {}", cert.display()))?;
    if certs.is_empty() {
        bail!("{} has no certificate", cert.display());
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("reading the private key {}", key.display()))?;
    let mut c = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .context("the certificate and key don't make a TLS configuration")?;
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(c))
}

/// A listener that hands axum finished TLS connections. Handshakes run
/// in their own tasks (a slow client never holds up the next), each with
/// the auth timeout, at most `max_connections` at once.
pub struct TlsListener {
    rx: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl TlsListener {
    pub fn new(tcp: TcpListener, files: Arc<TlsFiles>, timeout: Duration, max: usize) -> Self {
        let local = tcp.local_addr().expect("a bound listener");
        let (tx, rx) = mpsc::channel(64);
        let slots = Arc::new(Semaphore::new(max.max(1)));
        let task = tokio::spawn(async move {
            loop {
                let (sock, addr) = match tcp.accept().await {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::debug!("accept: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                let acceptor = tokio_rustls::TlsAcceptor::from(files.current());
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    match tokio::time::timeout(timeout, acceptor.accept(sock)).await {
                        Ok(Ok(tls)) => {
                            let _ = tx.send((tls, addr)).await;
                        }
                        Ok(Err(e)) => tracing::debug!("TLS handshake: {e}"),
                        Err(_) => tracing::debug!("TLS handshake timed out"),
                    }
                });
            }
        });
        TlsListener { rx, local, task }
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(x) => x,
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.local)
    }
}
