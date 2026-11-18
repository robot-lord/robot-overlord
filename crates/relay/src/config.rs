//! `relay.toml`, and the same settings built in code (tests run the relay
//! in process with [`Config::new`]).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::policy::{Authorizer, Limits, Registration};

/// Where `serve` looks without `--config`.
pub const DEFAULT_PATH: &str = "/etc/rlord-relay/relay.toml";

/// How the relay gets TLS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tls {
    /// Plain HTTP for a reverse proxy in front that terminates TLS. The
    /// client's address comes from `X-Forwarded-For`.
    Proxy,
    /// The relay terminates TLS itself with these PEM files, re-read on
    /// SIGHUP and every `tls_reload`.
    Files { cert: PathBuf, key: PathBuf },
}

/// Timers. Tests shrink them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeouts {
    /// From the upgrade to `ready`.
    pub auth: Duration,
    /// A ping this often...
    pub ping: Duration,
    /// ... and closed after this long with nothing received.
    pub idle: Duration,
    /// A link whose receiver takes no frame for this long is closed.
    pub stall: Duration,
    /// Certificate files are re-read this often.
    pub tls_reload: Duration,
    /// How often a pending enrolment asks the authorizer again.
    pub enroll_poll: Duration,
    /// What `bye` and a 503 tell a client to wait, in seconds.
    pub retry_after: u64,
    /// Housekeeping: expired queues and rendezvous dropped, and hosts
    /// suspended by the CLI while connected closed.
    pub sweep: Duration,
}

impl Default for Timeouts {
    fn default() -> Timeouts {
        Timeouts {
            auth: Duration::from_secs(5),
            ping: Duration::from_secs(25),
            idle: Duration::from_secs(60),
            stall: Duration::from_secs(10),
            tls_reload: Duration::from_secs(12 * 3600),
            enroll_poll: Duration::from_secs(1),
            retry_after: 5,
            sweep: Duration::from_secs(30),
        }
    }
}

/// Everything `serve` needs.
#[derive(Clone)]
pub struct Config {
    /// What peers pin and must reach the relay by. `None` only for a
    /// loopback `listen`: then `http://127.0.0.1:<port>` (proxy) or
    /// `https://127.0.0.1:<port>` (files) from the bound port.
    pub origin: Option<String>,
    pub listen: SocketAddr,
    /// `relay.key` and `relay.db` live here.
    pub data_dir: PathBuf,
    pub tls: Tls,
    pub registration: Registration,
    /// Every account's limits (with the static authorizer).
    pub limits: Limits,
    pub max_hosts: usize,
    /// Open connections; at the cap an upgrade gets HTTP 503.
    pub max_connections: usize,
    /// Connections from one address that haven't reached `ready`.
    pub unauthenticated_per_address: usize,
    /// A host not seen for this long is forgotten (`None`: never).
    pub host_expiry: Option<Duration>,
    pub timeouts: Timeouts,
    /// Replaces the static authorizer (a hosted relay's, or a test's).
    pub authorizer: Option<Arc<dyn Authorizer>>,
    /// Re-read the certificate files on SIGHUP (the binary sets it; an
    /// in-process test uses `Running::reload_tls`).
    pub reload_on_sighup: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("origin", &self.origin)
            .field("listen", &self.listen)
            .field("data_dir", &self.data_dir)
            .field("tls", &self.tls)
            .field("registration", &self.registration)
            .field("limits", &self.limits)
            .field("max_hosts", &self.max_hosts)
            .field("max_connections", &self.max_connections)
            .field(
                "unauthenticated_per_address",
                &self.unauthenticated_per_address,
            )
            .field("host_expiry", &self.host_expiry)
            .field("timeouts", &self.timeouts)
            .field("authorizer", &self.authorizer.as_ref().map(|_| "custom"))
            .finish()
    }
}

