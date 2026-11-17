//! "Waiting on background work": a session whose turn is over while an
//! async subagent (or a forked skill) its agent started still runs
//! (`agents::background`). Plain code: a tick reads what each live
//! session's transcript appended since the last look and keeps what is
//! shown in memory. Not a stored status: `annotate` lays
//! `background_tasks: N` over a row whose stored status is `idle` (your
//! turn), so hooks and `discover` keep theirs, and "needs you" (a prompt)
//! still wins. Background shells (`Bash` with `run_in_background`: often
//! a dev server or a watcher) never change the status: they are listed
//! as `background_shells`, for information.
//!
//! **Stale guard**: when neither the transcript nor any open subagent's
//! output has changed for `STALE_MS`, the work is no longer believed, so a
//! notification that never came (the agent was restarted, a task died
//! with it) can't leave a session stuck.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Value};

use rlord_core::agents::background::{Kind, Task, Tracker};
use rlord_core::db::{ArchivedMode, SessionQuery};
use rlord_core::types::now_ms;

use crate::Daemon;

const TICK: Duration = Duration::from_secs(5);
/// No write for this long (transcript or subagent output): the open work
/// is no longer believed.
pub const STALE_MS: i64 = 30 * 60_000;
/// Open subagents whose output files are looked at for freshness.
const STAT_CAP: usize = 5;
/// Tasks listed per row.
const LIST_CAP: usize = 5;

/// One live session's transcript as last read.
#[derive(Default, Clone)]
struct Seen {
    path: String,
    tracker: Tracker,
}

/// What a session shows, as of the last scan.
#[derive(Default, Clone, PartialEq, Debug)]
struct Shown {
    /// Open subagents (oldest first): the session is waiting on them.
    agents: Vec<Task>,
    agent_count: usize,
    /// Open background shells (oldest first): information only.
    shells: Vec<Task>,
    shell_count: usize,
}

#[derive(Default)]
pub struct Background {
    /// Read state, touched by the scan only (copied out to read).
    seen: Mutex<HashMap<String, Seen>>,
    /// What the rows show; `annotate` reads only this.
    shown: Mutex<HashMap<String, Shown>>,
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

fn mtime_ms(p: &Path) -> Option<i64> {
    let t = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64)
}

/// When the session last wrote anything: its transcript, or the output of
/// one of its open subagents (they write there, not in the transcript):
/// each one's `outputFile` and its sidechain transcript
/// (`<session>/subagents/agent-<id>.jsonl`). At most `STAT_CAP` of them.
fn last_write(path: &Path, t: &Tracker) -> i64 {
    let side = path.with_extension("").join("subagents");
    let mut newest = mtime_ms(path).unwrap_or(0);
    for (id, task) in t.of(Kind::Agent).take(STAT_CAP) {
        let mut files: Vec<PathBuf> = task.output.iter().cloned().collect();
        files.push(side.join(format!("agent-{id}.jsonl")));
        for f in files {
            newest = newest.max(mtime_ms(&f).unwrap_or(0));
        }
    }
    newest
}

/// What a tracker shows at `now`, given the session's last write.
fn shown(t: &Tracker, last_write: i64, now: i64) -> Shown {
    if now - last_write >= STALE_MS {
        return Shown::default();
    }
    let list = |kind| {
        let mut v: Vec<Task> = t.of(kind).map(|(_, t)| t.clone()).collect();
        v.sort_by_key(|t| t.started_at);
        let n = v.len();
        v.truncate(LIST_CAP);
        (v, n)
    };
    let (agents, agent_count) = list(Kind::Agent);
    let (shells, shell_count) = list(Kind::Shell);
    Shown {
        agents,
        agent_count,
        shells,
        shell_count,
    }
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
    // Copy the read state out: the reads happen unlocked (only this
    // thread writes it back).
    let mut seen: HashMap<String, Seen> = {
        let all = d.background.seen.lock().expect("background mutex");
        sources
            .iter()
            .map(|(id, _)| (id.clone(), all.get(id).cloned().unwrap_or_default()))
            .collect()
    };
    let now = now_ms();
    let mut after = HashMap::new();
    for (id, path) in &sources {
        let s = seen.get_mut(id).expect("copied above");
        if s.path != *path {
            *s = Seen {
                path: path.clone(),
                ..Default::default()
            };
        }
        let p = Path::new(path);
        let _ = s.tracker.advance(p);
        let sh = shown(&s.tracker, last_write(p, &s.tracker), now);
        if sh != Shown::default() {
            after.insert(id.clone(), sh);
        }
    }
    *d.background.seen.lock().expect("background mutex") = seen;
    // Compared with what was shown: going stale is a change too.
    let changed = {
        let mut shown = d.background.shown.lock().expect("background mutex");
        let changed = *shown != after;
        *shown = after;
        changed
    };
    if changed {
        d.changed();
    }
    Ok(())
}

