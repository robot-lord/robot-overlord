//! The paired devices (`remote/devices.json`): who may reach this host.
//!
//! A device is stored only after the user confirmed it. Its record holds
//! its static key (content identity: what `KKpsk2` is run with) and its
//! relay key fingerprint (what the relay's allow-list holds). Revoking
//! deletes the record and its mailbox files.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use rlord_core::private;
use rlord_relay_proto::B64;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Device {
    /// `rlord_e2e::device_id` of its static key.
    pub id: String,
    pub name: String,
    /// X25519 static public key.
    #[serde(rename = "static")]
    pub static_key: B64,
    /// SHA-256 of its P-256 relay public key.
    pub relay_fp: B64,
    /// Unix seconds.
    pub paired_at: u64,
    /// Unix seconds of its last live session; None before the first.
    #[serde(default)]
    pub last_seen: Option<u64>,
}

impl Device {
    pub fn static_key(&self) -> Option<[u8; 32]> {
        self.static_key.as_array()
    }

    pub fn relay_fp(&self) -> Option<[u8; 32]> {
        self.relay_fp.as_array()
    }

    /// Unused for `days` days at `now` (0 = never expires).
    pub fn expired(&self, now: u64, days: u32) -> bool {
        days > 0
            && now.saturating_sub(self.last_seen.unwrap_or(self.paired_at))
                > u64::from(days) * 86_400
    }
}

#[derive(Default)]
pub struct Devices {
    path: PathBuf,
    pub list: Vec<Device>,
}

impl Devices {
    pub fn load(dir: &Path) -> Result<Devices> {
        let path = dir.join("devices.json");
        let list = match std::fs::read(&path) {
            Ok(b) => {
                serde_json::from_slice(&b).with_context(|| format!("reading {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Devices { path, list })
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            private::dir(dir)?;
        }
        private::write(&self.path, &serde_json::to_vec_pretty(&self.list)?)?;
        Ok(())
    }

    pub fn by_fp(&self, fp: &[u8; 32]) -> Option<&Device> {
        self.list.iter().find(|d| d.relay_fp.0 == fp)
    }

    pub fn by_id(&self, id: &str) -> Option<&Device> {
        self.list.iter().find(|d| d.id == id)
    }

    /// The relay allow-list: every device's relay key fingerprint.
    pub fn allow(&self) -> Vec<B64> {
        self.list.iter().map(|d| d.relay_fp.clone()).collect()
    }

    /// Adds or replaces (the same static key pairing again). If it can't
    /// be saved, the list stays as it was.
    pub fn add(&mut self, d: Device) -> Result<()> {
        let before = self.list.clone();
        self.list
            .retain(|x| x.id != d.id && x.relay_fp != d.relay_fp);
        self.list.push(d);
        let r = self.save();
        if r.is_err() {
            self.list = before;
        }
        r
    }

    /// Removes a device; its record, if it was there.
    pub fn remove(&mut self, id: &str) -> Result<Option<Device>> {
        let Some(i) = self.list.iter().position(|d| d.id == id) else {
            return Ok(None);
        };
        let d = self.list.remove(i);
        self.save()?;
        Ok(Some(d))
    }

    pub fn seen(&mut self, id: &str, now: u64) -> Result<()> {
        if let Some(d) = self.list.iter_mut().find(|d| d.id == id) {
            d.last_seen = Some(now);
            self.save()?;
        }
        Ok(())
    }
}

/// Clips a device's name: at most 40 characters, control characters
/// removed, trimmed. Empty becomes "device".
pub fn clean_name(name: &str) -> String {
    let s: String = name.chars().filter(|c| !c.is_control()).take(40).collect();
    let s = s.trim();
    if s.is_empty() {
        "device".into()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, fp: u8) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            static_key: B64(vec![1; 32]),
            relay_fp: B64(vec![fp; 32]),
            paired_at: 100,
            last_seen: None,
        }
    }

    #[test]
    fn devices_persist_and_replace() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("remote");
        let mut d = Devices::load(&dir).unwrap();
        assert!(d.list.is_empty());
        d.add(dev("a", 1)).unwrap();
        d.add(dev("b", 2)).unwrap();
        d.add(dev("a", 3)).unwrap();
        let d2 = Devices::load(&dir).unwrap();
        assert_eq!(d2.list.len(), 2);
        assert_eq!(d2.by_id("a").unwrap().relay_fp, B64(vec![3; 32]));
        assert!(d2.by_fp(&[1; 32]).is_none());
        assert_eq!(d2.allow().len(), 2);
        let mut d2 = d2;
        assert!(d2.remove("a").unwrap().is_some());
        assert!(d2.remove("a").unwrap().is_none());
        assert_eq!(Devices::load(&dir).unwrap().list.len(), 1);
    }

    #[test]
    fn a_device_that_cant_be_saved_isnt_added() {
        let tmp = tempfile::tempdir().unwrap();
        // The folder can't be made: a file is in the way.
        std::fs::write(tmp.path().join("remote"), b"").unwrap();
        let mut d = Devices {
            path: tmp.path().join("remote").join("devices.json"),
            list: Vec::new(),
        };
        assert!(d.add(dev("a", 1)).is_err());
        assert!(d.list.is_empty());
    }

    #[test]
    fn expiry_and_names() {
        let mut d = dev("a", 1);
        assert!(!d.expired(100 + 90 * 86_400, 90));
        assert!(d.expired(101 + 90 * 86_400, 90));
        assert!(!d.expired(u64::MAX, 0));
        d.last_seen = Some(200 + 90 * 86_400);
        assert!(!d.expired(150 + 90 * 86_400, 90));
        assert_eq!(clean_name("  my\u{7}phone\n "), "myphone");
        assert_eq!(clean_name(&"x".repeat(60)).len(), 40);
        assert_eq!(clean_name("\n"), "device");
    }
}
