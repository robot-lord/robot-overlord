//! Cheap, deterministic triage for the overlord's upkeep.
//!
//! The overlord keeps every session's name, description and tags current.
//! Doing that by asking each session's agent (or reading every screen) is
//! slow and costs tokens, so rlord does the first pass in plain code:
//! which live sessions changed since the overlord last reviewed them, what
//! their meta-context is missing, and short excerpts from their transcripts
//! (first prompt, latest prompt, latest reply, the agent's own title). The
//! overlord spends AI effort only on what this flags.
//!
//! "Reviewed" is a per-session watermark in kv (`reviewed:<id>` = ms). It is
//! set by `mark_reviewed` and by any overlord edit through `update_session`
//! or `organize`. A session needs review when it was never reviewed or has
//! been updated (message either way, or status change) since.

use anyhow::Result;
use serde_json::{json, Value};

use rlord_core::agents::claude;
use rlord_core::db::Db;
use rlord_core::timefmt::rel_age;
use rlord_core::types::{now_ms, Session, Status};

fn kv_key(id: &str) -> String {
    format!("reviewed:{id}")
}

fn reviewed_at(db: &Db, id: &str) -> Option<i64> {
    db.kv_get(&kv_key(id))
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
}

/// Latest activity: the same "updated" the list shows. Messages only: a
/// resume or Claude's idle notification changes the state, not the work.
pub fn updated_at(s: &Session) -> i64 {
    // An error is news, even without a message.
    let failed = (s.status == Status::Error)
        .then_some(s.last_state_change_at)
        .flatten();
    [s.last_agent_msg_at, s.last_user_msg_at, failed]
        .into_iter()
        .flatten()
        .fold(s.created_at, i64::max)
}

/// What's missing from a session's meta-context.
fn flags(s: &Session) -> Vec<&'static str> {
    let mut f = Vec::new();
    if !s.name_locked {
        f.push("default_name");
    }
    if s.description.trim().is_empty() {
        f.push("no_description");
    }
    if s.tags.is_empty() {
        f.push("untagged");
    }
    if s.status == Status::WaitingInput {
        f.push("waiting_on_user");
    }
    f
}

/// `{"all": bool, "limit": n}` → sessions needing review, most recently
/// updated first. `all` lists every live session regardless of watermark.
pub fn triage(d: &crate::Daemon, params: &Value) -> Result<Value> {
    // Gather under the DB lock, read transcripts (file I/O) after it's released.
    let (mut out, sources) = {
        let db = d.db();
        collect(&db, params)?
    };
    fill_excerpts(&mut out, &sources);
    Ok(out)
}

/// Rows plus each row's transcript path (claude sessions only).
type Collected = (Value, Vec<Option<String>>);

fn collect(db: &Db, params: &Value) -> Result<Collected> {
    let all = params.get("all").and_then(Value::as_bool).unwrap_or(false);
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
    let mut live: Vec<Session> = db
        .list_sessions(false)?
        .into_iter()
        .filter(|s| s.role.is_empty())
        .collect();
    live.sort_by_key(|s| std::cmp::Reverse(updated_at(s)));
    let total = live.len();
    let mut pending = Vec::new();
    for s in &live {
        let reviewed = reviewed_at(db, &s.id);
        let changed = reviewed.is_none_or(|r| updated_at(s) > r);
        if changed || all {
            pending.push((s, reviewed, changed));
        }
    }
    let needing = pending.iter().filter(|(_, _, c)| *c).count();
    let mut sources = Vec::new();
    let rows: Vec<Value> = pending
        .into_iter()
        .take(limit)
        .map(|(s, reviewed, changed)| {
            sources.push(if s.agent == "claude" {
                db.transcript_source(&s.id).ok().flatten()
            } else {
                None
            });
            json!({
                "id": s.id, "name": s.name, "status": s.status,
                "dir": s.project_path, "branch": s.branch,
                "description": s.description, "tags": s.tags,
                "has_notes": !s.notes.trim().is_empty(),
                "updated": rel_age(Some(updated_at(s))),
                "reviewed": reviewed.map(|r| rel_age(Some(r))),
                "changed_since_review": changed,
                "flags": flags(s),
            })
        })
        .collect();
    let out = json!({
        "live_sessions": total,
        "needing_review": needing,
        "shown": rows.len(),
        "sessions": rows,
    });
    Ok((out, sources))
}

fn fill_excerpts(out: &mut Value, sources: &[Option<String>]) {
    let Some(rows) = out["sessions"].as_array_mut() else {
        return;
    };
    for (row, src) in rows.iter_mut().zip(sources) {
        if let Some(src) = src {
            let e = claude::transcript_excerpts(std::path::Path::new(src));
            row["excerpts"] = serde_json::to_value(e).unwrap_or(Value::Null);
        }
    }
}

/// `triage` against a bare `Db` (tests).
#[cfg(test)]
fn triage_db(db: &Db, params: &Value) -> Result<Value> {
    let (mut out, sources) = collect(db, params)?;
    fill_excerpts(&mut out, &sources);
    Ok(out)
}

/// Set the watermark on the given session ids (already resolved) to now.
pub fn mark(db: &Db, ids: &[String]) -> Result<()> {
    let now = now_ms().to_string();
    for id in ids {
        db.kv_set(&kv_key(id), &now)?;
    }
    Ok(())
}

/// `{"sessions": [refs]}` or `{"all": true}` → mark reviewed.
pub fn mark_reviewed(db: &Db, params: &Value) -> Result<Value> {
    let ids: Vec<String> = if params.get("all").and_then(Value::as_bool) == Some(true) {
        db.list_sessions(false)?
            .into_iter()
            .filter(|s| s.role.is_empty())
            .map(|s| s.id)
            .collect()
    } else {
        params
            .get("sessions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .map(|r| db.resolve_ref(r).map(|s| s.id))
            .collect::<Result<_>>()?
    };
    mark(db, &ids)?;
    Ok(json!({"marked": ids.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_sessions_drop_out_once_reviewed() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id: "01web",
            name: "web",
            agent: "shell",
            agent_session: None,
            project_path: "/tmp",
            tmux_session: Some("rlord-test-web"),
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        let s = db.resolve_ref("01web").unwrap();
        let t = triage_db(&db, &json!({})).unwrap();
        assert_eq!(t["needing_review"], 1);
        let flags = t["sessions"][0]["flags"].as_array().unwrap();
        assert!(flags.contains(&json!("untagged")) && flags.contains(&json!("no_description")));

        mark_reviewed(&db, &json!({"sessions": [s.id]})).unwrap();
        assert_eq!(triage_db(&db, &json!({})).unwrap()["needing_review"], 0);
        // `all` still lists it, marked unchanged.
        let t = triage_db(&db, &json!({"all": true})).unwrap();
        assert_eq!(t["sessions"][0]["changed_since_review"], false);

        // A state change alone (a resume) isn't new work.
        db.set_status(&s.id, Status::Working, now_ms() + 1000)
            .unwrap();
        assert_eq!(triage_db(&db, &json!({})).unwrap()["needing_review"], 0);
        // New activity after the review brings it back.
        db.touch_last_agent_msg(&s.id, now_ms() + 2000).unwrap();
        assert_eq!(triage_db(&db, &json!({})).unwrap()["needing_review"], 1);
    }
}
