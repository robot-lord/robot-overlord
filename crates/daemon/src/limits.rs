//! Sessions stopped by a usage limit, and picking them up when it resets.
//!
//! **Seeing it** is plain code. An agent that hits its usage limit ends the
//! turn with a record of it in its transcript, reset time included
//! (`transcript::limit_hit`; Claude Code only so far). A slow tick looks at
//! the tail of every live session's transcript that changed since the last
//! look, and keeps what it finds in kv `limits`: `{session id: {at,
//! resets_at, kind}}`. A session is **limited** from then until the limit
//! resets or the conversation goes on (a prompt, an answer), whichever is
//! first. The list shows it in place of the status ("limited until
//! 3:00pm"); it is not a stored status, so hooks and `discover` keep
//! theirs.
//!
//! **Resuming** happens only when asked (`limits.resume`: the overlord's
//! `resume_after_limit`, `rlord resume-after-limit`, the list's menu).
//! Each session asked for gets a job in kv `limit_resume`, which a daemon
//! restart doesn't forget. The same tick runs them. A job waits for its
//! limit's reset time plus `MARGIN_MS`, then:
//!
//! - the conversation went on since the limit hit (the user typed, or it
//!   answered): dropped. Someone picked it up already.
//! - otherwise the message (`[limits] resume_message`, "continue") is
//!   typed into the session the way `send_to_session` types (`send::
//!   type_message`: under the session's lock, only at its input prompt,
//!   not over a draft or while someone types there). Not possible right
//!   now: tried again each tick, up to `GIVE_UP_MS` past the reset.
//! - sent, and the limit hits again within `WATCH_MS`: the job waits for
//!   the new reset (at most `MAX_ATTEMPTS` sends in all).
//!
//! Everything a job does is in the session's log (`limit_resume_*`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde_json::{json, Map, Value};

use rlord_core::agents::transcript::{self, LimitHit};
use rlord_core::db::{ArchivedMode, Db, SessionQuery};
use rlord_core::timefmt::clock;
use rlord_core::types::{now_ms, Session, Status};

use crate::send::{self, Typed};
use crate::{overlord, Daemon};

/// kv: sessions stopped on a usage limit now, `{id: {at, resets_at, kind}}`.
pub const LIMITS_KEY: &str = "limits";
/// kv: resume jobs, `{id: {message, limit_at, resets_at, asked_at,
/// sent_at?, attempts}}`.
pub const JOBS_KEY: &str = "limit_resume";

const TICK: Duration = Duration::from_secs(15);
/// How long after the reset time a job sends (clocks differ a little).
const MARGIN_MS: i64 = 60_000;
/// After sending, how long a new limit still counts as "hit again".
const WATCH_MS: i64 = 15 * 60_000;
/// A session that can't be typed into this long after its reset: given up.
const GIVE_UP_MS: i64 = 60 * 60_000;
/// Sends per job, however often the limit hits again.
const MAX_ATTEMPTS: u64 = 5;

#[derive(Default)]
pub struct Limits {
    /// Held for a whole `scan`: two at once (the tick and a
    /// `limits.resume`) could otherwise each skip a file the other is
    /// reading, and the later save drop a limit.
    scan: Mutex<()>,
    /// Each transcript's size and change time when last looked at.
    seen: Mutex<HashMap<String, (u64, Option<SystemTime>)>>,
    /// kv `limits`, for the list (loaded on the first tick).
    now: Mutex<Option<Map<String, Value>>>,
}

