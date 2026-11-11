//! Warm-up: bring every live session's sub-agent data up to date from disk
//! in the background, so moving through the list never waits on it.
//!
//! On start the daemon queues every live (not archived) session, most
//! recently active first; an unarchived session is queued then. A queued
//! session is "loading": the list shows it, and `session.subagents` answers
//! from the DB without touching disk until the worker has done it. Disk
//! reads happen without the DB lock; only reading and writing rows hold it.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::Value;

use rlord_core::agents::claude;
use rlord_core::db::{Db, SessionQuery};
use rlord_core::types::{Status, Subagent};

use crate::{methods, Daemon};

#[derive(Default)]
pub struct Warm {
    /// Sessions queued and not yet loaded, with how many loads are queued
    /// (one queued again mid-load stays loading until its last load ends).
    pending: Mutex<HashMap<String, usize>>,
    queue: Mutex<Option<mpsc::Sender<String>>>,
}

impl Warm {
    pub fn is_loading(&self, id: &str) -> bool {
        self.pending.lock().is_ok_and(|p| p.contains_key(id))
    }

    /// Every session still loading (one lock, for annotating a list).
    pub fn loading(&self) -> HashSet<String> {
        self.pending
            .lock()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Queue a session for loading (e.g. just unarchived).
    pub fn enqueue(&self, id: &str) {
        let Ok(q) = self.queue.lock() else { return };
        if let Some(tx) = q.as_ref() {
            if let Ok(mut p) = self.pending.lock() {
                *p.entry(id.to_string()).or_default() += 1;
            }
            let _ = tx.send(id.to_string());
        }
    }
}

/// Start the worker and queue every live session (most recent first).
pub fn start(d: &Arc<Daemon>) {
    let (tx, rx) = mpsc::channel::<String>();
    let ids: Vec<String> = d
        .db()
        .sessions(&SessionQuery::default())
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.id)
        .collect();
    if let Ok(mut p) = d.warm.pending.lock() {
        for id in &ids {
            *p.entry(id.clone()).or_default() += 1;
        }
    }
    for id in ids {
        let _ = tx.send(id);
    }
    if let Ok(mut q) = d.warm.queue.lock() {
        *q = Some(tx);
    }
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        let mut since_notify = std::time::Instant::now();
        while let Ok(id) = rx.recv() {
            // A panic on one session (odd transcript data) must not end the
            // worker and leave every other session "loading" for good.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| refresh(&d, &id))) {
                Ok(Err(e)) => tracing::debug!(session = %id, error = %e, "warming session"),
                Err(_) => tracing::warn!(session = %id, "warming session panicked"),
                Ok(Ok(_)) => {}
            }
            if let Ok(mut p) = d.warm.pending.lock() {
                if let Some(n) = p.get_mut(&id) {
                    *n -= 1;
                    if *n == 0 {
                        p.remove(&id);
                    }
                }
            }
            // Tell the list now and then (and when the queue drains), not
            // after every session: each tick makes every TUI refetch.
            let drained = d.warm.pending.lock().is_ok_and(|p| p.is_empty());
            if drained || since_notify.elapsed() > std::time::Duration::from_secs(2) {
                d.changed();
                since_notify = std::time::Instant::now();
            }
        }
    });
}

/// Bring one session's sub-agents up to date from disk. The transcript
/// reads run without the DB lock; returns the session's sub-agent rows.
pub fn refresh(d: &Daemon, r: &str) -> Result<Vec<Subagent>> {
    let (s, src) = {
        let db = d.db();
        let s = db.resolve_ref(r)?;
        let src = db.transcript_source(&s.id)?;
        (s, src)
    };
    let Some(src) = src else {
        return d.db().subagents(&s.id);
    };
    // Cheap per call: transcript stats are incremental (only appended bytes
    // are read). Every agent is checked, finished or not (a finished one may
    // still be resumed or get its last lines flushed).
    let running = s.status != Status::Stopped;
    let scanned = claude::scan_subagents_where(&s.id, &src, running, &|_| true);
    let db = d.db();
    // Compare with the rows as they are now, not as they were before the
    // unlocked scan: a hook may have finished an agent meanwhile.
    let known = db.subagents(&s.id)?;
    apply(&db, &known, &scanned)?;
    methods::save_cursors(&db);
    db.subagents(&s.id)
}

/// Write the scanned agents that differ from the known rows.
fn apply(db: &Db, known: &[Subagent], scanned: &[Subagent]) -> Result<()> {
    for a in scanned {
        let same = known.iter().any(|k| {
            k.agent_id == a.agent_id
                && k.input_tokens >= a.input_tokens
                && k.output_tokens >= a.output_tokens
                && k.started_at == a.started_at
                && (a.finished_at.is_none() || k.finished_at.is_some())
        });
        if !same {
            db.upsert_subagent(a)?;
        }
    }
    Ok(())
}

/// `session.subagents`: a loading session answers from the DB as is (the
/// worker brings it up to date); otherwise refresh it now (cheap once warm).
pub fn session_subagents(d: &Daemon, params: &Value) -> Result<Value> {
    let r = methods::p_str(params, "ref")?;
    let id = d.db().resolve_ref(r)?.id;
    let rows = if d.warm.is_loading(&id) {
        d.db().subagents(&id)?
    } else {
        refresh(d, &id)?
    };
    Ok(serde_json::to_value(rows)?)
}

/// Queue a session that just became live with history (imported,
/// unarchived) so its first view doesn't read it all on the spot.
pub fn enqueue_ref(d: &Daemon, r: &str) {
    if let Ok(s) = d.db().resolve_ref(r) {
        d.warm.enqueue(&s.id);
    }
}
