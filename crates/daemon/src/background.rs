//! "Waiting on background work": a session whose turn is over while work
//! its agent started still runs (`agents::background`: background `Bash`,
//! async subagents). Plain code: a tick reads what each live session's
//! transcript appended since the last look and keeps the count of open
//! work in memory. Not a stored status: `annotate` lays `background: N`
//! over a row whose stored status is `idle` (your turn), so hooks and
//! `discover` keep theirs, and "needs you" (a prompt) still wins.
//!
//! **Stale guard**: a transcript that hasn't changed for `STALE_MS` is
//! taken as "your turn" again, so a notification that never came (the
//! agent was restarted, a task died with it) can't leave a session stuck.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Value};

use rlord_core::agents::background::Tracker;
use rlord_core::db::{ArchivedMode, SessionQuery};
use rlord_core::types::now_ms;

use crate::Daemon;

const TICK: Duration = Duration::from_secs(5);
/// A transcript unchanged this long: its open work is no longer believed.
pub const STALE_MS: i64 = 30 * 60_000;

/// One live session's transcript as last read.
#[derive(Default)]
struct Seen {
    path: String,
    tracker: Tracker,
    /// The file's last change (ms).
    changed_at: i64,
}

#[derive(Default)]
pub struct Background {
    seen: Mutex<HashMap<String, Seen>>,
}

pub fn start(d: &Arc<Daemon>) {
    let d = Arc::clone(d);
    std::thread::spawn(move || loop {
        if let Err(e) = scan(&d) {
            tracing::debug!(error = %e, "looking for background work");
        }
        std::thread::sleep(TICK);
    });
}

/// Read what every live session's transcript appended since the last look.
fn scan(d: &Daemon) -> Result<()> {
    let sources: Vec<(String, String)> = {
        let db = d.db();
        let live: Vec<String> = db
            .sessions(&SessionQuery {
                archived: ArchivedMode::Live,
                ..Default::default()
            })?
            .into_iter()
            .map(|s| s.id)
            .collect();
        db.transcript_sources()?
            .into_iter()
            .filter(|(id, _)| live.contains(id))
            .collect()
    };
    let now = now_ms();
    // (Files are read without the DB lock; the map's lock is ours alone.)
    let mut seen = d.background.seen.lock().expect("background mutex");
    let before: HashMap<String, Option<usize>> = seen
        .iter()
        .map(|(id, s)| (id.clone(), count(s, now)))
        .collect();
    seen.retain(|id, _| sources.iter().any(|(i, _)| i == id));
    for (id, path) in &sources {
        let s = seen.entry(id.clone()).or_default();
        if s.path != *path {
            *s = Seen {
                path: path.clone(),
                ..Default::default()
            };
        }
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        s.changed_at = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as i64);
        if meta.len() != s.tracker.offset {
            let _ = s.tracker.advance(Path::new(path));
        }
    }
    let changed = seen.len() != before.len()
        || seen
            .iter()
            .any(|(id, s)| before.get(id) != Some(&count(s, now)));
    drop(seen);
    if changed {
        d.changed();
    }
    Ok(())
}

/// Open background work believed still running, if any.
fn count(s: &Seen, now: i64) -> Option<usize> {
    fresh(s.tracker.open.len(), s.changed_at, now)
}

/// `open` work in a transcript last changed at `changed_at`: shown only
/// while there is some and the file changed within `STALE_MS`.
pub fn fresh(open: usize, changed_at: i64, now: i64) -> Option<usize> {
    (open > 0 && now - changed_at < STALE_MS).then_some(open)
}

/// Annotate list rows with `background`: how many background tasks are
/// still running for a session whose turn is over (stored status `idle`),
/// else null.
pub fn annotate(d: &Daemon, rows: &mut [Value]) {
    let seen = d.background.seen.lock().expect("background mutex");
    let now = now_ms();
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        let n = seen
            .get(id)
            .filter(|_| row["status"] == "idle")
            .and_then(|s| count(s, now));
        row["background"] = n.map_or(Value::Null, |n| json!(n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shown_only_while_open_and_fresh() {
        let now = 10 * STALE_MS;
        assert_eq!(fresh(2, now - 1_000, now), Some(2));
        assert_eq!(fresh(0, now - 1_000, now), None);
        // Nothing written for the stale time: back to "your turn".
        assert_eq!(fresh(2, now - STALE_MS, now), None);
    }

    #[test]
    fn rows_are_annotated_only_on_their_turn() {
        let d = Daemon::new(
            rlord_core::db::Db::open_in_memory().unwrap(),
            Default::default(),
        );
        let mut t = Tracker::default();
        t.open.insert("a1".into());
        d.background.seen.lock().unwrap().insert(
            "s1".into(),
            Seen {
                path: "x".into(),
                tracker: t,
                changed_at: now_ms(),
            },
        );
        let mut rows = [
            json!({"id": "s1", "status": "idle"}),
            json!({"id": "s1", "status": "waiting_input"}),
            json!({"id": "s2", "status": "idle"}),
        ];
        annotate(&d, &mut rows);
        assert_eq!(rows[0]["background"], 1);
        assert!(rows[1]["background"].is_null());
        assert!(rows[2]["background"].is_null());
    }

    #[test]
    fn scan_reads_the_transcript_and_counts() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        let db = rlord_core::db::Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id: "s1",
            name: "web",
            agent: "claude",
            agent_session: None,
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
        db.upsert_transcript_source("s1", path.to_str().unwrap())
            .unwrap();
        let d = Daemon::new(db, rlord_core::config::Config::default());
        let mut f = std::fs::File::create(&path).unwrap();
        let launch = json!({"type": "user", "toolUseResult": {"isAsync": true, "status": "async_launched", "agentId": "a1"}});
        writeln!(f, "{launch}").unwrap();
        scan(&d).unwrap();
        let mut rows = [json!({"id": "s1", "status": "idle"})];
        annotate(&d, &mut rows);
        assert_eq!(rows[0]["background"], 1);
        let done = json!({"type": "user", "message": {"content":
            "<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n</task-notification>"}});
        writeln!(f, "{done}").unwrap();
        scan(&d).unwrap();
        annotate(&d, &mut rows);
        assert!(rows[0]["background"].is_null());
    }
}