fn kv_map(db: &Db, key: &str) -> Result<Map<String, Value>> {
    Ok(db
        .kv_get(key)?
        .and_then(|v| serde_json::from_str::<Value>(&v).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default())
}

fn save_map(db: &Db, key: &str, m: &Map<String, Value>) -> Result<()> {
    if m.is_empty() {
        db.kv_delete(key)
    } else {
        db.kv_set(key, &Value::Object(m.clone()).to_string())
    }
}

/// The limit a session is stopped on, from what its transcript says
/// (`hit`): None once the conversation went on or the limit has reset.
fn current(hit: Option<&LimitHit>, now: i64) -> Option<Value> {
    let h = hit.filter(|h| !h.touched && h.resets_at > now)?;
    Some(json!({"at": h.at, "resets_at": h.resets_at, "kind": h.kind}))
}

pub fn start(d: &Arc<Daemon>) {
    let d = Arc::clone(d);
    std::thread::spawn(move || loop {
        if let Err(e) = scan(&d) {
            tracing::debug!(error = %e, "looking for usage limits");
        }
        if let Err(e) = run_jobs(&d) {
            tracing::debug!(error = %e, "resuming sessions after their limit");
        }
        std::thread::sleep(TICK);
    });
}

/// The limits as last seen (loading kv the first time).
fn limits(d: &Daemon) -> Map<String, Value> {
    let mut now = d.limits.now.lock().expect("limits mutex");
    if now.is_none() {
        *now = Some(kv_map(&d.db(), LIMITS_KEY).unwrap_or_default());
    }
    now.clone().unwrap_or_default()
}

/// Look at every live session's transcript that changed since the last
/// look (all of them the first time), and at limits that have reset.
fn scan(d: &Daemon) -> Result<()> {
    let _scan = d.limits.scan.lock().expect("limits mutex");
    let before = limits(d);
    let sources: Vec<(String, String)> = {
        let db = d.db();
        let live: Vec<String> = db
            .sessions(&SessionQuery {
                include_overlord: true,
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
    let mut after = Map::new();
    // New stamps, kept once what was read is saved: a scan that fails
    // part way looks at those files again next time.
    let mut stamps = Vec::new();
    // (Files are read without the DB lock.)
    for (id, path) in &sources {
        let path = Path::new(path);
        let stamp = std::fs::metadata(path)
            .ok()
            .map(|m| (m.len(), m.modified().ok()));
        let Some(stamp) = stamp else {
            continue; // (not a file: OpenCode's database, or gone)
        };
        let unchanged = d.limits.seen.lock().expect("limits mutex").get(id) == Some(&stamp);
        if !unchanged {
            stamps.push((id.clone(), stamp));
        }
        let state = if unchanged {
            // Nothing new was written: only the clock can end it.
            before
                .get(id)
                .filter(|l| l["resets_at"].as_i64().is_some_and(|t| t > now))
                .cloned()
        } else {
            current(transcript::limit_hit(path).as_ref(), now)
        };
        if let Some(l) = state {
            after.insert(id.clone(), l);
        }
    }
    if after != before {
        {
            let db = d.db();
            for (id, l) in &after {
                if before.get(id).map(|b| &b["at"]) != Some(&l["at"]) {
                    db.insert_event(id, now, "limit_hit", &l.to_string())?;
                }
            }
            save_map(&db, LIMITS_KEY, &after)?;
        }
        *d.limits.now.lock().expect("limits mutex") = Some(after);
        d.changed();
    }
    d.limits.seen.lock().expect("limits mutex").extend(stamps);
    Ok(())
}

/// Annotate list rows with `limit`: `{resets_at, until, kind, resume}`
/// for a session stopped on a usage limit (`resume`: a job will pick it
/// up), else null.
pub fn annotate(d: &Daemon, rows: &mut [Value]) {
    let limits = limits(d);
    let jobs = if limits.is_empty() {
        Map::new()
    } else {
        kv_map(&d.db(), JOBS_KEY).unwrap_or_default()
    };
    let now = now_ms();
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        row["limit"] = match limits.get(id) {
            Some(l) if l["resets_at"].as_i64().is_some_and(|t| t > now) => json!({
                "resets_at": l["resets_at"],
                "until": clock(l["resets_at"].as_i64().unwrap_or(0)),
                "kind": l["kind"],
                "resume": jobs.contains_key(id),
            }),
            _ => Value::Null,
        };
    }
}

/// `limits.resume {sessions?: [ref], message?}`: schedule a resume for
/// each session named, or with none for every session limited now. Only
/// sessions stopped on a limit get one; the overlords never (the user
/// switches to another agent's instead).
pub fn resume(d: &Daemon, params: &Value) -> Result<Value> {
    // What is limited right now, not as of the last tick.
    scan(d)?;
    let limits = limits(d);
    let message = params["message"]
        .as_str()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| d.config().limits.resume_message.trim().to_string());
    let message = if message.is_empty() {
        "continue".to_string()
    } else {
        message
    };
    let db = d.db();
    let named: Vec<&str> = params["sessions"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut scheduled = Vec::new();
    let mut skipped = Vec::new();
    let targets: Vec<Session> = if named.is_empty() {
        limits
            .keys()
            .filter_map(|id| db.resolve_ref(id).ok())
            .filter(|s| !overlord::is_overlord_role(&s.role))
            .collect()
    } else {
        let mut v = Vec::new();
        for r in named {
            match db.resolve_ref(r) {
                Ok(s) => v.push(s),
                Err(e) => skipped.push(json!({"session": r, "why": format!("{e:#}")})),
            }
        }
        v
    };
    // (Each target's lock is taken before the DB, as `run_job` does.)
    drop(db);
    let now = now_ms();
    for s in targets {
        let why = if overlord::is_overlord_role(&s.role) {
            Some("that is the overlord: switch to another agent's overlord instead".to_string())
        } else if s.archived_at.is_some() {
            Some("archived".to_string())
        } else if !limits.contains_key(&s.id) {
            Some("not stopped on a usage limit".to_string())
        } else {
            None
        };
        if let Some(why) = why {
            skipped.push(json!({"session": s.name, "why": why}));
            continue;
        }
        let l = &limits[&s.id];
        let job = json!({
            "message": message, "limit_at": l["at"], "resets_at": l["resets_at"],
            "asked_at": now, "attempts": 0,
        });
        // Its lock: a job running now finishes before this one replaces it.
        let _session = d.sessions.lock(&s.id);
        let db = d.db();
        let mut jobs = kv_map(&db, JOBS_KEY)?;
        jobs.insert(s.id.clone(), job.clone());
        save_map(&db, JOBS_KEY, &jobs)?;
        db.insert_event(&s.id, now, "limit_resume_scheduled", &job.to_string())?;
        drop(db);
        scheduled.push(json!({
            "session": s.name,
            "at": clock(l["resets_at"].as_i64().unwrap_or(0) + MARGIN_MS),
        }));
    }
    d.changed();
    let note = if scheduled.is_empty() && skipped.is_empty() {
        "no session is stopped on a usage limit right now"
    } else {
        "each is sent the message once its limit resets, unless someone touches it first"
    };
    Ok(json!({"scheduled": scheduled, "skipped": skipped, "message": message, "note": note}))
}

/// What to do with a job now.
#[derive(Debug, PartialEq)]
enum Step {
    Wait,
    /// Type the message in.
    Send,
    /// Wait for a newer limit's reset: (limit_at, resets_at).
    Reschedule(i64, i64),
    /// Forget the job (why; `done`: it did its work).
    Drop(&'static str, bool),
}

/// Decide a job's next step from the session (None: gone) and what its
/// transcript says now.
fn step(job: &Value, s: Option<&Session>, hit: Option<&LimitHit>, now: i64) -> Step {
    let Some(s) = s else {
        return Step::Drop("the session is gone", false);
    };
    if s.archived_at.is_some() {
        return Step::Drop("the session was archived", false);
    }
    let limit_at = job["limit_at"].as_i64().unwrap_or(0);
    let resets_at = job["resets_at"].as_i64().unwrap_or(0);
    // A limit newer than the one this job waits on, with nothing after it.
    let newer = hit.filter(|h| h.at > limit_at && !h.touched);
    if let Some(sent) = job["sent_at"].as_i64() {
        return match newer {
            Some(_) if job["attempts"].as_u64().unwrap_or(0) >= MAX_ATTEMPTS => {
                Step::Drop("the limit kept hitting", false)
            }
            Some(h) => Step::Reschedule(h.at, h.resets_at.max(h.at)),
            None if now - sent > WATCH_MS => Step::Drop("resumed", true),
            None => Step::Wait,
        };
    }
    if let Some(h) = newer {
        return Step::Reschedule(h.at, h.resets_at.max(h.at));
    }
    // The conversation went on since the limit hit.
    let touched = match hit {
        Some(h) if h.at == limit_at => h.touched,
        // (Its record is no longer in the transcript's tail: a lot was
        // written since.)
        _ => true,
    };
    if touched || s.last_user_msg_at.is_some_and(|t| t > limit_at) {
        return Step::Drop("the session was used since the limit hit", false);
    }
    if now < resets_at + MARGIN_MS {
        return Step::Wait;
    }
    if s.status == Status::Stopped {
        return Step::Drop("the session is stopped", false);
    }
    if now > resets_at + GIVE_UP_MS {
        return Step::Drop(
            "it couldn't be typed into for an hour after the reset",
            false,
        );
    }
    // Between turns. "Your turn"; or still "working": the untouched limit
    // record (checked above) is where the turn ended, whatever the hooks
    // said (an older install has no `StopFailure` hook). Not "needs you":
    // that is blocked on a prompt. Its screen is checked right before
    // typing either way.
    match s.status {
        Status::Idle | Status::Working => Step::Send,
        _ => Step::Wait,
    }
}

fn run_jobs(d: &Daemon) -> Result<()> {
    let ids: Vec<String> = kv_map(&d.db(), JOBS_KEY)?.keys().cloned().collect();
    for id in ids {
        if let Err(e) = run_job(d, &id) {
            tracing::debug!(session = %id, error = %e, "limit resume job");
        }
    }
    Ok(())
}

fn run_job(d: &Daemon, id: &str) -> Result<()> {
    // Its lock first (locks.rs): nothing restarts or stops it, and nothing
    // else types into it, between the look and the paste.
    let _session = d.sessions.lock(id);
    let (job, s, src) = {
        let db = d.db();
        let Some(job) = kv_map(&db, JOBS_KEY)?.remove(id) else {
            return Ok(());
        };
        (
            job,
            db.resolve_ref(id).ok(),
            db.transcript_source(id).ok().flatten(),
        )
    };
    let hit = src.and_then(|p| transcript::limit_hit(Path::new(&p)));
    let now = now_ms();
    // Only the job read above: one rescheduled meanwhile (`limits.resume`
    // asked again) is left alone.
    let same = |j: &Value| ["limit_at", "asked_at"].iter().all(|k| j[*k] == job[*k]);
    let update = |f: &dyn Fn(&mut Map<String, Value>)| -> Result<()> {
        let db = d.db();
        let mut jobs = kv_map(&db, JOBS_KEY)?;
        if !jobs.get(id).is_some_and(same) {
            return Ok(());
        }
        f(&mut jobs);
        save_map(&db, JOBS_KEY, &jobs)
    };
    let log = |kind: &str, p: Value| -> Result<()> {
        if s.is_some() {
            d.db().insert_event(id, now, kind, &p.to_string())?;
        }
        Ok(())
    };
    match step(&job, s.as_ref(), hit.as_ref(), now) {
        Step::Wait => return Ok(()),
        Step::Drop(why, done) => {
            update(&|j| {
                j.remove(id);
            })?;
            if !done {
                log("limit_resume_skipped", json!({"why": why}))?;
            }
        }
        Step::Reschedule(limit_at, resets_at) => {
            update(&|j| {
                if let Some(job) = j.get_mut(id).and_then(Value::as_object_mut) {
                    job.insert("limit_at".into(), json!(limit_at));
                    job.insert("resets_at".into(), json!(resets_at));
                    job.remove("sent_at");
                }
            })?;
            log(
                "limit_resume_rescheduled",
                json!({"resets_at": resets_at, "why": "the limit hit again"}),
            )?;
        }
        Step::Send => {
            let Some(s) = s.as_ref() else {
                return Ok(());
            };
            let text = job["message"].as_str().unwrap_or("continue");
            match send::type_message(d, s, text)? {
                // (Tried again next tick; `step` gives up in time.)
                Typed::NotNow(why) => {
                    tracing::debug!(session = %id, %why, "limit resume held");
                    return Ok(());
                }
                Typed::Done => {
                    let attempts = job["attempts"].as_u64().unwrap_or(0) + 1;
                    update(&|j| {
                        if let Some(job) = j.get_mut(id).and_then(Value::as_object_mut) {
                            job.insert("sent_at".into(), json!(now));
                            job.insert("attempts".into(), json!(attempts));
                        }
                    })?;
                    log(
                        "limit_resumed",
                        json!({"chars": text.chars().count(), "attempt": attempts}),
                    )?;
                }
            }
        }
    }
    d.changed();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(status: Status) -> Session {
        serde_json::from_value(json!({
            "id": "01s", "name": "web", "name_locked": false, "agent": "claude",
            "agent_session": null, "agent_session_trust": "hook", "project_path": "/tmp",
            "tmux_session": null, "status": status, "group_path": "", "pinned": false,
            "created_at": 0, "archived_at": null, "last_user_msg_at": 500,
            "last_agent_msg_at": null, "last_state_change_at": null, "repo_path": null,
            "workspace_path": null, "branch": null, "model": null, "summary_at": null,
        }))
        .unwrap()
    }

    fn hit(at: i64, resets_at: i64, touched: bool) -> LimitHit {
        LimitHit {
            at,
            resets_at,
            kind: "five_hour".into(),
            touched,
        }
    }

    #[test]
    fn limited_until_it_resets_or_the_conversation_goes_on() {
        assert!(current(Some(&hit(1_000, 9_000, false)), 2_000).is_some());
        assert_eq!(current(Some(&hit(1_000, 9_000, true)), 2_000), None);
        assert_eq!(current(Some(&hit(1_000, 9_000, false)), 9_000), None);
        assert_eq!(current(None, 2_000), None);
    }

    #[test]
    fn a_job_waits_for_the_reset_then_sends_once() {
        let job = json!({"message": "continue", "limit_at": 1_000, "resets_at": 100_000,
                         "attempts": 0});
        let idle = session(Status::Idle);
        let h = hit(1_000, 100_000, false);
        // Before the reset, and inside the margin after it: wait.
        assert_eq!(step(&job, Some(&idle), Some(&h), 50_000), Step::Wait);
        assert_eq!(
            step(&job, Some(&idle), Some(&h), 100_000 + 5_000),
            Step::Wait
        );
        let due = 100_000 + MARGIN_MS;
        assert_eq!(step(&job, Some(&idle), Some(&h), due), Step::Send);
        // "Needs you" is blocked on a prompt: not now.
        let waiting = session(Status::WaitingInput);
        assert_eq!(step(&job, Some(&waiting), Some(&h), due), Step::Wait);
        // Still reading "working" (no `StopFailure` hook): the untouched
        // limit record ended the turn.
        let working = session(Status::Working);
        assert_eq!(step(&job, Some(&working), Some(&h), due), Step::Send);
        // Stopped: never.
        let stopped = session(Status::Stopped);
        assert!(matches!(
            step(&job, Some(&stopped), Some(&h), due),
            Step::Drop(_, false)
        ));
        assert!(matches!(
            step(&job, Some(&idle), Some(&h), 100_000 + GIVE_UP_MS + 1),
            Step::Drop(_, false)
        ));
        assert!(matches!(step(&job, None, None, due), Step::Drop(_, false)));
    }

    #[test]
    fn a_touched_session_is_left_alone() {
        let job = json!({"message": "continue", "limit_at": 1_000, "resets_at": 100_000,
                         "attempts": 0});
        let idle = session(Status::Idle);
        let due = 100_000 + MARGIN_MS;
        // The transcript went on after the limit.
        let touched = hit(1_000, 100_000, true);
        assert!(matches!(
            step(&job, Some(&idle), Some(&touched), due),
            Step::Drop(_, false)
        ));
        // Even before the reset: there is nothing left to wait for.
        assert!(matches!(
            step(&job, Some(&idle), Some(&touched), 2_000),
            Step::Drop(_, false)
        ));
        // rlord saw a prompt after the limit.
        let mut prompted = session(Status::Idle);
        prompted.last_user_msg_at = Some(1_500);
        let h = hit(1_000, 100_000, false);
        assert!(matches!(
            step(&job, Some(&prompted), Some(&h), due),
            Step::Drop(_, false)
        ));
        // The limit's record scrolled out of the transcript's tail.
        assert!(matches!(
            step(&job, Some(&idle), None, due),
            Step::Drop(_, false)
        ));
    }

    #[test]
    fn a_limit_that_hits_again_reschedules() {
        let idle = session(Status::Idle);
        let sent = json!({"message": "continue", "limit_at": 1_000, "resets_at": 100_000,
                          "sent_at": 170_000, "attempts": 1});
        // Our own message sits after the old limit: that is not "touched".
        let old = hit(1_000, 100_000, true);
        assert_eq!(step(&sent, Some(&idle), Some(&old), 180_000), Step::Wait);
        // It hit again: wait for the new reset.
        let again = hit(175_000, 900_000, false);
        assert_eq!(
            step(&sent, Some(&idle), Some(&again), 180_000),
            Step::Reschedule(175_000, 900_000)
        );
        // Nothing new for a while: the job is done.
        assert_eq!(
            step(&sent, Some(&idle), Some(&old), 170_000 + WATCH_MS + 1),
            Step::Drop("resumed", true)
        );
        // Not for ever.
        let mut many = sent.clone();
        many["attempts"] = json!(MAX_ATTEMPTS);
        assert!(matches!(
            step(&many, Some(&idle), Some(&again), 180_000),
            Step::Drop(_, false)
        ));
        // A reset time already past doesn't make it send at once: never
        // before the limit's own time plus the margin.
        let stale = hit(175_000, 100_000, false);
        assert_eq!(
            step(&sent, Some(&idle), Some(&stale), 180_000),
            Step::Reschedule(175_000, 175_000)
        );
    }

    /// Scans at once (the tick and a `limits.resume`) never lose a limit
    /// one of them read.
    #[test]
    fn concurrent_scans_keep_the_limit() {
        const LINE: &str = r#"{"type":"assistant","timestamp":"2026-11-12T18:40:58.108Z","message":{"content":[{"type":"text","text":"x"}]},"apiError":"usage_limit_reached","apiErrorParams":{"rate_limit_info":{"status":"rejected","resetsAt":99999999999,"rateLimitType":"five_hour"}},"error":"rate_limit","isApiErrorMessage":true}"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, format!("{LINE}\n")).unwrap();
        for _ in 0..20 {
            let db = Db::open_in_memory().unwrap();
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
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    let d = Arc::clone(&d);
                    std::thread::spawn(move || scan(&d).unwrap())
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            assert!(limits(&d).contains_key("s1"));
            assert!(kv_map(&d.db(), LIMITS_KEY).unwrap().contains_key("s1"));
        }
    }
}
