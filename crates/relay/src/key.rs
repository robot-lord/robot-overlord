//! `relay.key`: the relay's P-256 identity, made on first start. Every host
//! and device pins its fingerprint; losing it means every host sets up
//! again.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{bail, Context, Result};
use rlord_relay_proto::Identity;

use crate::registry::{hex, unhex};

/// Creates `dir` (0700) when it doesn't exist.
pub fn make_data_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Loads the key at `path`, or makes one there (0600) when there is none.
pub fn load_or_create(path: &Path) -> Result<Identity> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let mode = std::fs::metadata(path)?.permissions().mode();
            if mode & 0o077 != 0 {
                bail!(
                    "{} is readable by others (mode {:o}); chmod 600 it",
                    path.display(),
                    mode & 0o777
                );
            }
            let bytes = unhex(text.trim())
                .with_context(|| format!("{} is not a relay key", path.display()))?;
            Identity::from_bytes(&bytes)
                .map_err(|_| anyhow::anyhow!("{} is not a relay key", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let id = generate()?;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("creating {}", path.display()))?;
            writeln!(f, "{}", hex(&id.to_bytes()))?;
            f.sync_all()?;
            Ok(id)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// A fresh identity.
pub fn generate() -> Result<Identity> {
    loop {
        if let Some(id) = Identity::from_seed(random()?) {
            return Ok(id);
        }
    }
}

pub fn random<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(b)
}
