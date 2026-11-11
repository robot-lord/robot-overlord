//! `session.import_scan`: agent conversations on disk that rlord doesn't
//! track yet (see `rlord_core::import`). Plain code, no AI: the overlord's
//! `rlord-import` subagent decides which are worth importing.

use anyhow::Result;
use serde_json::{json, Value};

use rlord_core::import::{scan_claude, ScanOpts};
use rlord_core::types::now_ms;

use crate::methods::str_list;
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
    let opts = ScanOpts {
        since_ms: now_ms() - window_ms,
        // Filter by query before limiting, so the limit counts matches.
        limit: if words.is_empty() { limit } else { 0 },
        only: str_list(params, "ids"),
    };
    let mut found = scan_claude(&opts, &|id| tracked.contains(id));
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
        found.truncate(limit);
    }
    Ok(json!({"count": found.len(), "candidates": found}))
}
