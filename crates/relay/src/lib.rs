//! `rlord-relay`: the server hosts and devices dial out to
//! (docs/relay.md; the wire protocol is docs/working/e2e-protocol.md,
//! sections 1 and 2).
//!
//! It proves its identity (`relay.key`) to every peer, checks theirs,
//! splices a host's links to its devices, keeps short in-memory mailbox
//! queues and signal flags, and sees only ciphertext: no content crypto
//! lives here. Durable state is registration only, behind [`store::Store`].
//!
//! The library runs the real relay in process for tests: [`start`] with a
//! [`Config`], and [`Admin`] for what the CLI does (tokens, hosts).

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rlord_relay_proto::{Identity, RelayAddress, B64};
use tokio::sync::watch;

pub mod config;
mod conn;
mod hub;
pub mod key;
pub mod meter;
pub mod policy;
pub mod registry;
mod server;
pub mod store;

pub use config::{Config, Timeouts, Tls};
pub use meter::{Meter, Usage};
pub use policy::{Authorizer, Decision, HostRequest, Limits, Registration, StaticAuthorizer};
pub use registry::{Fp, Status};

use hub::Hub;
use registry::Registry;
use store::{SqliteStore, Store};

/// A relay running in this process.
pub struct Running {
    addr: SocketAddr,
    address: RelayAddress,
    hub: Arc<Hub>,
    tls: Option<Arc<server::TlsFiles>>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl Running {
    /// Where it listens (with the real port when `listen` had port 0).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The origin peers pin.
    pub fn origin(&self) -> &str {
        &self.address.origin
    }

    /// What `rlord-relay address` prints: the origin and the key pin.
    pub fn address(&self) -> &RelayAddress {
        &self.address
    }

    /// SHA-256 of the relay's public key.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.address.fingerprint
    }

    /// The websocket URL peers dial (`ws://127.0.0.1:<port>/v1` in tests).
    pub fn ws_url(&self) -> String {
        self.address.ws_url()
    }

    /// This process's `ready.boot`.
    pub fn boot(&self) -> &str {
        &self.hub.boot
    }

    /// Usage counters per account and per identity.
    pub fn meter(&self) -> &Meter {
        &self.hub.meter
    }

    /// Open websocket connections.
    pub fn connections(&self) -> usize {
        self.hub.conns.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The CLI's operations on this relay's store. A suspension or removal
    /// made through it also closes the host's live connections at once.
    pub fn admin(&self) -> Admin {
        Admin {
            registry: self.hub.registry.clone(),
            hub: Some(Arc::downgrade(&self.hub)),
        }
    }

    /// Re-reads the certificate files (what SIGHUP does).
    pub fn reload_tls(&self) -> Result<()> {
        match &self.tls {
            Some(t) => t.reload(),
            None => bail!("not in files mode"),
        }
    }

    /// Closes every connection (`bye` `overloaded`, then 4503: clients
    /// come back after `retry_after`) and stops listening.
    pub async fn shutdown(mut self) {
        self.stop().await;
    }

    async fn stop(&mut self) {
        self.shutdown.send_replace(true);
        if let Some(s) = self.server.take() {
            let abort = s.abort_handle();
            if tokio::time::timeout(Duration::from_secs(5), s)
                .await
                .is_err()
            {
                abort.abort();
            }
        }
        for t in self.tasks.drain(..) {
            t.abort();
        }
        let until = tokio::time::Instant::now() + Duration::from_secs(3);
        while self.connections() > 0 && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        if let Some(s) = self.server.take() {
            s.abort();
        }
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }
}

/// Opens the data directory: the key (made on first start) and the store.
fn open(cfg: &Config) -> Result<(Identity, Arc<dyn Store>)> {
    key::make_data_dir(&cfg.data_dir)?;
    let identity = key::load_or_create(&cfg.key_path())?;
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&cfg.db_path())?);
    Ok((identity, store))
}

