//! Durable state, behind one trait so a hosted relay can swap the backend
//! (docs/working/web-ui-remote.md, "Production relay": "Modular
//! deployment"). What is durable is registration only: host records, each
//! host's device allow-list and enrolment tokens. Connections, links,
//! rendezvous, signal flags and mailbox queues live in memory and never
//! reach a store, so no ciphertext is ever written to disk.
//!
//! The trait is a little more than a key-value map, and nothing else:
//!
//! - **versions** and a **conditional write** (`expect`), for a token's
//!   last use, replacing an allow-list, or two relays writing one host;
//! - a **prefix scan**, for a host's devices or an account's hosts;
//! - **expiry** as an `expires_at` on each record, which readers check (a
//!   backend's own TTL would only be cleanup).
//!
//! Any backend must pass `crates/relay/tests/store.rs`.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};

/// A record's version. Versions start at 1 and grow by one per write.
pub type Version = u64;

/// `expect` for a write that must create the record.
pub const ABSENT: Option<Version> = Some(0);

/// A stored record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub value: Vec<u8>,
    pub version: Version,
    /// Unix seconds after which the record reads as absent.
    pub expires_at: Option<u64>,
}

/// A conditional write lost: the record was not at the expected version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict;

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the record changed underneath this write")
    }
}

impl std::error::Error for Conflict {}

/// Durable registration state. Namespaces in use: `host` (key: the host's
/// fingerprint in hex), `device` (`<host hex>/<device hex>`: the host's
/// allow-list), `token` (SHA-256 of the token, hex).
///
/// `expect` in a write: `None` writes whatever is there, [`ABSENT`] only
/// creates, `Some(v)` only replaces version `v`. An expired record counts as
/// absent everywhere.
pub trait Store: Send + Sync + 'static {
    fn get(&self, ns: &str, key: &str) -> Result<Option<Record>>;
    /// Writes `value`; the new version, or [`Conflict`].
    fn put(
        &self,
        ns: &str,
        key: &str,
        value: &[u8],
        expires_at: Option<u64>,
        expect: Option<Version>,
    ) -> Result<std::result::Result<Version, Conflict>>;
    /// Deletes the record; [`Conflict`] when `expect` doesn't match (a
    /// missing record matches `None` and [`ABSENT`]).
    fn delete(
        &self,
        ns: &str,
        key: &str,
        expect: Option<Version>,
    ) -> Result<std::result::Result<(), Conflict>>;
    /// Every live record whose key starts with `prefix`, in key order.
    fn scan(&self, ns: &str, prefix: &str) -> Result<Vec<(String, Record)>>;
    /// Cleanup: deletes expired records; how many went.
    fn purge_expired(&self) -> Result<usize>;
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn live(expires_at: Option<u64>, now: u64) -> bool {
    expires_at.is_none_or(|t| t > now)
}

/// The SQLite backend: `<data dir>/relay.db`, 0600.
pub struct SqliteStore {
    db: Mutex<rusqlite::Connection>,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<SqliteStore> {
        create_private(path)?;
        let db = rusqlite::Connection::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        // The CLI (`token new`, `host add`) writes while `serve` runs.
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv (
                ns TEXT NOT NULL,
                key TEXT NOT NULL,
                value BLOB NOT NULL,
                version INTEGER NOT NULL,
                expires_at INTEGER,
                PRIMARY KEY (ns, key)
            ) WITHOUT ROWID;",
        )?;
        Ok(SqliteStore { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Creates `path` as an empty 0600 file when it doesn't exist, so SQLite
/// (which gives its journal files the database's mode) never makes it
/// readable by others.
fn create_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
    }
}

type Row = (Vec<u8>, i64, Option<i64>);

fn read(tx: &rusqlite::Connection, ns: &str, key: &str, now: u64) -> Result<Option<Record>> {
    let row: Option<Row> = tx
        .query_row(
            "SELECT value, version, expires_at FROM kv WHERE ns = ?1 AND key = ?2",
            params![ns, key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(row.and_then(|(value, version, exp)| {
        let expires_at = exp.map(|t| t as u64);
        live(expires_at, now).then_some(Record {
            value,
            version: version as u64,
            expires_at,
        })
    }))
}

fn matches(current: Option<&Record>, expect: Option<Version>) -> bool {
    match (expect, current) {
        (None, _) => true,
        (Some(0), None) => true,
        (Some(v), Some(r)) => r.version == v,
        (Some(_), None) => false,
    }
}

impl Store for SqliteStore {
    fn get(&self, ns: &str, key: &str) -> Result<Option<Record>> {
        read(&self.db(), ns, key, now())
    }

    fn put(
        &self,
        ns: &str,
        key: &str,
        value: &[u8],
        expires_at: Option<u64>,
        expect: Option<Version>,
    ) -> Result<std::result::Result<Version, Conflict>> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = read(&tx, ns, key, now())?;
        if !matches(current.as_ref(), expect) {
            return Ok(Err(Conflict));
        }
        // An expired record is absent: its successor starts again at 1.
        let version = current.map_or(1, |r| r.version + 1);
        tx.execute(
            "INSERT INTO kv (ns, key, value, version, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (ns, key) DO UPDATE SET value = ?3, version = ?4, expires_at = ?5",
            params![ns, key, value, version as i64, expires_at.map(|t| t as i64)],
        )?;
        tx.commit()?;
        Ok(Ok(version))
    }

    fn delete(
        &self,
        ns: &str,
        key: &str,
        expect: Option<Version>,
    ) -> Result<std::result::Result<(), Conflict>> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = read(&tx, ns, key, now())?;
        if !matches(current.as_ref(), expect) {
            return Ok(Err(Conflict));
        }
        tx.execute(
            "DELETE FROM kv WHERE ns = ?1 AND key = ?2",
            params![ns, key],
        )?;
        tx.commit()?;
        Ok(Ok(()))
    }

    fn scan(&self, ns: &str, prefix: &str) -> Result<Vec<(String, Record)>> {
        let db = self.db();
        let now = now();
        let mut st = db.prepare(
            "SELECT key, value, version, expires_at FROM kv
             WHERE ns = ?1 AND substr(key, 1, length(?2)) = ?2 ORDER BY key",
        )?;
        let rows = st.query_map(params![ns, prefix], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, value, version, exp) = row?;
            let expires_at = exp.map(|t| t as u64);
            if live(expires_at, now) {
                out.push((
                    key,
                    Record {
                        value,
                        version: version as u64,
                        expires_at,
                    },
                ));
            }
        }
        Ok(out)
    }

    fn purge_expired(&self) -> Result<usize> {
        Ok(self.db().execute(
            "DELETE FROM kv WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            params![now() as i64],
        )?)
    }
}
