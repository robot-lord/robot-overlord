//! "Waiting on background work": a session whose turn is over while an
//! async subagent (or a forked skill) its agent started still runs
//! (`agents::background`). Plain code: a tick reads what each live
//! session's transcript appended since the last look and keeps what is
//! shown in memory. Not a stored status: `annotate` lays
//! `async_tasks: N` over a row whose stored status is `idle` (your
//! turn), so hooks and `discover` keep theirs, and "needs you" (a prompt)
//! still wins. Background shells (`Bash` with `run_in_background`: often
//! a dev server or a watcher) never change the status: they are listed
//! as `async_shells`, for information.
//!
//! **Stale guard**: when neither the transcript nor any open subagent's
//! output has changed for `STALE_MS`, its subagents are hidden from the
//! status (a notification that never came can't leave a session stuck),
//! not forgotten: a later notification still closes them, and they show
//! again if their files change. Shells are hidden the same way once the
//! transcript itself has been quiet that long (they are only information). Work is forgotten only
//! when the session has been stopped past a short grace (a `/clear` stops
//! it for a moment, in the same process) or its agent process starts again:
//! rlord's resume, restart or refork, or the agent's own `SessionStart`
//! hook with source `startup`. Not `resume`, which an in-process `/resume`
//! sends too (a `claude -c` outside rlord is missed: owed). Its tasks died
//! with the old process.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

use rlord_core::agents::background::{Kind, Task, Tracker};
use rlord_core::agents::claude;
use rlord_core::db::LiveTranscript;
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
/// A session stopped (or failed) this long has its state dropped. Shorter:
/// kept, since a `/clear` stops a session for a moment (its `SessionEnd`)
/// in the same process, and its tasks go on.
const STOP_GRACE_MS: i64 = 15_000;
/// Bytes read per tick across all sessions; the rest waits for the next
/// tick (first looks at many large transcripts after a daemon start).
const TICK_BUDGET: u64 = 16 << 20;

/// One live session's transcript as last read.
#[derive(Default, Clone)]
struct Seen {
    tracker: Tracker,
    /// The file's (len, inode, mtime ms) once all of it was read:
    /// unchanged, it isn't opened.
    stamp: (u64, u64, i64),
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
    /// Which session the next tick reads first.
    turn: std::sync::atomic::AtomicUsize,
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
    meta_mtime_ms(&std::fs::metadata(p).ok()?)
}

fn meta_mtime_ms(m: &std::fs::Metadata) -> Option<i64> {
    m.modified().ok().map(rlord_core::timefmt::ms_epoch)
}

/// When the session last wrote anything: its transcript, or the output of
/// one of its open subagents (they write there, not in the transcript):
/// each one's `outputFile` and its sidechain transcript
/// (`agent-<id>.jsonl` in the `subagents` folder of the transcript it was
/// launched in, which a `/clear` leaves behind). At most `STAT_CAP` of
/// them, the most recently started first.
fn last_write(main: i64, t: &Tracker) -> i64 {
    let mut newest = main;
    let mut agents: Vec<_> = t.of(Kind::Agent).collect();
    agents.sort_by_key(|(_, t)| std::cmp::Reverse(t.started_at));
    for (id, task) in agents.into_iter().take(STAT_CAP) {
        let mut files: Vec<PathBuf> = task.output.iter().cloned().collect();
        if let Some(side) = &task.side {
            files.push(claude::subagent_transcript(side, id));
        }
        for f in files {
            newest = newest.max(mtime_ms(&f).unwrap_or(0));
        }
    }
    newest
}

/// Bring a tracker up to date: read what was appended and drop the work
/// launched before the agent was last started again (`started_at`: its
/// tasks died with the old process). Returns what it shows at `now`.
fn refresh(s: &mut Seen, path: &Path, started_at: i64, now: i64, budget: &mut u64) -> Shown {
    use std::os::unix::fs::MetadataExt;
    // One stat: unchanged since the last read, nothing is opened.
    let Ok(meta) = std::fs::metadata(path) else {
        // (The transcript can't be seen: its subagents' own files still
        // tell whether they run.)
        let agents = last_write(0, &s.tracker);
        return shown(&s.tracker, agents, 0, now);
    };
    let main = meta_mtime_ms(&meta).unwrap_or(0);
    let stamp = (meta.len(), meta.ino(), main);
    // (Saved only once the whole file is read, a skipped long line
    // included, and the read didn't fail: else read on next tick.)
    if stamp != s.stamp {
        if let Ok(n) = s.tracker.advance_within(path, &meta, *budget) {
            *budget = budget.saturating_sub(n);
            // (All read, or all but a line still being written: that
            // line's end changes the file, and is read then.)
            // (Nothing read of a file that grew: the budget ran out; not
            // saved.)
            if s.tracker.offset == meta.len() || (s.tracker.partial && n > 0) {
                s.stamp = stamp;
            }
        }
    }
    let t = &mut s.tracker;
    // A task with no launch time can't be placed after the start: dropped.
    if started_at > 0 {
        t.open.retain(|_, task| task.started_at >= started_at);
    }
    // (The subagents' own files only while some are open.)
    let agents = if t.of(Kind::Agent).next().is_some() {
        last_write(main, t)
    } else {
        main
    };
    shown(t, agents, main, now)
}