/// The origin a relay with no `origin` setting gets from its bound port.
fn derived_origin(cfg: &Config, addr: SocketAddr) -> Result<String> {
    if let Some(o) = &cfg.origin {
        return Ok(o.clone());
    }
    if !addr.ip().is_loopback() {
        bail!("origin is required unless the relay listens on a loopback address");
    }
    let scheme = match cfg.tls {
        Tls::Proxy => "http",
        Tls::Files { .. } => "https",
    };
    let host = match addr.ip() {
        std::net::IpAddr::V4(ip) => ip.to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
    };
    rlord_relay_proto::address::normalize_origin(&format!("{scheme}://{host}:{}", addr.port()))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Starts the relay: binds `listen`, makes `relay.key` on first start and
/// serves until [`Running::shutdown`]. Starting again on the same data
/// directory keeps the key and registrations and gives a new `boot`.
pub async fn start(cfg: Config) -> Result<Running> {
    cfg.validate()?;
    let (identity, store) = open(&cfg)?;
    let registry = Registry::new(store);
    let auth: Arc<dyn Authorizer> = match &cfg.authorizer {
        Some(a) => a.clone(),
        None => Arc::new(StaticAuthorizer {
            registry: registry.clone(),
            registration: cfg.registration,
            limits: cfg.limits.clone(),
            max_hosts: cfg.max_hosts,
            host_expiry: cfg.host_expiry,
        }),
    };
    let tls = match &cfg.tls {
        Tls::Files { cert, key } => Some(Arc::new(server::TlsFiles::load(cert, key)?)),
        Tls::Proxy => None,
    };
    let tcp = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("listening on {}", cfg.listen))?;
    let addr = tcp.local_addr()?;
    let origin = derived_origin(&cfg, addr)?;
    let address = RelayAddress {
        origin: origin.clone(),
        fingerprint: identity.fingerprint(),
    };
    let boot = B64(key::random::<12>()?.to_vec()).to_string();
    let (shutdown, shutdown_rx) = watch::channel(false);
    let hub = Arc::new(Hub::new(
        cfg.clone(),
        identity,
        origin,
        boot,
        registry,
        auth,
        shutdown_rx.clone(),
    ));
    let app = server::router(hub.clone()).into_make_service_with_connect_info::<server::PeerAddr>();
    let mut stop = shutdown_rx.clone();
    let stopped = async move {
        let _ = stop.wait_for(|s| *s).await;
    };
    let server = match &tls {
        None => tokio::spawn(async move {
            if let Err(e) = axum::serve(tcp, app).with_graceful_shutdown(stopped).await {
                tracing::error!("server: {e}");
            }
        }),
        Some(files) => {
            let l = server::TlsListener::new(
                tcp,
                files.clone(),
                cfg.timeouts.auth,
                cfg.max_connections,
            );
            tokio::spawn(async move {
                if let Err(e) = axum::serve(l, app).with_graceful_shutdown(stopped).await {
                    tracing::error!("server: {e}");
                }
            })
        }
    };
    let mut tasks = Vec::new();
    // Housekeeping.
    let h = hub.clone();
    tasks.push(tokio::spawn(async move {
        let mut tick = tokio::time::interval(sweep_every(&h.cfg));
        loop {
            tick.tick().await;
            h.sweep();
        }
    }));
    if let Some(files) = &tls {
        let f = files.clone();
        let every = cfg.timeouts.tls_reload;
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
            loop {
                tick.tick().await;
                if let Err(e) = f.reload() {
                    tracing::warn!("certificate reload: {e:#}");
                }
            }
        }));
        if cfg.reload_on_sighup {
            let f = files.clone();
            let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
            tasks.push(tokio::spawn(async move {
                while hup.recv().await.is_some() {
                    if let Err(e) = f.reload() {
                        tracing::warn!("certificate reload: {e:#}");
                    }
                }
            }));
        }
    }
    tracing::info!(%addr, origin = %address.origin, "relay listening");
    Ok(Running {
        addr,
        address,
        hub,
        tls,
        shutdown,
        tasks,
        server: Some(server),
    })
}

/// Housekeeping runs often enough for the shortest timer it serves.
fn sweep_every(cfg: &Config) -> Duration {
    let t = cfg.limits.mailbox_ttl.min(Duration::from_secs(30));
    t.max(Duration::from_millis(100))
}

/// What `rlord-relay address` prints for a config.
pub fn address(cfg: &Config) -> Result<RelayAddress> {
    let Some(origin) = &cfg.origin else {
        bail!("no origin in the config");
    };
    let (identity, _) = open(cfg)?;
    Ok(RelayAddress {
        origin: origin.clone(),
        fingerprint: identity.fingerprint(),
    })
}

