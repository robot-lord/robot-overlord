//! Warm-up: bring every live session's sub-agent data and usage (the
//! Usage column) up to date from disk in the background, so moving through
//! the list never waits on it.
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

use rlord_core::agents::{claude, Agent};
use rlord_core::db::{Db, SessionQuery};
use rlord_core::types::{Status, Subagent};

use crate::{methods, Daemon};

#[derive(Default)]
pub struct Warm {
    /// Sessions queued for a full load (sub-agents and usage) and not yet
    /// loaded, with how many loads are queued (one queued again mid-load
    /// stays loading until its last load ends). What "loading" (sub-agents)
    /// means; usage-only jobs aren't here.
    pending: Mutex<HashMap<String, usize>>,
    queue: Mutex<Option<mpsc::Sender<(String, Job)>>>,
    /// Sessions queued for their usage alone (`enqueue_usage`) and not yet
    /// read. Once read, one can be queued again (it fell far behind).
    usage_pending: Mutex<HashSet<String>>,
    /// Sessions whose usage job left nothing read (an unreadable
    /// transcript): not queued again this run, so a list can't keep the
    /// worker busy on them.
    usage_failed: Mutex<HashSet<String>>,
}

/// What a queued session needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Job {
    /// Sub-agents and usage.
    Full,
    /// Only its usage (a session the list shows whose transcript rlord
    /// hasn't read yet, e.g. an archived one, or is far behind on).
    Usage,
}

impl Warm {
    /// Its usage alone is queued (`enqueue_usage`) and not yet read.
    pub fn usage_queued(&self, id: &str) -> bool {
        self.usage_pending.lock().is_ok_and(|p| p.contains(id))
    }

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
        self.send(id, Job::Full);
    }

    /// Queue a session to read its usage, unless it's queued already.
    /// True: its usage is being read (the row shows it as loading); false:
    /// it won't be (its last read failed, or there's no worker).
    pub fn enqueue_usage(&self, id: &str) -> bool {
        if self.usage_failed.lock().is_ok_and(|f| f.contains(id)) {
            return false;
        }
        let Ok(mut p) = self.usage_pending.lock() else {
            return false;
        };
        if p.contains(id) {
            return true;
        }
        let sent = self.send(id, Job::Usage);
        if sent {
            p.insert(id.to_string());
        }
        sent
    }

    fn send(&self, id: &str, job: Job) -> bool {
        let Ok(q) = self.queue.lock() else {
            return false;
        };
        let Some(tx) = q.as_ref() else {
            return false;
        };
        if job == Job::Full {
            if let Ok(mut p) = self.pending.lock() {
                *p.entry(id.to_string()).or_default() += 1;
            }
        }
        tx.send((id.to_string(), job)).is_ok()
    }

    /// A job ended: the session is no longer pending for it.
    fn done(&self, id: &str, job: Job, read: bool) {
        match job {
            Job::Full => {
                if let Ok(mut p) = self.pending.lock() {
                    if let Some(n) = p.get_mut(id) {
                        *n -= 1;
                        if *n == 0 {
                            p.remove(id);
                        }
                    }
                }
            }
            Job::Usage => {
                if let Ok(mut p) = self.usage_pending.lock() {
                    p.remove(id);
                }
                if !read {
                    if let Ok(mut f) = self.usage_failed.lock() {
                        f.insert(id.to_string());
                    }
                }
            }
        }
    }

    fn idle(&self) -> bool {
        self.pending.lock().is_ok_and(|p| p.is_empty())
            && self.usage_pending.lock().is_ok_and(|p| p.is_empty())
    }
}

/// Start the worker and queue every live session (most recent first).
pub fn start(d: &Arc<Daemon>) {
    let (tx, rx) = mpsc::channel::<(String, Job)>();
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
        let _ = tx.send((id, Job::Full));
    }
    if let Ok(mut q) = d.warm.queue.lock() {
        *q = Some(tx);
    }
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        let mut since_notify = std::time::Instant::now();
        while let Ok((id, job)) = rx.recv() {
            // A panic on one session (odd transcript data) must not end the
            // worker and leave every other session "loading" for good.
            let run = || match job {
                Job::Full => refresh(&d, &id, false).map(|_| true),
                Job::Usage => read_usage(&d, &id),
            };
            let read = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
                Ok(Err(e)) => {
                    tracing::debug!(session = %id, error = %e, "warming session");
                    false
                }
                Err(_) => {
                    tracing::warn!(session = %id, "warming session panicked");
                    false
                }
                Ok(Ok(read)) => read,
            };
            d.warm.done(&id, job, read);
            // Tell the list now and then (and when the queue drains), not
            // after every session: each tick makes every TUI refetch.
            let drained = d.warm.idle();
            if drained || since_notify.elapsed() > std::time::Duration::from_secs(2) {
                d.changed();
                since_notify = std::time::Instant::now();
            }
        }
    });
}

/// Read one session's usage (its transcript, without the DB lock), so
/// the list has it at hand. False: nothing was read (no transcript, or it
/// can't be read).
fn read_usage(d: &Daemon, r: &str) -> Result<bool> {
    let (s, src) = {
        let db = d.db();
        let s = db.resolve_ref(r)?;
        let src = db.transcript_source(&s.id)?;
        (s, src)
    };
    let Some(src) = src else { return Ok(false) };
    let path = std::path::Path::new(&src);
    // From its saved position if it has one: never again from the start.
    crate::usage::recall(d, path);
    rlord_core::usage::of(path, s.model.as_deref());
    methods::save_cursors(&d.db());
    Ok(rlord_core::agents::transcript::unread_bytes(path).is_some())
}