/// What a tracker shows at `now`: its subagents while the session wrote
/// anything (`agents_write`: its transcript or their output) within
/// `STALE_MS`, its shells while its transcript (`main_write`) did. Hidden
/// work stays open.
fn shown(t: &Tracker, agents_write: i64, main_write: i64, now: i64) -> Shown {
    let stale = now - agents_write >= STALE_MS;
    let list = |kind| {
        let mut v: Vec<Task> = t.of(kind).map(|(_, t)| t.clone()).collect();
        v.sort_by_key(|t| t.started_at);
        let n = v.len();
        v.truncate(LIST_CAP);
        (v, n)
    };
    let (agents, agent_count) = if stale {
        (Vec::new(), 0)
    } else {
        list(Kind::Agent)
    };
    let (shells, shell_count) = if now - main_write >= STALE_MS {
        (Vec::new(), 0)
    } else {
        list(Kind::Shell)
    };
    Shown {
        agents,
        agent_count,
        shells,
        shell_count,
    }
}

/// Read what every running session's transcript appended since the last
/// look (the overlords too: one can wait on its own subagent).
fn scan(d: &Daemon) -> Result<()> {
    // (Ids, paths, start times and statuses only, under the DB lock.)
    let now = now_ms();
    // Running, or stopped within `STOP_GRACE_MS` (a `/clear`).
    let sources: Vec<LiveTranscript> = d.db().live_transcripts(now - STOP_GRACE_MS)?;
    // Only this thread locks `seen` (`annotate` reads `shown`), so it is
    // held for the whole scan.
    let mut seen = d.background.seen.lock().expect("background mutex");
    let mut after = HashMap::new();
    let running: HashSet<&str> = sources.iter().map(|l| l.id.as_str()).collect();
    // Stopped past the grace, failed or archived: nothing of theirs runs.
    // (Resumed later, the work found again is older than the resume.)
    seen.retain(|id, _| running.contains(id.as_str()));
    let mut budget = TICK_BUDGET;
    // Start at a different session each tick: a late one isn't starved of
    // the budget.
    let start = d
        .background
        .turn
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let n = sources.len().max(1);
    for l in sources.iter().cycle().skip(start % n).take(sources.len()) {
        let s = seen.entry(l.id.clone()).or_default();
        s.tracker.switch_hint = l.switched_at;
        // (A new path, after a `/clear`: the tracker notices the new file
        // by its inode and carries the open work over.)
        let sh = refresh(s, Path::new(&l.path), l.started_at, now, &mut budget);
        if sh != Shown::default() {
            after.insert(l.id.clone(), sh);
        }
    }
    drop(seen);
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
/// - `async_tasks`: how many subagents still run for a session whose
///   turn is over (stored status `idle`), else null; `async_work`:
///   each one's `{label, started_at}` (oldest first, at most 5).
/// - `async_shells`: `{count, tasks}` of its background shells, any
///   status, else null. Information only.
pub fn annotate(d: &Daemon, rows: &mut [Value]) {
    let shown = d.background.shown.lock().expect("background mutex");
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        let s = shown.get(id);
        let agents = s.filter(|s| s.agent_count > 0 && row["status"] == "idle");
        row["async_tasks"] = agents.map_or(Value::Null, |s| json!(s.agent_count));
        row["async_work"] = agents.map_or(Value::Null, |s| task_list(&s.agents));
        row["async_shells"] = s.filter(|s| s.shell_count > 0).map_or(
            Value::Null,
            |s| json!({"count": s.shell_count, "tasks": task_list(&s.shells)}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A budget with no limit, for one call.
    fn all() -> u64 {
        u64::MAX
    }
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
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("s1.jsonl");
        std::fs::write(&main, format!("{}\n{}\n", agent("a1"), shell("b1"))).unwrap();
        let mut t = Seen::default();
        let now = now_ms();
        assert_eq!(refresh(&mut t, &main, 0, now, &mut all()).agent_count, 1);
        // Nothing written for the stale time: the agent and the shell are
        // hidden, back to "your turn".
        let later = refresh(&mut t, &main, 0, now + STALE_MS, &mut all());
        assert_eq!(later, Shown::default());
        // Not forgotten: shown again once it writes (here: `now` again).
        assert_eq!(t.tracker.open.len(), 2);
        assert_eq!(refresh(&mut t, &main, 0, now, &mut all()).agent_count, 1);
        // A late notification still closes it.
        let done = json!({"type": "attachment", "attachment": {"type": "queued_command",
            "commandMode": "task-notification",
            "prompt": "<task-notification><task-id>a1</task-id><status>completed</status></task-notification>"}});
        let mut f = std::fs::File::options().append(true).open(&main).unwrap();
        writeln!(f, "{done}").unwrap();
        refresh(&mut t, &main, 0, now + STALE_MS, &mut all());
        assert_eq!(t.tracker.open.keys().collect::<Vec<_>>(), ["b1"]);
    }

    #[test]
    fn a_restart_drops_the_work_from_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("s1.jsonl");
        let mut old = agent("a1");
        old["timestamp"] = json!("2026-01-01T00:00:00.000Z");
        let mut new = agent("a2");
        new["timestamp"] = json!("2026-01-01T00:10:00.000Z");
        std::fs::write(&main, format!("{old}\n{new}\n")).unwrap();
        let started =
            rlord_core::agents::transcript::parse_iso_ms("2026-01-01T00:05:00.000Z").unwrap();
        let mut t = Seen::default();
        // (`now` right after: fresh enough by the file's mtime.)
        let sh = refresh(&mut t, &main, started, now_ms(), &mut all());
        assert_eq!(sh.agent_count, 1);
        assert_eq!(t.tracker.open.keys().collect::<Vec<_>>(), ["a2"]);
    }

    #[test]
    fn the_stamp_waits_for_the_whole_file() {
        use rlord_core::agents::background::CHUNK;
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("s1.jsonl");
        let mut f = std::fs::File::create(&main).unwrap();
        writeln!(f, "{}", agent("a1")).unwrap();
        let long = json!({"type": "user", "pad": "x".repeat(CHUNK as usize + 10)});
        let done = json!({"type": "attachment", "attachment": {"type": "queued_command",
            "commandMode": "task-notification",
            "prompt": "<task-notification><task-id>a1</task-id><status>completed</status></task-notification>"}});
        writeln!(f, "{long}\n{done}").unwrap();
        drop(f);
        let mut s = Seen::default();
        let now = now_ms();
        // More than one tick's read: the file is unchanged in between, yet
        // each tick goes on reading until the end.
        for _ in 0..5 {
            refresh(&mut s, &main, 0, now, &mut all());
        }
        assert!(s.tracker.open.is_empty());
        assert_eq!(s.stamp.0, std::fs::metadata(&main).unwrap().len());
    }

    #[test]
    fn the_tick_budget_is_shared_and_the_rest_waits() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("s1.jsonl");
        let mut f = std::fs::File::create(&main).unwrap();
        let pad = json!({"type": "assistant", "pad": "x".repeat(1000)});
        for _ in 0..10 {
            writeln!(f, "{pad}").unwrap();
        }
        writeln!(f, "{}", agent("a1")).unwrap();
        drop(f);
        let mut s = Seen::default();
        let mut budget = 3_000;
        refresh(&mut s, &main, 0, now_ms(), &mut budget);
        // Spent, not all read, the stamp not saved.
        assert!(budget < 1_100);
        assert!(s.tracker.open.is_empty());
        assert_eq!(s.stamp, (0, 0, 0));
        let mut none = 0;
        refresh(&mut s, &main, 0, now_ms(), &mut none);
        assert!(s.tracker.open.is_empty());
        // Later ticks carry on.
        for _ in 0..10 {
            refresh(&mut s, &main, 0, now_ms(), &mut 3_000);
        }
        assert_eq!(s.tracker.open.len(), 1);
    }

    #[test]
    fn a_partial_last_line_saves_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("s1.jsonl");
        std::fs::write(&main, format!("{}\n{{\"type\":\"user\"", agent("a1"))).unwrap();
        let mut s = Seen::default();
        refresh(&mut s, &main, 0, now_ms(), &mut all());
        assert_eq!(s.stamp.0, std::fs::metadata(&main).unwrap().len());
        assert_eq!(s.tracker.open.len(), 1);
    }

    #[test]
    fn a_fork_drops_the_launches_it_inherited() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("fork.jsonl");
        let mut copied = agent("a1");
        copied["timestamp"] = json!("2026-01-01T00:00:00.000Z");
        std::fs::write(&main, format!("{copied}\n")).unwrap();
        // Its `forked` event, after the copied launch.
        let forked =
            rlord_core::agents::transcript::parse_iso_ms("2026-01-02T00:00:00.000Z").unwrap();
        let mut s = Seen::default();
        assert_eq!(
            refresh(&mut s, &main, forked, now_ms(), &mut all()),
            Shown::default()
        );
    }

    #[test]
    fn a_task_with_no_time_goes_on_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("s1.jsonl");
        std::fs::write(&main, format!("{}\n", agent("a1"))).unwrap();
        let mut s = Seen::default();
        assert_eq!(
            refresh(&mut s, &main, 0, now_ms(), &mut all()).agent_count,
            1
        );
        assert_eq!(
            refresh(&mut s, &main, 1, now_ms(), &mut all()),
            Shown::default()
        );
    }

    #[test]
    fn a_carried_agent_keeps_its_sidechain_folder() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("s1.jsonl");
        let new = dir.path().join("s2.jsonl");
        std::fs::write(&old, format!("{}\n", agent("a1"))).unwrap();
        std::fs::write(&new, "").unwrap();
        let side = dir.path().join("s1").join("subagents");
        std::fs::create_dir_all(&side).unwrap();
        std::fs::write(side.join("agent-a1.jsonl"), "").unwrap();
        let mut s = Seen::default();
        refresh(&mut s, &old, 0, now_ms(), &mut all());
        refresh(&mut s, &new, 0, now_ms(), &mut all());
        // Both transcripts quiet; the subagent's own file is what's fresh.
        assert!(now_ms() - last_write(0, &s.tracker) < STALE_MS);
    }

    #[test]
    fn a_clear_carries_the_work_to_the_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("s1.jsonl");
        let new = dir.path().join("s2.jsonl");
        std::fs::write(&old, format!("{}\n", agent("a1"))).unwrap();
        std::fs::write(&new, "").unwrap();
        let mut t = Seen::default();
        refresh(&mut t, &old, 0, now_ms(), &mut all());
        // Same process (no new start): the open work goes on.
        assert_eq!(
            refresh(&mut t, &new, 0, now_ms(), &mut all()).agent_count,
            1
        );
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
        let main_at = mtime_ms(&main).unwrap();
        assert!(now_ms() - last_write(main_at, &t) < STALE_MS);
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
        // (Running: a stopped session isn't scanned.)
        db.set_status("s1", rlord_core::types::Status::Idle, 1)
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
        assert!(rows[0]["async_tasks"].is_null());
        assert_eq!(rows[0]["async_shells"]["count"], 1);
        writeln!(f, "{}", agent("a1")).unwrap();
        scan(&d).unwrap();
        annotate(&d, &mut rows);
        assert_eq!(rows[0]["async_tasks"], 1);
        // Needs you wins; the shells still show.
        assert!(rows[1]["async_tasks"].is_null());
        assert_eq!(rows[1]["async_shells"]["count"], 1);
        let done = json!({"type": "user", "origin": {"kind": "task-notification"}, "message": {"content":
            "<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n</task-notification>"}});
        writeln!(f, "{done}").unwrap();
        scan(&d).unwrap();
        annotate(&d, &mut rows);
        assert!(rows[0]["async_tasks"].is_null());
    }

    #[test]
    fn a_clear_stop_keeps_the_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        let d = daemon(&path);
        std::fs::write(&path, format!("{}\n", agent("a1"))).unwrap();
        scan(&d).unwrap();
        // `/clear`: SessionEnd stops it for a moment, SessionStart(clear)
        // brings it back. Same process: the agent is still open.
        d.db()
            .set_status("s1", rlord_core::types::Status::Stopped, now_ms())
            .unwrap();
        scan(&d).unwrap();
        d.db()
            .set_status("s1", rlord_core::types::Status::Idle, now_ms())
            .unwrap();
        scan(&d).unwrap();
        let mut rows = [json!({"id": "s1", "status": "idle"})];
        annotate(&d, &mut rows);
        assert_eq!(rows[0]["async_tasks"], 1);
    }

    #[test]
    fn a_stopped_session_shows_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1.jsonl");
        let d = daemon(&path);
        std::fs::write(&path, format!("{}\n{}\n", agent("a1"), shell("b1"))).unwrap();
        scan(&d).unwrap();
        let mut rows = [json!({"id": "s1", "status": "idle"})];
        annotate(&d, &mut rows);
        assert_eq!(rows[0]["async_tasks"], 1);
        d.db()
            .set_status(
                "s1",
                rlord_core::types::Status::Stopped,
                now_ms() - STOP_GRACE_MS,
            )
            .unwrap();
        scan(&d).unwrap();
        annotate(&d, &mut rows);
        assert!(rows[0]["async_tasks"].is_null());
        assert!(rows[0]["async_shells"].is_null());
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
        assert!(rows[0]["async_tasks"].is_null());
    }
}