impl Config {
    /// A loopback relay in proxy mode on a free port, token registration,
    /// default limits: what tests start from.
    pub fn new(data_dir: impl Into<PathBuf>) -> Config {
        Config {
            origin: None,
            listen: "127.0.0.1:0".parse().unwrap(),
            data_dir: data_dir.into(),
            tls: Tls::Proxy,
            registration: Registration::Token,
            limits: Limits::default(),
            max_hosts: 1000,
            max_connections: 10_000,
            unauthenticated_per_address: 8,
            host_expiry: Some(Duration::from_secs(90 * 86_400)),
            timeouts: Timeouts::default(),
            authorizer: None,
            reload_on_sighup: false,
        }
    }

    /// Reads `relay.toml`.
    pub fn load(path: &Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        let f: File = toml::from_str(text)?;
        let tls = match f.tls.mode {
            Mode::Proxy => Tls::Proxy,
            Mode::Files => Tls::Files {
                cert: f.tls.cert.context("[tls] mode = \"files\" needs cert")?,
                key: f.tls.key.context("[tls] mode = \"files\" needs key")?,
            },
        };
        let listen = match f.listen {
            Some(l) => l
                .parse()
                .with_context(|| format!("listen = {l:?} is not an ip:port"))?,
            None => match tls {
                Tls::Proxy => "127.0.0.1:8080".parse().unwrap(),
                Tls::Files { .. } => "0.0.0.0:443".parse().unwrap(),
            },
        };
        let l = f.limits;
        let d = Limits::default();
        let t = Timeouts::default();
        let secs = |v: Option<u64>, def: Duration| v.map_or(def, Duration::from_secs);
        let cfg = Config {
            origin: f.origin,
            listen,
            data_dir: f.data_dir.unwrap_or_else(|| "/var/lib/rlord-relay".into()),
            tls,
            registration: f.hosts.registration,
            limits: Limits {
                devices_per_host: l.devices_per_host.unwrap_or(d.devices_per_host),
                bytes_per_sec: l.bytes_per_second.unwrap_or(d.bytes_per_sec),
                burst_bytes: l.burst_bytes.unwrap_or(d.burst_bytes),
                mailbox_messages: l.mailbox_messages.unwrap_or(d.mailbox_messages),
                mailbox_bytes: l.mailbox_bytes.unwrap_or(d.mailbox_bytes),
                mailbox_ttl: secs(l.mailbox_ttl, d.mailbox_ttl),
                rendezvous_per_host: l.rendezvous_per_host.unwrap_or(d.rendezvous_per_host),
                rendezvous_ttl: secs(l.rendezvous_ttl, d.rendezvous_ttl),
                rendezvous_attempts: l.rendezvous_attempts.unwrap_or(d.rendezvous_attempts),
            },
            max_hosts: l.max_hosts.unwrap_or(1000),
            max_connections: l.max_connections.unwrap_or(10_000),
            unauthenticated_per_address: l.unauthenticated_per_address.unwrap_or(8),
            host_expiry: match l.host_expiry_days.unwrap_or(90) {
                0 => None,
                days => Some(Duration::from_secs(days * 86_400)),
            },
            timeouts: Timeouts {
                auth: secs(l.auth_timeout, t.auth),
                ping: secs(l.ping_interval, t.ping),
                idle: secs(l.idle_timeout, t.idle),
                stall: secs(l.stall_timeout, t.stall),
                ..t
            },
            authorizer: None,
            reload_on_sighup: false,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Settings that can't work.
    pub fn validate(&self) -> Result<()> {
        if let Some(o) = &self.origin {
            let n = rlord_relay_proto::address::normalize_origin(o)
                .map_err(|e| anyhow::anyhow!("origin = {o:?}: {e}"))?;
            if &n != o {
                bail!("origin = {o:?}: write it as {n:?} (peers compare it byte for byte)");
            }
        } else if !self.listen.ip().is_loopback() {
            bail!("origin is required (how peers reach this relay, e.g. \"https://relay.example.com\")");
        }
        let l = &self.limits;
        if l.bytes_per_sec == 0 || l.burst_bytes == 0 {
            bail!("[limits] bytes_per_second and burst_bytes must be above 0");
        }
        if self.timeouts.ping.is_zero() || self.timeouts.idle.is_zero() {
            bail!("[limits] ping_interval and idle_timeout must be above 0");
        }
        if self.timeouts.ping >= self.timeouts.idle {
            bail!("[limits] ping_interval must be shorter than idle_timeout");
        }
        Ok(())
    }

    pub fn key_path(&self) -> PathBuf {
        self.data_dir.join("relay.key")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("relay.db")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    origin: Option<String>,
    listen: Option<String>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    tls: TlsFile,
    #[serde(default)]
    hosts: HostsFile,
    #[serde(default)]
    limits: LimitsFile,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Files,
    Proxy,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TlsFile {
    #[serde(default)]
    mode: Mode,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct HostsFile {
    #[serde(default)]
    registration: Registration,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LimitsFile {
    max_hosts: Option<usize>,
    max_connections: Option<usize>,
    devices_per_host: Option<usize>,
    unauthenticated_per_address: Option<usize>,
    bytes_per_second: Option<u64>,
    burst_bytes: Option<u64>,
    mailbox_messages: Option<usize>,
    mailbox_bytes: Option<usize>,
    mailbox_ttl: Option<u64>,
    rendezvous_per_host: Option<usize>,
    rendezvous_ttl: Option<u64>,
    rendezvous_attempts: Option<u32>,
    auth_timeout: Option<u64>,
    ping_interval: Option<u64>,
    idle_timeout: Option<u64>,
    stall_timeout: Option<u64>,
    host_expiry_days: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_file() {
        let c = Config::parse(
            r#"
origin = "https://relay.example.com"
listen = "0.0.0.0:443"
data_dir = "/var/lib/rlord-relay"

[tls]
mode = "files"
cert = "/etc/rlord-relay/fullchain.pem"
key = "/etc/rlord-relay/privkey.pem"

[hosts]
registration = "closed"

[limits]
max_hosts = 5
mailbox_ttl = 0
host_expiry_days = 0
"#,
        )
        .unwrap();
        assert_eq!(c.origin.as_deref(), Some("https://relay.example.com"));
        assert!(matches!(c.tls, Tls::Files { .. }));
        assert_eq!(c.registration, Registration::Closed);
        assert_eq!(c.max_hosts, 5);
        assert!(c.limits.mailbox_ttl.is_zero());
        assert_eq!(c.host_expiry, None);
        assert_eq!(c.limits.devices_per_host, 16);
    }

    #[test]
    fn proxy_mode_binds_loopback_by_default() {
        let c = Config::parse(
            "origin = \"https://r.example\"\n[tls]\nmode = \"proxy\"\n[hosts]\nregistration = \"open\"\n",
        )
        .unwrap();
        assert_eq!(c.listen.to_string(), "127.0.0.1:8080");
        assert_eq!(c.tls, Tls::Proxy);
        assert_eq!(c.registration, Registration::Open);
    }

    #[test]
    fn refuses_what_cannot_work() {
        for bad in [
            // A typo is an error, not a silent default.
            "origin = \"https://r.example\"\n[limits]\nmax_host = 3\n",
            // Not normalised.
            "origin = \"https://R.example/\"\n[tls]\nmode = \"proxy\"\n",
            // No origin on a public address.
            "listen = \"0.0.0.0:443\"\n[tls]\nmode = \"proxy\"\n",
            // Files mode without files.
            "origin = \"https://r.example\"\n",
            "origin = \"http://r.example\"\n[tls]\nmode = \"proxy\"\n",
            "origin = \"https://r.example\"\n[tls]\nmode = \"proxy\"\n[limits]\nping_interval = 60\nidle_timeout = 60\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }
}
