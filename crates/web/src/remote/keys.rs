//! The host's remote-access keys and files, in `<data dir>/remote/` (0700;
//! every file 0600 through `rlord_core::private`):
//!
//! - `host.key`: the X25519 static key (content identity), 32 bytes.
//! - `relay.key`: the P-256 relay identity, 32 bytes. Made for one relay
//!   and never reused for another.
//! - `pin.json`: the origin and fingerprint these keys were made for.
//! - `enroll.json`: an enrolment token, until the relay has accepted us.
//! - `devices.json` (`devices.rs`), `mailbox-<device id>.bin` and
//!   `requests-<device id>.json` (`mailbox.rs`).
//!
//! The pin is part of the keys: moving to another relay is `rlord relay
//! setup --reset`, which deletes this folder (every device pairs again).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use rlord_core::private;
use rlord_e2e::StaticKey;
use rlord_relay_proto::address::{b64_fingerprint, fingerprint_from_b64};
use rlord_relay_proto::{Identity, RelayAddress};

/// The keys of a host set up for one relay.
pub struct Keys {
    pub host: StaticKey,
    pub relay: Identity,
    pub pin: RelayAddress,
}

#[derive(Serialize, Deserialize)]
struct PinFile {
    origin: String,
    fingerprint: String,
}

#[derive(Serialize, Deserialize)]
struct EnrollFile {
    token: String,
}

pub fn random32() -> Result<[u8; 32]> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    Ok(b)
}

/// A new P-256 identity from the OS random source.
pub fn new_identity() -> Result<Identity> {
    loop {
        if let Some(id) = Identity::from_seed(random32()?) {
            return Ok(id);
        }
    }
}

fn read32(path: &Path) -> Result<[u8; 32]> {
    let b = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    b.try_into()
        .map_err(|_| anyhow::anyhow!("{} is not a 32-byte key", path.display()))
}

impl Keys {
    /// The keys in `dir`; None when this host was never set up.
    pub fn load(dir: &Path) -> Result<Option<Keys>> {
        let pin_path = dir.join("pin.json");
        if !pin_path.exists() {
            return Ok(None);
        }
        let pin: PinFile = serde_json::from_slice(&std::fs::read(&pin_path)?)
            .with_context(|| format!("reading {}", pin_path.display()))?;
        Ok(Some(Keys {
            host: StaticKey::from_private(read32(&dir.join("host.key"))?),
            relay: Identity::from_bytes(&read32(&dir.join("relay.key"))?)
                .map_err(|e| anyhow::anyhow!("relay.key: {e}"))?,
            pin: RelayAddress {
                origin: pin.origin,
                fingerprint: fingerprint_from_b64(&pin.fingerprint)
                    .map_err(|e| anyhow::anyhow!("pin.json: {e}"))?,
            },
        }))
    }

    /// New keys for `pin` in `dir` (which must hold none).
    pub fn create(dir: &Path, pin: &RelayAddress) -> Result<Keys> {
        if dir.join("pin.json").exists() {
            bail!("this host already has keys");
        }
        private::dir(dir)?;
        let host = StaticKey::generate().map_err(|e| anyhow::anyhow!("{e}"))?;
        let relay = new_identity()?;
        private::write(&dir.join("host.key"), host.private())?;
        private::write(&dir.join("relay.key"), &relay.to_bytes())?;
        // The pin last: its presence says the keys are complete.
        let pin_file = PinFile {
            origin: pin.origin.clone(),
            fingerprint: b64_fingerprint(&pin.fingerprint),
        };
        private::write(
            &dir.join("pin.json"),
            &serde_json::to_vec_pretty(&pin_file)?,
        )?;
        Ok(Keys {
            host,
            relay,
            pin: pin.clone(),
        })
    }

    /// The relay identity in a prologue: the pin (only ever used once the
    /// connection has proved exactly that origin and key).
    pub fn relay_id(&self) -> rlord_e2e::RelayId {
        rlord_e2e::RelayId::new(self.pin.origin.clone(), self.pin.fingerprint)
    }
}

/// Deletes the whole folder: keys, devices, mailboxes.
pub fn reset(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", dir.display()))
        }
        _ => Ok(()),
    }
}

fn enroll_path(dir: &Path) -> PathBuf {
    dir.join("enroll.json")
}

pub fn save_token(dir: &Path, token: &str) -> Result<()> {
    private::write(
        &enroll_path(dir),
        &serde_json::to_vec(&EnrollFile {
            token: token.to_string(),
        })?,
    )?;
    Ok(())
}

pub fn token(dir: &Path) -> Option<String> {
    let b = std::fs::read(enroll_path(dir)).ok()?;
    serde_json::from_slice::<EnrollFile>(&b)
        .ok()
        .map(|e| e.token)
}

/// The relay accepted us: the token is spent.
pub fn forget_token(dir: &Path) {
    let _ = std::fs::remove_file(enroll_path(dir));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("remote");
        assert!(Keys::load(&dir).unwrap().is_none());
        let pin = RelayAddress {
            origin: "https://r.example".into(),
            fingerprint: [3; 32],
        };
        let k = Keys::create(&dir, &pin).unwrap();
        assert!(Keys::create(&dir, &pin).is_err());
        let l = Keys::load(&dir).unwrap().unwrap();
        assert_eq!(l.host.public(), k.host.public());
        assert_eq!(l.relay.public(), k.relay.public());
        assert_eq!(l.pin, pin);
        for f in ["host.key", "relay.key", "pin.json"] {
            let m = std::fs::metadata(dir.join(f)).unwrap().permissions().mode();
            assert_eq!(m & 0o777, 0o600, "{f}");
        }
        let m = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(m & 0o777, 0o700);
        save_token(&dir, "t1").unwrap();
        assert_eq!(token(&dir).as_deref(), Some("t1"));
        forget_token(&dir);
        assert_eq!(token(&dir), None);
        reset(&dir).unwrap();
        assert!(!dir.exists());
        reset(&dir).unwrap();
    }
}
