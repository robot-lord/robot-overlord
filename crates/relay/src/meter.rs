//! In-memory usage counters, per account and per identity (a key
//! fingerprint): bytes each way, messages each way, connections. Never
//! written per frame to the store; a hosted relay would flush them as
//! periodic usage records. A restart starts them again from zero.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::registry::Fp;

/// One set of counters.
#[derive(Default, Debug)]
pub struct Counters {
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    messages_in: AtomicU64,
    messages_out: AtomicU64,
    connections: AtomicU64,
    open: AtomicU64,
}

/// A copy of [`Counters`] at one moment.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Bytes received from the peer(s) (websocket payloads).
    pub bytes_in: u64,
    /// Bytes sent to the peer(s).
    pub bytes_out: u64,
    pub messages_in: u64,
    pub messages_out: u64,
    /// Connections admitted, ever.
    pub connections: u64,
    /// Connections open now.
    pub open: u64,
}

impl Counters {
    pub fn usage(&self) -> Usage {
        Usage {
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_out: self.bytes_out.load(Ordering::Relaxed),
            messages_in: self.messages_in.load(Ordering::Relaxed),
            messages_out: self.messages_out.load(Ordering::Relaxed),
            connections: self.connections.load(Ordering::Relaxed),
            open: self.open.load(Ordering::Relaxed),
        }
    }
}

/// The counters one connection adds to: its account's and its identity's.
#[derive(Clone, Debug)]
pub struct Tally {
    account: Arc<Counters>,
    identity: Arc<Counters>,
}

impl Tally {
    fn each(&self, f: impl Fn(&Counters)) {
        f(&self.account);
        f(&self.identity);
    }

    pub fn received(&self, bytes: usize) {
        self.each(|c| {
            c.bytes_in.fetch_add(bytes as u64, Ordering::Relaxed);
            c.messages_in.fetch_add(1, Ordering::Relaxed);
        });
    }

    pub fn sent(&self, bytes: usize) {
        self.each(|c| {
            c.bytes_out.fetch_add(bytes as u64, Ordering::Relaxed);
            c.messages_out.fetch_add(1, Ordering::Relaxed);
        });
    }

    pub fn closed(&self) {
        self.each(|c| {
            c.open.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

#[derive(Default)]
pub struct Meter {
    accounts: Mutex<HashMap<String, Arc<Counters>>>,
    identities: Mutex<HashMap<Fp, Arc<Counters>>>,
}

impl Meter {
    /// A connection admitted under `account` for `identity`.
    pub fn open(&self, account: &str, identity: &Fp) -> Tally {
        let account = self
            .accounts
            .lock()
            .unwrap()
            .entry(account.to_string())
            .or_default()
            .clone();
        let identity = self
            .identities
            .lock()
            .unwrap()
            .entry(*identity)
            .or_default()
            .clone();
        let t = Tally { account, identity };
        t.each(|c| {
            c.connections.fetch_add(1, Ordering::Relaxed);
            c.open.fetch_add(1, Ordering::Relaxed);
        });
        t
    }

    pub fn account(&self, account: &str) -> Usage {
        self.accounts
            .lock()
            .unwrap()
            .get(account)
            .map(|c| c.usage())
            .unwrap_or_default()
    }

    pub fn identity(&self, fp: &Fp) -> Usage {
        self.identities
            .lock()
            .unwrap()
            .get(fp)
            .map(|c| c.usage())
            .unwrap_or_default()
    }

    pub fn accounts(&self) -> Vec<(String, Usage)> {
        let mut v: Vec<_> = self
            .accounts
            .lock()
            .unwrap()
            .iter()
            .map(|(a, c)| (a.clone(), c.usage()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}