/// Bring one session's sub-agents and usage up to date from disk. The
/// transcript reads run without the DB lock; returns the session's
/// sub-agent rows. `request`: on a request path (`session.subagents`), so
/// its main transcript is read only if little is new (else queued on the
/// warm-up); the worker reads it all.
pub fn refresh(d: &Daemon, r: &str, request: bool) -> Result<Vec<Subagent>> {
    let (s, src, known) = {
        let db = d.db();
        let s = db.resolve_ref(r)?;
        let src = db.transcript_source(&s.id)?;
        let known = db.subagents(&s.id)?;
        (s, src, known)
    };
    // Its usage (every agent): after this, the list reads only what's
    // appended.
    if let Some(src) = &src {
        let path = std::path::Path::new(src);
        if request {
            crate::usage::refresh_bounded(d, &s.id, path, s.model.as_deref());
        } else {
            crate::usage::recall(d, path);
            rlord_core::usage::of(path, s.model.as_deref());
        }
    }
    // Sub-agent transcripts read forward from their saved positions too.
    let mut saved = crate::usage::saved_positions(
        d,
        known
            .iter()
            .filter_map(|a| a.transcript_path.as_deref())
            .map(std::path::Path::new),
    );
    for a in &known {
        if let Some(t) = &a.transcript_path {
            crate::usage::restore(&mut saved, std::path::Path::new(t));
        }
    }
    // (Only Claude's sub-agents are tracked so far.)
    let tracked = Agent::parse(&s.agent).is_some_and(Agent::tracks_subagents);
    let Some(src) = src.filter(|_| tracked) else {
        let db = d.db();
        methods::save_cursors(&db);
        return db.subagents(&s.id);
    };
    // Cheap per call: transcript stats are incremental (only appended bytes
    // are read). Every agent is checked, finished or not (a finished one may
    // still be resumed or get its last lines flushed).
    let running = s.status != Status::Stopped;
    let scanned = claude::scan_subagents_read(&s.id, &src, running, &|_| true);
    let db = d.db();
    // Compare with the rows as they are now, not as they were before the
    // unlocked scan: a hook may have finished an agent meanwhile.
    let known = db.subagents(&s.id)?;
    apply(&db, &known, &scanned)?;
    methods::save_cursors(&db);
    db.subagents(&s.id)
}

/// Write the scanned agents that differ from the known rows (or whose
/// totals were counted by older logic: the scan recounts them).
/// `scanned`: each with whether its transcript was read (if not, its zeros
/// mustn't replace anything).
fn apply(db: &Db, known: &[Subagent], scanned: &[(Subagent, bool)]) -> Result<()> {
    let recount = match scanned.first() {
        Some((a, _)) => db.subagents_to_recount(&a.session_id)?,
        None => return Ok(()),
    };
    for (a, read) in scanned {
        let recounted = *read && recount.contains(&a.agent_id) && a.transcript_path.is_some();
        let same = known.iter().any(|k| {
            k.agent_id == a.agent_id
                && !recounted
                && k.input_tokens >= a.input_tokens
                && k.output_tokens >= a.output_tokens
                && k.started_at == a.started_at
                && (a.finished_at.is_none() || k.finished_at.is_some())
        });
        if same {
            continue;
        }
        if *read {
            db.upsert_subagent_counted(a)?;
        } else {
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
        refresh(d, &id, true)?
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unread_subagent_transcript_replaces_nothing() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id: "01a",
            name: "a",
            agent: "claude",
            agent_session: Some("01a"),
            project_path: "/tmp",
            tmux_session: None,
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        let a = Subagent {
            agent_id: "ag".into(),
            session_id: "01a".into(),
            agent_type: "Explore".into(),
            description: String::new(),
            tool_use_id: None,
            spawn_depth: 1,
            background: false,
            started_at: Some(1),
            finished_at: Some(2),
            transcript_path: Some("/gone/agent-ag.jsonl".into()),
            result: String::new(),
            input_tokens: 200,
            output_tokens: 40,
        };
        // Counted by older logic: due a recount.
        db.upsert_subagent(&a).unwrap();
        let known = db.subagents("01a").unwrap();
        let zeros = Subagent {
            input_tokens: 0,
            output_tokens: 0,
            ..a.clone()
        };
        // Its transcript couldn't be read: the zeros aren't a count.
        apply(&db, &known, &[(zeros.clone(), false)]).unwrap();
        let got = &db.subagents("01a").unwrap()[0];
        assert_eq!((got.input_tokens, got.output_tokens), (200, 40));
        assert_eq!(db.subagents_to_recount("01a").unwrap().len(), 1);
        // Read (and lower): the recount wins.
        let lower = Subagent {
            input_tokens: 100,
            output_tokens: 20,
            ..a
        };
        apply(&db, &known, &[(lower, true)]).unwrap();
        let got = &db.subagents("01a").unwrap()[0];
        assert_eq!((got.input_tokens, got.output_tokens), (100, 20));
    }
}
