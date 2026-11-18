//! Who may use the relay, and how much (docs/working/web-ui-remote.md,
//! "Production relay": identity and accounts).
//!
//! **Accounts govern access to the relay, never the keys.** An account is a
//! label on a host record; a device belongs to its host's account through
//! the host's allow-list and never logs in. The relay asks an
//! [`Authorizer`] one question when a host connects: admit (with an account
//! and its [`Limits`]), refuse (a close code) or pending (an `enroll`
//! address and code the user approves elsewhere; reserved for a hosted
//! relay's account service). [`StaticAuthorizer`] is what a self-hosted
//! relay uses: the `token`, `open` and `closed` modes, accounts as labels.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::Result;
use rlord_relay_proto::msg::{close, Enroll};
use serde::Deserialize;

use crate::registry::{Fp, Registry, Status, LOCAL};
use crate::store::now;

/// What one account may use. A hosted relay's tiers would be named sets of
/// these; a self-hosted relay gives every account the `[limits]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Devices on one host's allow-list.
    pub devices_per_host: usize,
    /// Bytes a connection may send per second, on average (a host's, times
    /// one plus the number of devices linked to it).
    pub bytes_per_sec: u64,
    /// ... and in one burst.
    pub burst_bytes: u64,
    /// One mailbox queue (per device and direction): messages, bytes, and
    /// how long a message waits. A zero `mailbox_ttl` turns queues off.
    pub mailbox_messages: usize,
    pub mailbox_bytes: usize,
    pub mailbox_ttl: Duration,
    /// Pairing rendezvous a host may have open at once, their longest
    /// life, and the pairing attempts one may take.
    pub rendezvous_per_host: usize,
    pub rendezvous_ttl: Duration,
    pub rendezvous_attempts: u32,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            devices_per_host: 16,
            bytes_per_sec: 256 * 1024,
            burst_bytes: 1024 * 1024,
            mailbox_messages: 32,
            mailbox_bytes: 128 * 1024,
            mailbox_ttl: Duration::from_secs(600),
            rendezvous_per_host: 4,
            rendezvous_ttl: Duration::from_secs(300),
            rendezvous_attempts: 3,
        }
    }
}

impl Limits {
    /// What `ready.limits` tells a client.
    pub fn wire(&self) -> rlord_relay_proto::msg::Limits {
        rlord_relay_proto::msg::Limits {
            devices: Some(self.devices_per_host as u32),
            bytes_per_sec: Some(self.bytes_per_sec),
            live_panes: None,
        }
    }
}

/// Who may register a host (`[hosts] registration`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Registration {
    /// A host's first connection must carry an unused, unexpired token from
    /// `rlord-relay token new`.
    #[default]
    Token,
    /// Any host key, up to `max_hosts`.
    Open,
    /// Only hosts added with `rlord-relay host add`.
    Closed,
}

/// A host asking to be let in. Its key is already proved.
pub struct HostRequest<'a> {
    pub fingerprint: &'a Fp,
    pub enroll: Option<&'a Enroll>,
    /// The client's address (from `X-Forwarded-For` in proxy mode).
    pub addr: IpAddr,
}

/// The answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Admit {
        account: String,
        limits: Limits,
    },
    /// Close with this code (`msg::close`).
    Refuse(u16),
    /// Send `enroll {url, code, expires}`, then ask again until it is
    /// decided or `expires` (Unix seconds) passes.
    Pending {
        url: String,
        code: String,
        expires: u64,
    },
}

/// The one place the relay decides who is in. Called on every host
/// connection (after the key is proved) and for every account's limits.
/// Synchronous: a hosted authorizer that calls out answers `Pending` and
/// decides in the background.
pub trait Authorizer: Send + Sync + 'static {
    fn host(&self, req: &HostRequest<'_>) -> Result<Decision>;
    fn limits(&self, account: &str) -> Limits;
}

/// The self-hosted authorizer: registration modes, accounts as labels,
/// one set of limits for every account.
pub struct StaticAuthorizer {
    pub registry: Registry,
    pub registration: Registration,
    pub limits: Limits,
    pub max_hosts: usize,
    /// A host not seen for this long is forgotten (`None`: never).
    pub host_expiry: Option<Duration>,
}

impl StaticAuthorizer {
    fn expired(&self, last_seen: u64) -> bool {
        self.host_expiry
            .is_some_and(|e| now().saturating_sub(last_seen) > e.as_secs())
    }

    fn admit(&self, account: String) -> Decision {
        Decision::Admit {
            limits: self.limits(&account),
            account,
        }
    }
}

impl Authorizer for StaticAuthorizer {
    fn host(&self, req: &HostRequest<'_>) -> Result<Decision> {
        let fp = req.fingerprint;
        if let Some((host, _)) = self.registry.host(fp)? {
            if self.expired(host.last_seen) {
                self.registry.remove_host(fp)?;
            } else if host.status == Status::Suspended {
                return Ok(Decision::Refuse(close::SUSPENDED));
            } else {
                self.registry.touch_host(fp)?;
                return Ok(self.admit(host.account));
            }
        }
        match self.registration {
            Registration::Closed => Ok(Decision::Refuse(close::NOT_ALLOWED)),
            Registration::Open => {
                if self.registry.host_count()? >= self.max_hosts {
                    return Ok(Decision::Refuse(close::LIMIT));
                }
                self.registry.add_host(fp, LOCAL)?;
                Ok(self.admit(LOCAL.to_string()))
            }
            Registration::Token => {
                let Some(Enroll::Token { token }) = req.enroll else {
                    return Ok(Decision::Refuse(close::NOT_ALLOWED));
                };
                if self.registry.host_count()? >= self.max_hosts {
                    return Ok(Decision::Refuse(close::LIMIT));
                }
                let Some(account) = self.registry.use_token(token)? else {
                    return Ok(Decision::Refuse(close::NOT_ALLOWED));
                };
                self.registry.add_host(fp, &account)?;
                Ok(self.admit(account))
            }
        }
    }

    fn limits(&self, _account: &str) -> Limits {
        self.limits.clone()
    }
}