/// `rlord-relay check`: the config loads, the store opens, the key signs
/// and verifies, and in files mode the certificate loads. The lines to
/// print.
pub fn check(cfg: &Config) -> Result<Vec<String>> {
    cfg.validate()?;
    let (identity, store) = open(cfg)?;
    let msg = b"rlord-relay check";
    if !rlord_relay_proto::auth::verify(&identity.public(), msg, &identity.sign(msg)) {
        bail!("the relay key does not verify its own signature");
    }
    let hosts = Registry::new(store).host_count()?;
    let mut lines = vec![
        format!("key: {} ok", cfg.key_path().display()),
        format!("store: {} ok ({hosts} hosts)", cfg.db_path().display()),
    ];
    match &cfg.tls {
        Tls::Files { cert, key } => {
            server::server_config(cert, key)?;
            lines.push(format!("tls: {} ok", cert.display()));
        }
        Tls::Proxy => lines.push(format!(
            "tls: proxy mode (plain HTTP on {}; the proxy in front terminates TLS)",
            cfg.listen
        )),
    }
    match &cfg.origin {
        Some(o) => lines.push(format!(
            "origin: {o} (peers must reach the relay at exactly this origin)"
        )),
        None => lines.push("origin: from the bound port (loopback only)".into()),
    }
    if let Some(o) = &cfg.origin {
        lines.push(format!(
            "address: {}",
            RelayAddress {
                origin: o.clone(),
                fingerprint: identity.fingerprint(),
            }
        ));
    }
    Ok(lines)
}

/// One host as `host list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    pub fingerprint: Fp,
    pub account: String,
    pub status: Status,
    pub first_seen: u64,
    pub last_seen: u64,
    pub devices: usize,
}

/// What the CLI does to a relay's store: tokens and hosts. Safe while the
/// relay runs (SQLite serialises the writes).
pub struct Admin {
    registry: Registry,
    hub: Option<std::sync::Weak<Hub>>,
}

impl Admin {
    /// Opens `<data_dir>/relay.db` (made when missing).
    pub fn open(data_dir: &Path) -> Result<Admin> {
        key::make_data_dir(data_dir)?;
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&data_dir.join("relay.db"))?);
        Ok(Admin {
            registry: Registry::new(store),
            hub: None,
        })
    }

    /// `token new`: a token good for `uses` hosts for `ttl`, which
    /// registers each under `account`. Only its hash is stored.
    pub fn token_new(&self, uses: u32, ttl: Duration, account: &str) -> Result<String> {
        self.registry
            .new_token(account, uses, store::now() + ttl.as_secs())
    }

    /// `host add`: registers a host (for `registration = "closed"`);
    /// `false` when it was already registered.
    pub fn host_add(&self, fp: &Fp, account: &str) -> Result<bool> {
        registry::check_account(account)?;
        self.registry.add_host(fp, account)
    }

    /// `host remove`: forgets a host and its allow-list.
    pub fn host_remove(&self, fp: &Fp) -> Result<bool> {
        let had = self.registry.remove_host(fp)?;
        if let Some(h) = self.hub.as_ref().and_then(|h| h.upgrade()) {
            h.close_host(fp, rlord_relay_proto::msg::close::NOT_ALLOWED);
        }
        Ok(had)
    }

    /// `host suspend`: the host and its devices get 4402 until resumed.
    pub fn host_suspend(&self, fp: &Fp) -> Result<bool> {
        let found = self.registry.set_status(fp, Status::Suspended)?;
        if let Some(h) = self.hub.as_ref().and_then(|h| h.upgrade()) {
            h.close_host(fp, rlord_relay_proto::msg::close::SUSPENDED);
        }
        Ok(found)
    }

    pub fn host_resume(&self, fp: &Fp) -> Result<bool> {
        self.registry.set_status(fp, Status::Active)
    }

    pub fn hosts(&self) -> Result<Vec<HostInfo>> {
        let mut out = Vec::new();
        for (fp, h) in self.registry.hosts()? {
            out.push(HostInfo {
                devices: self.registry.devices(&fp)?.len(),
                fingerprint: fp,
                account: h.account,
                status: h.status,
                first_seen: h.first_seen,
                last_seen: h.last_seen,
            });
        }
        Ok(out)
    }
}
