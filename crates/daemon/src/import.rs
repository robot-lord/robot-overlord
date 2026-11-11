//! `session.import_scan`: agent conversations on disk that rlord doesn't
//! track yet (see `rlord_core::import`). Plain code, no AI: the overlord's
//! `rlord-import` subagent decides which are worth importing.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::git;
use rlord_core::import::{add_git_info, explain_claude, scan_claude, ScanOpts};
use rlord_core::types::now_ms;

use crate::methods::{self, p_str, str_list};
use crate::Daemon;

const HOUR_MS: i64 = 3_600_000;

/// Params: `since_days` (default 30; by latest exchange, not start) or
/// `since_hours` (wins when given),
/// `limit` (default 30, max 200), `ids` (only these conversations, any age),
/// `query` (words that must all appear in the folder, branch, title or prompts).
pub fn scan(d: &Daemon, params: &Value) -> Result<Value> {
    // Only the tracked set needs the DB; the file scan runs without the lock.
    let tracked = d.db().agent_session_ids()?;
    let window_ms = match params["since_hours"].as_i64() {
        Some(h) => h.max(0) * HOUR_MS,
        None => params["since_days"].as_i64().unwrap_or(30).max(0) * 24 * HOUR_MS,
    };
    let limit = params["limit"].as_u64().unwrap_or(30).clamp(1, 200) as usize;
    let words: Vec<String> = params["query"]
        .as_str()
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let only = str_list(params, "ids");
    let opts = ScanOpts {
        since_ms: now_ms() - window_ms,
        // Filter by query before limiting, so the limit counts matches; ids
        // asked for are all looked at.
        limit: if words.is_empty() && only.is_empty() {
            limit
        } else {
            0
        },
        only: only.clone(),
    };
    let mut found = scan_claude(&opts, &|id| tracked.contains(id));
    // Asked for by id but not importable at all (judged before query and
    // limit, which only narrow the listing): say why.
    let rejected: Vec<Value> = opts
        .only
        .iter()
        .filter(|id| {
            !found
                .iter()
                .any(|c| c.agent_session.starts_with(id.as_str()))
        })
        .map(|id| {
            let why =
                explain_claude(id, &|i| tracked.contains(i)).unwrap_or_else(|| "unknown".into());
            json!({"id": id, "reason": why})
        })
        .collect();
    if !words.is_empty() {
        found.retain(|c| {
            let hay = [
                Some(c.cwd.as_str()),
                c.branch.as_deref(),
                c.title.as_deref(),
                c.first_prompt.as_deref(),
                c.last_prompt.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
            words.iter().all(|w| hay.contains(w.as_str()))
        });
    }
    found.truncate(limit);
    found.iter_mut().for_each(add_git_info);
    Ok(json!({"count": found.len(), "candidates": found, "rejected": rejected}))
}

/// `session.import`: import one conversation (`agent_session`: its id) as a
/// stopped session. Scanning and recreating a worktree run without the DB
/// lock; only the insert holds it.
pub fn import(d: &Daemon, params: &Value) -> Result<Value> {
    let sid = p_str(params, "agent_session")?.trim().to_string();
    let agent = params["agent"].as_str().unwrap_or("claude");
    if agent != "claude" {
        bail!("importing '{agent}' conversations isn't supported yet (claude only)");
    }
    if sid.len() != 36 {
        bail!("give the full conversation id");
    }
    if let Some(s) = d.db().session_by_agent_session(&sid)? {
        bail!("already in rlord as '{}'", s.name);
    }
    let opts = ScanOpts {
        only: vec![sid.clone()],
        ..Default::default()
    };
    let mut c = scan_claude(&opts, &|_| false)
        .into_iter()
        .next()
        .ok_or_else(|| {
            let why = explain_claude(&sid, &|_| false).unwrap_or_else(|| "unknown".into());
            anyhow::anyhow!("can't import Claude conversation {sid}: {why}")
        })?;
    add_git_info(&mut c);
    let mut recreated = false;
    if !c.dir_exists && c.recreatable && params["recreate_worktree"].as_bool() == Some(true) {
        if let (Some(repo), Some(branch)) = (&c.repo, &c.branch) {
            git::worktree_add(repo, branch, std::path::Path::new(&c.cwd))
                .with_context(|| format!("recreating the worktree at {}", c.cwd))?;
            recreated = true;
        }
    }
    methods::import_candidate(&d.db(), &c, params, recreated)
}