fn task_list(v: &[Task]) -> Value {
    v.iter()
        .map(|t| json!({"label": t.label, "started_at": t.started_at}))
        .collect()
}

/// Annotate rows with background work:
/// - `background_tasks`: how many subagents still run for a session whose
///   turn is over (stored status `idle`), else null; `background_work`:
///   each one's `{label, started_at}` (oldest first, at most 5).
/// - `background_shells`: `{count, tasks}` of its background shells, any
///   status, else null. Information only.
pub fn annotate(d: &Daemon, rows: &mut [Value]) {
    let shown = d.background.shown.lock().expect("background mutex");
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        let s = shown.get(id);
        let agents = s.filter(|s| s.agent_count > 0 && row["status"] == "idle");
        row["background_tasks"] = agents.map_or(Value::Null, |s| json!(s.agent_count));
        row["background_work"] = agents.map_or(Value::Null, |s| task_list(&s.agents));
        row["background_shells"] = s.filter(|s| s.shell_count > 0).map_or(
            Value::Null,
            |s| json!({"count": s.shell_count, "tasks": task_list(&s.shells)}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tracker(lines: &[Value]) -> Tracker {
        let mut t = Tracker::default();
        for l in lines {
            t.feed(l);
        }
        t
    }

    fn agent(id: &str) -> Value {
        json!({"type": "user", "toolUseResult": {"isAsync": true, "status": "async_launched", "agentId": id}})
    }

    fn shell(id: &str) -> Value {
        json!({"type": "user", "toolUseResult": {"backgroundTaskId": id}})
    }

    #[test]
    fn stale_work_is_not_shown() {
        let t = tracker(&[agent("a1")]);
        let now = 10 * STALE_MS;
        assert_eq!(shown(&t, now - 1_000, now).agent_count, 1);
        // Nothing written for the stale time: back to "your turn".
        assert_eq!(shown(&t, now - STALE_MS, now), Shown::default());
    }

    #[test]
    fn a_subagents_output_keeps_it_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("s1.jsonl");
        std::fs::write(&main, "").unwrap();
        let out = dir.path().join("a1.output");
        std::fs::write(&out, "").unwrap();
        let mut launch = agent("a1");
        launch["toolUseResult"]["outputFile"] = json!(out.to_str().unwrap());
        let t = tracker(&[launch]);
        // Old transcript; fresh subagent output.
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&main)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(now_ms() - mtime_ms(&main).unwrap() >= STALE_MS);
        assert!(now_ms() - last_write(&main, &t) < STALE_MS);
    }

    fn daemon(path: &Path) -> Arc<Daemon> {
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
        Daemon::new(db, rlord_core::config::Config::default())
    }

    #[test]
    fn subagents_set_the_status_shells_only_inform() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        let d = daemon(&path);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", shell("b1")).unwrap();
        scan(&d).unwrap();
        let mut rows = [
            json!({"id": "s1", "status": "idle"}),
            json!({"id": "s1", "status": "waiting_input"}),
        ];
        annotate(&d, &mut rows);
        assert!(rows[0]["background_tasks"].is_null());
        assert_eq!(rows[0]["background_shells"]["count"], 1);
        writeln!(f, "{}", agent("a1")).unwrap();
        scan(&d).unwrap();
        annotate(&d, &mut rows);
        assert_eq!(rows[0]["background_tasks"], 1);
        // Needs you wins; the shells still show.
        assert!(rows[1]["background_tasks"].is_null());
        assert_eq!(rows[1]["background_shells"]["count"], 1);
        let done = json!({"type": "user", "origin": {"kind": "task-notification"}, "message": {"content":
            "<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n</task-notification>"}});
        writeln!(f, "{done}").unwrap();
        scan(&d).unwrap();
        annotate(&d, &mut rows);
        assert!(rows[0]["background_tasks"].is_null());
    }

    #[test]
    fn going_stale_pushes_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        let d = daemon(&path);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", agent("a1")).unwrap();
        let mut rx = d.notify.subscribe();
        scan(&d).unwrap();
        assert!(rx.try_recv().is_ok());
        while rx.try_recv().is_ok() {}
        // Nothing new: no push.
        scan(&d).unwrap();
        assert!(rx.try_recv().is_err());
        // The file goes quiet past the stale time: shown changes, a push.
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        f.set_modified(old).unwrap();
        scan(&d).unwrap();
        assert!(rx.try_recv().is_ok());
        let mut rows = [json!({"id": "s1", "status": "idle"})];
        annotate(&d, &mut rows);
        assert!(rows[0]["background_tasks"].is_null());
    }
}
