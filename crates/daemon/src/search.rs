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

use rlord_core::agents::transcript::clip;
use rlord_core::agents::{self, Agent};
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

/// The overlord's session, or one that ran in an overlord's folder (the
/// retired front desk of a two-session overlord).
fn is_overlords(s: &Session, dirs: &[String]) -> bool {
    crate::overlord::is_overlord_role(&s.role) || dirs.contains(&s.project_path)
}

pub fn search(d: &Daemon, params: &Value) -> Result<Value> {
    let query = params["query"].as_str().unwrap_or("").trim();
    if query.is_empty() {
        bail!("query is required");
    }
    let ws = words(query);
    let everywhere = params["scope"].as_str() == Some("all");
    let limit = params["limit"].as_u64().unwrap_or(8).clamp(1, 25) as usize;
    // One session ("what happened in X?"): only it, and its transcripts.
    let one = match params["session"].as_str().map(str::trim) {
        Some(r) if !r.is_empty() => {
            let s = d.db().resolve_ref(r)?;
            if crate::overlord::is_overlord_role(&s.role) {
                bail!("the overlord's own conversations are searched with recall");
            }
            Some(s)
        }
        _ => None,
    };
    let named = one.is_some();
    let everywhere = everywhere && !named;
    let sessions = match one {
        Some(s) => vec![s],
        None => {
            let q = SessionQuery {
                archived: ArchivedMode::All,
                ..Default::default()
            };
            d.db().sessions(&q)?
        }
    };
    // Each session's transcripts: the current one, then any from before a
    // `/clear`. Searched in that order, so a session's hits start with its
    // current transcript. (Not the overlord's own.)
    let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
    let mut paths_of: HashMap<usize, Vec<PathBuf>> = HashMap::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    // The overlord's own conversations are full of searches: never results
    // (it looks its own past up with `recall`).
    // Whole folders: it runs in its own, so every conversation it ever had
    // is in there, known to rlord or not.
    // (Claude keeps a folder's conversations together, so its transcript
    // directory stands for the folder; a Codex rollout and an OpenCode
    // session say their folder.)
    let own_dirs: Vec<String> = crate::overlord::dirs()
        .iter()
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect();
    let skip: std::collections::HashSet<PathBuf> = own_dirs
        .iter()
        .filter_map(|dir| {
            Agent::Claude
                .transcript_path(dir, "x")?
                .parent()
                .map(Path::to_path_buf)
        })
        .collect();
    // A session that ran in an overlord's folder (the retired front desk)
    // is searched only when named: then its own transcripts are what was
    // asked for.
    let dropped = |s: &Session| !named && is_overlords(s, &own_dirs);
    for (i, s) in sessions.iter().enumerate() {
        if dropped(s) {
            // (Its folder is skipped whole, above: no need to look its
            // transcripts up one by one.)
            continue;
        }
        let ps = summary::transcripts_of(d, s);
        if named && ps.is_empty() && is_overlords(s, &own_dirs) {
            bail!("{} has no transcript left on disk to search", s.name);
        }
        for p in &ps {
            if by_path.insert(p.clone(), i).is_none() {
                paths.push(p.clone());
            }
        }
        paths_of.insert(i, ps);
    }
    if everywhere {
        // Codex rollouts sit together by date: which folder each ran in is
        // one question to Codex's index; a rollout it doesn't list (older
        // than the index, or no index at all) says so in its first bytes.
        // Its folder may be recorded as given or resolved (symlinks).
        let folders = agents::codex::rollout_folders(&agents::codex::home()).unwrap_or_default();
        let own_real: Vec<String> = own_dirs
            .iter()
            .filter_map(|d| std::fs::canonicalize(d).ok())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let is_own = |cwd: &str| own_dirs.iter().chain(&own_real).any(|d| d == cwd);
        let in_own_folder = |p: &Path| match agents::agent_of(p) {
            Agent::Claude => false, // (by its directory: `skip`)
            Agent::Codex => match folders.get(p) {
                Some(cwd) => is_own(cwd),
                None => agents::codex::rollout_cwd(p).is_some_and(|cwd| is_own(&cwd)),
            },
            Agent::OpenCode => is_own(&Agent::OpenCode.transcript_folder(p)),
        };
        paths.extend(history::all_transcripts().into_iter().filter(|p| {
            !by_path.contains_key(p)
                && !p.parent().is_some_and(|d| skip.contains(d))
                && !in_own_folder(p)
        }));
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
        .filter(|(_, s)| !dropped(s))
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
                "about": clip(about, 240),
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
                "agent": agents::agent_of(&fh.path).name(),
                "folder": agents::agent_of(&fh.path).transcript_folder(&fh.path),
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
        // A conversation in OpenCode's database (its path isn't a file).
        (_, Some(t)) if agents::opencode::is_transcript(Path::new(t)) => {
            let p = PathBuf::from(t);
            let ours = p.parent() == Some(Agent::OpenCode.transcripts_root().as_path());
            if !ours || !agents::transcript::exists(&p) {
                bail!("not an agent's transcript");
            }
            (json!({"transcript": p}), p)
        }
        (_, Some(t)) => {
            // Only the agents' own transcripts.
            let p = std::fs::canonicalize(t).context("no such transcript")?;
            let known = Agent::ALL.into_iter().any(|a| {
                std::fs::canonicalize(a.transcripts_root()).is_ok_and(|root| p.starts_with(root))
            });
            if !known || p.extension().is_none_or(|x| x != "jsonl") {
                bail!("not an agent's transcript");
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
        let why = if Agent::parse(&s.agent).is_none() {
            Some("its agent can't write one")
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

#[cfg(test)]
mod tests {
    use super::*;
    use rlord_core::config::Config;
    use rlord_core::db::{Db, NewSession};

    /// The retired front desk ran in the overlord's folder: left out of a
    /// search across sessions, searched when named.
    #[test]
    fn the_front_desk_is_searched_when_named() {
        let dir = std::env::temp_dir().join(format!("rlord-search-fd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("conv-fd.jsonl");
        std::fs::write(
            &t,
            json!({"type": "user", "timestamp": "2026-01-01T00:00:00Z",
                   "message": {"role": "user", "content": "launch the zebracorn report"}})
            .to_string()
                + "\n",
        )
        .unwrap();
        let db = Db::open_in_memory().unwrap();
        let folder = crate::overlord::dirs()[0].to_string_lossy().into_owned();
        db.insert_session(&NewSession {
            id: "01FRONT",
            name: "overlord-front-desk",
            agent: "claude",
            agent_session: Some("conv-fd"),
            project_path: &folder,
            tmux_session: None,
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        db.set_archived("01FRONT", Some(2)).unwrap();
        db.upsert_transcript_source("01FRONT", &t.to_string_lossy())
            .unwrap();
        let d = Daemon::new(db, Config::default());
        let all = search(&d, &json!({"query": "zebracorn"})).unwrap();
        assert_eq!(all["sessions"], json!([]), "{all}");
        let one = search(
            &d,
            &json!({"query": "zebracorn", "session": "overlord-front-desk"}),
        )
        .unwrap();
        assert_eq!(one["sessions"][0]["id"], "01FRONT", "{one}");
        assert_eq!(one["sessions"][0]["transcript_hits"], 1, "{one}");
        // Its transcript gone: a clear error, not an empty answer.
        std::fs::remove_dir_all(&dir).unwrap();
        let e = search(
            &d,
            &json!({"query": "zebracorn", "session": "overlord-front-desk"}),
        )
        .unwrap_err();
        assert!(e.to_string().contains("no transcript"), "{e}");
    }
}
