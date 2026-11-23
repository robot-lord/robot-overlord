//! Software [`Signers`], [`Vault`] and [`Listener`] for tests and
//! `rlord-device`. A phone uses its keystore instead: these keep keys as
//! plain bytes (in memory, or in files of mode 0600).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use rlord_relay_proto::Identity;

use crate::{Listener, PlatformError, Signers, Vault};

/// P-256 keys in memory, or in a folder (one file per label) when made
/// with [`SoftSigners::in_dir`].
#[derive(Default)]
pub struct SoftSigners {
    keys: Mutex<HashMap<String, Identity>>,
    dir: Option<PathBuf>,
}

impl SoftSigners {
    pub fn new() -> SoftSigners {
        SoftSigners::default()
    }

    /// Keys kept as files `<label>.key` in `dir` (made if missing).
    pub fn in_dir(dir: impl Into<PathBuf>) -> SoftSigners {
        SoftSigners {
            keys: Mutex::new(HashMap::new()),
            dir: Some(dir.into()),
        }
    }

    fn path(&self, label: &str) -> Option<PathBuf> {
        let safe: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.as_ref().map(|d| d.join(format!("{safe}.key")))
    }

    fn load(&self, label: &str) -> Result<Identity, PlatformError> {
        if let Some(k) = self.keys.lock().unwrap().get(label) {
            return Ok(k.clone());
        }
        let p = self
            .path(label)
            .ok_or_else(|| PlatformError::new(format!("no key {label}")))?;
        let b = std::fs::read(&p).map_err(|_| PlatformError::new(format!("no key {label}")))?;
        let k = Identity::from_bytes(&b).map_err(|e| PlatformError::new(e.to_string()))?;
        self.keys
            .lock()
            .unwrap()
            .insert(label.to_string(), k.clone());
        Ok(k)
    }

    /// The key under `label` itself (tests: a second engine with the same
    /// identity).
    pub fn identity(&self, label: &str) -> Option<Identity> {
        self.load(label).ok()
    }

    /// Put `key` under `label`.
    pub fn insert(&self, label: &str, key: Identity) -> Result<(), PlatformError> {
        if let Some(p) = self.path(label) {
            write_private(&p, &key.to_bytes())?;
        }
        self.keys.lock().unwrap().insert(label.to_string(), key);
        Ok(())
    }
}

fn write_private(p: &std::path::Path, b: &[u8]) -> Result<(), PlatformError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let err = |e: std::io::Error| PlatformError::new(format!("{}: {e}", p.display()));
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(err)?;
    }
    let tmp = p.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(err)?;
    f.write_all(b).map_err(err)?;
    f.sync_all().map_err(err)?;
    std::fs::rename(&tmp, p).map_err(err)
}

/// A fresh software identity.
pub fn new_identity() -> Result<Identity, PlatformError> {
    loop {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| PlatformError::new(e.to_string()))?;
        if let Some(k) = Identity::from_seed(seed) {
            return Ok(k);
        }
    }
}

impl Signers for SoftSigners {
    fn create(&self, label: String) -> Result<Vec<u8>, PlatformError> {
        let k = new_identity()?;
        let public = k.public().to_vec();
        self.insert(&label, k)?;
        Ok(public)
    }

    fn public_key(&self, label: String) -> Result<Vec<u8>, PlatformError> {
        Ok(self.load(&label)?.public().to_vec())
    }

    fn sign(&self, label: String, msg: Vec<u8>) -> Result<Vec<u8>, PlatformError> {
        Ok(self.load(&label)?.sign(&msg).to_vec())
    }

    fn delete(&self, label: String) {
        self.keys.lock().unwrap().remove(&label);
        if let Some(p) = self.path(&label) {
            let _ = std::fs::remove_file(p);
        }
    }

    fn hardware(&self) -> bool {
        false
    }
}

/// No wrapping at all (tests and `rlord-device`): the files hold the
/// engine's JSON as is.
#[derive(Default)]
pub struct PlainVault;

impl Vault for PlainVault {
    fn seal(&self, plain: Vec<u8>) -> Result<Vec<u8>, PlatformError> {
        Ok(plain)
    }

    fn open(&self, sealed: Vec<u8>) -> Result<Vec<u8>, PlatformError> {
        Ok(sealed)
    }
}

/// What a [`Collect`] listener received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    Page(String),
    Notify(String, u32),
}

/// A listener that passes everything to a channel.
pub struct Collect(pub tokio::sync::mpsc::UnboundedSender<Out>);

impl Collect {
    pub fn new() -> (Collect, tokio::sync::mpsc::UnboundedReceiver<Out>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Collect(tx), rx)
    }
}

impl Listener for Collect {
    fn to_page(&self, json: String) {
        let _ = self.0.send(Out::Page(json));
    }

    fn notify(&self, kind: String, count: u32) {
        let _ = self.0.send(Out::Notify(kind, count));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_keys_sign_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let s = SoftSigners::in_dir(dir.path());
        let public = s.create("relay-a".into()).unwrap();
        assert_eq!(public.len(), 65);
        let sig = s.sign("relay-a".into(), b"m".to_vec()).unwrap();
        assert!(rlord_relay_proto::auth::verify(&public, b"m", &sig));
        // Another instance over the same folder finds it.
        let again = SoftSigners::in_dir(dir.path());
        assert_eq!(again.public_key("relay-a".into()).unwrap(), public);
        again.delete("relay-a".into());
        assert!(SoftSigners::in_dir(dir.path())
            .public_key("relay-a".into())
            .is_err());
        assert!(!s.hardware());
    }
}
