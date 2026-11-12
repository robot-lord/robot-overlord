//! "We did this before": search what sessions said and did, then read
//! around a hit to carry it somewhere else. Plain code (`rlord_core::history`
//! does the reading); every answer is bounded.
//!
//! Search looks at a session's summary, description, notes, name and tags
//! (from the DB) and in its transcript. Sessions matching on what they're
//! about come first, then by how often the transcript mentions it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::claude;
use rlord_core::db::{ArchivedMode, SessionQuery};
use rlord_core::history;
use rlord_core::types::Session;

use crate::{summary, Daemon};

/// Hits shown per transcript.
const PER_FILE: usize = 3;

fn words(q: &str) -> Vec<String> {
    q.split_whitespace().map(str::to_lowercase).collect()
}

/// Which of a session's own fields mention every word.
fn matched_fields(s: &Session, ws: &[String]) -> Vec<&'static str> {
    let fields: [(&'static str, String); 4] = [
        ("summary", s.summary.to_lowercase()),
        ("description", s.description.to_lowercase()),
        ("notes", s.notes.to_lowercase()),
        (
            "name",
            format!("{} {}", s.name, s.tags.join(" ")).to_lowercase(),
        ),
    ];
    fields
        .into_iter()
        .filter(|(_, v)| ws.iter().all(|w| v.contains(w.as_str())))
        .map(|(k, _)| k)
        .collect()
}

