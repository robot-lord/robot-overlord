//! The records the relay keeps, as versioned JSON values in the [`Store`]:
//! hosts (account, status, first and last seen), each host's device
//! allow-list, and enrolment tokens (hashed). Nothing else is durable.

use std::sync::Arc;

use anyhow::{bail, Result};
use rlord_relay_proto::B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::{now, Store, Version, ABSENT};

pub type Fp = [u8; 32];

pub const NS_HOST: &str = "host";
pub const NS_DEVICE: &str = "device";
pub const NS_TOKEN: &str = "token";

/// The account of a relay without accounts.
pub const LOCAL: &str = "local";

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A fingerprint as the CLI takes it: base64url (as `rlord` shows it) or
/// 64 hex characters.
pub fn parse_fingerprint(s: &str) -> Result<Fp> {
    let s = s.trim();
    if let Some(fp) = unhex(s).and_then(|v| Fp::try_from(v).ok()) {
        return Ok(fp);
    }
    match rlord_relay_proto::address::fingerprint_from_b64(s) {
        Ok(fp) => Ok(fp),
        Err(_) => bail!("{s:?} is not a key fingerprint (43 base64url or 64 hex characters)"),
    }
}

/// A fingerprint as the CLI shows it.
pub fn show_fingerprint(fp: &Fp) -> String {
    B64(fp.to_vec()).to_string()
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Suspended,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub account: String,
    pub status: Status,
    /// Unix seconds.
    pub first_seen: u64,
    pub last_seen: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct Device {
    added: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct Token {
    account: String,
    uses: u32,
}

/// Typed access to the store.
#[derive(Clone)]
pub struct Registry {
    pub store: Arc<dyn Store>,
}

fn device_key(host: &Fp, device: &Fp) -> String {
    format!("{}/{}", hex(host), hex(device))
}

fn token_key(token: &str) -> String {
    hex(&Sha256::digest(token.trim().as_bytes()))
}

impl Registry {
    pub fn new(store: Arc<dyn Store>) -> Registry {
        Registry { store }
    }

    pub fn host(&self, fp: &Fp) -> Result<Option<(Host, Version)>> {
        Ok(match self.store.get(NS_HOST, &hex(fp))? {
            Some(r) => Some((serde_json::from_slice(&r.value)?, r.version)),
            None => None,
        })
    }

    /// Writes a host record; `false` when `expect` lost.
    pub fn put_host(&self, fp: &Fp, host: &Host, expect: Option<Version>) -> Result<bool> {
        Ok(self
            .store
            .put(NS_HOST, &hex(fp), &serde_json::to_vec(host)?, None, expect)?
            .is_ok())
    }

    /// Creates a host record; `false` when one exists.
    pub fn add_host(&self, fp: &Fp, account: &str) -> Result<bool> {
        let t = now();
        let host = Host {
            account: account.to_string(),
            status: Status::Active,
            first_seen: t,
            last_seen: t,
        };
        self.put_host(fp, &host, ABSENT)
    }

    /// Updates a host's last-seen time (best effort: a lost race means
    /// someone else just wrote it).
    pub fn touch_host(&self, fp: &Fp) -> Result<()> {
        if let Some((mut h, v)) = self.host(fp)? {
            h.last_seen = now();
            self.put_host(fp, &h, Some(v))?;
        }
        Ok(())
    }

    /// Removes a host and its allow-list; `false` when there was none.
    pub fn remove_host(&self, fp: &Fp) -> Result<bool> {
        let had = self.host(fp)?.is_some();
        for (key, _) in self.store.scan(NS_DEVICE, &format!("{}/", hex(fp)))? {
            let _ = self.store.delete(NS_DEVICE, &key, None)?;
        }
        let _ = self.store.delete(NS_HOST, &hex(fp), None)?;
        Ok(had)
    }

    /// Sets a host's status; `false` when there is no such host.
    pub fn set_status(&self, fp: &Fp, status: Status) -> Result<bool> {
        for _ in 0..8 {
            let Some((mut h, v)) = self.host(fp)? else {
                return Ok(false);
            };
            h.status = status;
            if self.put_host(fp, &h, Some(v))? {
                return Ok(true);
            }
        }
        bail!("the host record kept changing; try again")
    }

    pub fn hosts(&self) -> Result<Vec<(Fp, Host)>> {
        let mut out = Vec::new();
        for (key, r) in self.store.scan(NS_HOST, "")? {
            let Some(fp) = unhex(&key).and_then(|v| Fp::try_from(v).ok()) else {
                continue;
            };
            out.push((fp, serde_json::from_slice(&r.value)?));
        }
        Ok(out)
    }

    pub fn host_count(&self) -> Result<usize> {
        Ok(self.store.scan(NS_HOST, "")?.len())
    }

    pub fn is_device(&self, host: &Fp, device: &Fp) -> Result<bool> {
        Ok(self
            .store
            .get(NS_DEVICE, &device_key(host, device))?
            .is_some())
    }

    pub fn devices(&self, host: &Fp) -> Result<Vec<Fp>> {
        Ok(self
            .store
            .scan(NS_DEVICE, &format!("{}/", hex(host)))?
            .into_iter()
            .filter_map(|(key, _)| {
                let (_, d) = key.split_once('/')?;
                Fp::try_from(unhex(d)?).ok()
            })
            .collect())
    }

    /// Replaces a host's allow-list; the devices that were on it and are
    /// not any more.
    pub fn set_devices(&self, host: &Fp, allow: &[Fp]) -> Result<Vec<Fp>> {
        let old = self.devices(host)?;
        let removed: Vec<Fp> = old.iter().filter(|d| !allow.contains(d)).copied().collect();
        for d in &removed {
            let _ = self.store.delete(NS_DEVICE, &device_key(host, d), None)?;
        }
        let rec = serde_json::to_vec(&Device { added: now() })?;
        for d in allow.iter().filter(|d| !old.contains(d)) {
            // Absent only: an entry already there keeps its date.
            let _ = self
                .store
                .put(NS_DEVICE, &device_key(host, d), &rec, None, ABSENT)?;
        }
        Ok(removed)
    }

    /// Makes an enrolment token for `account`, good for `uses` hosts until
    /// `expires_at` (Unix seconds). Only its hash is stored.
    pub fn new_token(&self, account: &str, uses: u32, expires_at: u64) -> Result<String> {
        if uses == 0 {
            bail!("a token needs at least one use");
        }
        check_account(account)?;
        let mut raw = [0u8; 24];
        getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
        let token = format!("rlt_{}", B64(raw.to_vec()));
        let rec = serde_json::to_vec(&Token {
            account: account.to_string(),
            uses,
        })?;
        match self
            .store
            .put(NS_TOKEN, &token_key(&token), &rec, Some(expires_at), ABSENT)?
        {
            Ok(_) => Ok(token),
            Err(_) => bail!("token collision; try again"),
        }
    }

    /// Uses a token once: its account, or `None` when it is unknown, used
    /// up or expired. A lost race with another use is retried.
    pub fn use_token(&self, token: &str) -> Result<Option<String>> {
        let key = token_key(token);
        for _ in 0..8 {
            let Some(r) = self.store.get(NS_TOKEN, &key)? else {
                return Ok(None);
            };
            let t: Token = serde_json::from_slice(&r.value)?;
            if t.uses == 0 {
                return Ok(None);
            }
            let left = Token {
                uses: t.uses - 1,
                ..t.clone()
            };
            let won = if left.uses == 0 {
                self.store.delete(NS_TOKEN, &key, Some(r.version))?.is_ok()
            } else {
                self.store
                    .put(
                        NS_TOKEN,
                        &key,
                        &serde_json::to_vec(&left)?,
                        r.expires_at,
                        Some(r.version),
                    )?
                    .is_ok()
            };
            if won {
                return Ok(Some(t.account));
            }
        }
        Ok(None)
    }
}

/// Account names are labels: short, printable, no spaces.
pub fn check_account(account: &str) -> Result<()> {
    if account.is_empty()
        || account.len() > 64
        || !account
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
    {
        bail!("an account is 1 to 64 of a-z, A-Z, 0-9, - _ . @");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex(&[0, 0xab, 0x10]), "00ab10");
        assert_eq!(unhex("00ab10"), Some(vec![0, 0xab, 0x10]));
        assert_eq!(unhex("0"), None);
        assert_eq!(unhex("zz"), None);
        let fp = [7u8; 32];
        assert_eq!(parse_fingerprint(&hex(&fp)).unwrap(), fp);
        assert_eq!(parse_fingerprint(&show_fingerprint(&fp)).unwrap(), fp);
        assert!(parse_fingerprint("nope").is_err());
        assert!(check_account("local").is_ok());
        assert!(check_account("a b").is_err());
        assert!(check_account("").is_err());
    }
}
