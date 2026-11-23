//! What the engine keeps on the device: one record per paired host, as
//! JSON sealed by the platform's [`crate::Vault`], in one file per host in
//! the engine's folder, written atomically (temp file, fsync, rename,
//! fsync of the folder). Nothing of the host's content is kept: the
//! snapshot and chat live in memory; the only user content at rest is the
//! outbox (the user's own unanswered requests).

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::outbox::Outbox;

pub const RECORD_VERSION: u32 = 1;
const PREFIX: &str = "host-";
const SUFFIX: &str = ".sealed";

pub fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()
}

/// The paired host.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HostRecord {
    /// The host's X25519 static key (base64url).
    pub static_key: String,
    /// The relay address with its pin (`origin#k=…`).
    pub pin: String,
    /// The host's relay key fingerprint (base64url): a device names it in
    /// `hello`.
    pub fp: String,
    pub name: String,
}

/// Everything the engine keeps for one host.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Record {
    pub v: u32,
    /// What the host calls this device.
    pub device_name: String,
    /// This device's X25519 static key for this host (base64url, private).
    pub static_key: String,
    /// The label of this device's relay identity key in [`crate::Signers`].
    pub label: String,
    pub host: HostRecord,
    /// The mailbox (`MailboxSession::export`, base64url), once keyed.
    #[serde(default)]
    pub mailbox: Option<String>,
    /// The relay boot and queue number of the last mailbox frame opened.
    #[serde(default)]
    pub mailbox_seen: Option<(String, u64)>,
    /// The relay boot seen last (`ready.boot`).
    #[serde(default)]
    pub boot: Option<String>,
    /// The host's state revision seen last.
    #[serde(default)]
    pub rev: Option<String>,
    #[serde(default)]
    pub outbox: Outbox,
    /// The sessions waiting for input when a notification was last made.
    #[serde(default)]
    pub notified: Vec<String>,
}

impl Record {
    /// The file this record lives in (from its label, which is per host).
    pub fn file(&self) -> String {
        file_for(&self.label)
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("serializable")
    }

    pub fn decode(b: &[u8]) -> Option<Record> {
        serde_json::from_slice::<Record>(b)
            .ok()
            .filter(|r| r.v == RECORD_VERSION)
    }
}

/// The relay key label for a host, from its static key: one key set per
/// host (a second host never sees the same device keys).
pub fn label_for(host_static: &[u8; 32]) -> String {
    let hex: String = host_static[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("relay-{hex}")
}

/// The file name for a label.
pub fn file_for(label: &str) -> String {
    let id = label.strip_prefix("relay-").unwrap_or(label);
    format!("{PREFIX}{id}{SUFFIX}")
}

/// Writes `bytes` to `dir/name` atomically: a temp file, fsync, rename,
/// fsync of the folder. A crash leaves the old file or the new one.
pub fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    std::fs::File::open(dir)?.sync_all()
}

/// The host files in `dir`, by name.
pub fn host_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name.starts_with(PREFIX) && name.ends_with(SUFFIX)).then(|| (name, e.path()))
        })
        .collect();
    v.sort();
    v
}

pub fn remove(dir: &Path, name: &str) -> std::io::Result<()> {
    match std::fs::remove_file(dir.join(name)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => std::fs::File::open(dir)?.sync_all(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_files_are_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let r = Record {
            v: RECORD_VERSION,
            device_name: "phone".into(),
            static_key: b64(&[1; 32]),
            label: label_for(&[0xab; 32]),
            host: HostRecord {
                static_key: b64(&[2; 32]),
                pin: "http://127.0.0.1:1#k=AA".into(),
                fp: b64(&[3; 32]),
                name: "mac".into(),
            },
            mailbox: None,
            mailbox_seen: Some(("b".into(), 3)),
            boot: None,
            rev: None,
            outbox: Outbox::default(),
            notified: vec![],
        };
        assert_eq!(r.label, "relay-abababababababab");
        assert_eq!(r.file(), "host-abababababababab.sealed");
        write_atomic(dir.path(), &r.file(), &r.encode()).unwrap();
        write_atomic(dir.path(), &r.file(), &r.encode()).unwrap();
        let files = host_files(dir.path());
        assert_eq!(files.len(), 1, "no temp file left: {files:?}");
        let back = Record::decode(&std::fs::read(&files[0].1).unwrap()).unwrap();
        assert_eq!(back, r);
        remove(dir.path(), &r.file()).unwrap();
        remove(dir.path(), &r.file()).unwrap();
        assert!(host_files(dir.path()).is_empty());
        assert!(Record::decode(b"{}").is_none());
    }
}