pub fn search(d: &Daemon, params: &Value) -> Result<Value> {
    let query = params["query"].as_str().unwrap_or("").trim();
    if query.is_empty() {
        bail!("query is required");
    }
    let ws = words(query);
    let everywhere = params["scope"].as_str() == Some("all");
    let limit = params["limit"].as_u64().unwrap_or(8).clamp(1, 25) as usize;
    let sessions = {
        let q = SessionQuery {
            archived: ArchivedMode::All,
            ..Default::default()
        };
        d.db().sessions(&q)?
    };
    // Each session's transcripts: the current one, then any from before a
    // `/clear`. Searched in that order, so a session's hits start with its
    // current transcript. (Not the overlord's own.)
    let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
    let mut paths_of: HashMap<usize, Vec<PathBuf>> = HashMap::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    for (i, s) in sessions.iter().enumerate() {
        if crate::overlord::is_overlord_role(&s.role) {
            continue;
        }
        let ps = summary::transcripts_of(d, s);
        for p in &ps {
            if by_path.insert(p.clone(), i).is_none() {
                paths.push(p.clone());
            }
        }
        paths_of.insert(i, ps);
    }
    if everywhere {
        paths.extend(
            history::all_claude_transcripts()
                .into_iter()
                .filter(|p| !by_path.contains_key(p)),
        );
    }
    let found = history::search(&paths, query, PER_FILE);
    // A session's hits across its transcripts (current first); each hit
    // says which transcript it's in when it isn't the current one.
    let mut hits: HashMap<usize, (usize, Vec<Value>)> = HashMap::new();
    let mut outside = Vec::new();
    for fh in found {
        match by_path.get(&fh.path) {
            Some(&i) => {
                let current = paths_of.get(&i).and_then(|v| v.first()) == Some(&fh.path);
                let e = hits.entry(i).or_default();
                e.0 += fh.total;
                for h in fh.hits {
                    if e.1.len() < PER_FILE {
                        let mut v = json!(h);
                        if !current {
                            v["transcript"] = json!(fh.path);
                        }
                        e.1.push(v);
                    }
                }
            }
            None => outside.push(fh),
        }
    }
    // Sessions that match on what they're about, or in their transcript.
    let mut rows: Vec<(usize, Vec<&'static str>, usize)> = sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| !crate::overlord::is_overlord_role(&s.role))
        .map(|(i, s)| (i, matched_fields(s, &ws), hits.get(&i).map_or(0, |h| h.0)))
        .filter(|(_, f, n)| !f.is_empty() || *n > 0)
        .collect();
    let recency = |s: &Session| crate::triage::updated_at(s);
    rows.sort_by(|a, b| {
        let fa = !a.1.is_empty();
        let fb = !b.1.is_empty();
        fb.cmp(&fa)
            .then(b.2.cmp(&a.2))
            .then(recency(&sessions[b.0]).cmp(&recency(&sessions[a.0])))
    });
    let total = rows.len();
    let out: Vec<Value> = rows
        .into_iter()
        .take(limit)
        .map(|(i, fields, n)| {
            let s = &sessions[i];
            let about = if !s.summary.trim().is_empty() {
                &s.summary
            } else {
                &s.description
            };
            json!({
                "session": s.name, "id": s.id,
                "archived": s.archived_at.is_some(),
                "status": s.status.as_str(),
                "about": claude::clip(about, 240),
                "matched_in": fields,
                "transcript_hits": n,
                "hits": hits.get(&i).map(|h| json!(h.1)).unwrap_or(json!([])),
            })
        })
        .collect();
    let outside_total = outside.len();
    outside.sort_by_key(|f| std::cmp::Reverse(f.total));
    let outside: Vec<Value> = outside
        .into_iter()
        .take(limit)
        .map(|fh| {
            json!({
                "transcript": fh.path,
                "folder": project_of(&fh.path),
                "transcript_hits": fh.total,
                "hits": fh.hits,
            })
        })
        .collect();
    let mut v = json!({
        "query": query,
        "sessions": out,
        "more_sessions": total.saturating_sub(limit),
        "note": "read_history with a session (or an outside transcript) and a hit's line reads the conversation around it",
    });
    if everywhere {
        v["outside_rlord"] = json!(outside);
        v["more_outside"] = json!(outside_total.saturating_sub(limit));
    }
    Ok(v)
}

/// The folder a Claude transcript belongs to (its project directory name,
/// which is the path with `/` as `-`).
fn project_of(p: &Path) -> String {
    p.parent()
        .and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn read(d: &Daemon, params: &Value) -> Result<Value> {
    let line = params["line"].as_u64();
    let before = params["before"].as_u64().unwrap_or(6).min(40) as usize;
    let after = params["after"].as_u64().unwrap_or(14).min(60) as usize;
    let (label, path) = match (params["session"].as_str(), params["transcript"].as_str()) {
        // A hit in a session's older transcript names that transcript.
        (Some(r), t) if !r.trim().is_empty() => {
            let s = d.db().resolve_ref(r)?;
            let all = summary::transcripts_of(d, &s);
            let p = match t {
                Some(t) => all
                    .into_iter()
                    .find(|p| p.to_string_lossy() == t)
                    .context("that transcript isn't this session's")?,
                None => all
                    .into_iter()
                    .next()
                    .context("that session has no transcript on disk")?,
            };
            (json!({"session": s.name, "id": s.id}), p)
        }
        (_, Some(t)) => {
            // Only Claude Code's own transcripts.
            let root = std::fs::canonicalize(claude::config_dir().join("projects"))?;
            let p = std::fs::canonicalize(t).context("no such transcript")?;
            if !p.starts_with(&root) || p.extension().is_none_or(|x| x != "jsonl") {
                bail!("not a Claude Code transcript");
            }
            (json!({"transcript": p}), p)
        }
        _ => bail!("session or transcript is required"),
    };
    let msgs = history::read(&path, line, before, after, 12_000)?;
    let mut v = label;
    v["messages"] = json!(msgs);
    Ok(v)
}

pub fn summarize(d: &Daemon, params: &Value) -> Result<Value> {
    let force = params["force"].as_bool().unwrap_or(false);
    let refs: Vec<String> = match &params["sessions"] {
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => params["session"]
            .as_str()
            .map(str::to_string)
            .into_iter()
            .collect(),
    };
    if refs.is_empty() {
        bail!("sessions is required");
    }
    if refs.len() > 20 {
        bail!("at most 20 at a time (each is a model call)");
    }
    let (mut queued, mut skipped) = (Vec::new(), Vec::new());
    for r in refs {
        let s = match d.db().resolve_ref(&r) {
            Ok(s) => s,
            Err(e) => {
                skipped.push(json!({"session": r, "why": e.to_string()}));
                continue;
            }
        };
        let why = if s.agent != "claude" {
            Some("not a Claude Code session")
        } else if !force && !s.summary.trim().is_empty() {
            Some("already has one (force to redo)")
        } else if summary::transcript_of(d, &s).is_none() {
            Some("no transcript on disk")
        } else if !summary::enqueue(d, &s.id, force) {
            Some("already being written")
        } else {
            None
        };
        match why {
            Some(w) => skipped.push(json!({"session": s.name, "why": w})),
            None => queued.push(json!(s.name)),
        }
    }
    Ok(json!({"queued": queued, "skipped": skipped,
              "note": "summaries are written in the background, one at a time; list_sessions shows them when done"}))
}
