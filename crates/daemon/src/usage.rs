//! Usage on session rows (the list's Usage column, the details pane, the
//! overlord's `list_sessions`), and the usage sort.
//!
//! Each row gets `usage` (turns, tokens, context, window, context_pct; null
//! while unknown) from `rlord_core::usage`, read here without the DB lock.
//! A request never reads a whole large transcript: one rlord hasn't read
//! (that it has no saved position for), or one far behind, is left to the
//! warm-up worker, and the row says `usage_loading` meanwhile.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use rlord_core::agents::opencode;
use rlord_core::agents::transcript::{self, CursorState};
use rlord_core::db::SortMode;
use rlord_core::usage::{self, Usage, UsageMetric};

use crate::{methods, Daemon};

/// More than this left to read: the warm-up reads it, not the request.
const READ_INLINE_MAX: u64 = 4 << 20;

/// The metric a request asks for (`usage`), else the one the user last
/// picked (`ui::usage_pref`), else the configured one.
pub fn metric(d: &Daemon, params: &Value) -> UsageMetric {
    params["usage"]
        .as_str()
        .and_then(UsageMetric::parse)
        .or_else(|| crate::ui::usage_pref(d))
        .unwrap_or(d.config().usage)
}

/// Fill in each row's `usage` (null while unknown; `usage_loading` while
/// the warm-up reads it). Reads here are bounded: what was appended since
/// the last read, at most `READ_INLINE_MAX`. `single`: one session's
/// details, which may also read a transcript rlord hasn't read yet if it's
/// no bigger than that.
pub fn annotate(d: &Daemon, rows: &mut [Value], single: bool) {
    if rows.is_empty() {
        return;
    }
    let ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
    let sources: HashMap<String, PathBuf> = d
        .db()
        .transcript_sources_for(&ids)
        .unwrap_or_default()
        .into_iter()
        .map(|(id, p)| (id, PathBuf::from(p)))
        .collect();
    // Transcripts whose read position fell out of memory (or wasn't loaded
    // at start): the saved one, so only what's new is read. Each is put
    // back just before its row is read: all at once could push past the
    // cap and evict the rows not yet reached.
    let mut saved = saved_positions(d, sources.values().map(PathBuf::as_path));
    let opencode = opencode_usage(rows, &sources);
    let full_loading = d.warm.loading();
    for row in rows.iter_mut() {
        let id = row["id"].as_str().unwrap_or("").to_string();
        row["usage"] = Value::Null;
        let Some(path) = sources.get(&id) else {
            continue;
        };
        if opencode::locate(path).is_some() {
            if let Some(u) = opencode.get(&id) {
                row["usage"] = u.to_json();
            }
            continue;
        }
        let Ok(meta) = std::fs::metadata(path) else {
            // (No such file: nothing to read, nothing to queue.)
            continue;
        };
        restore(&mut saved, path);
        let cheap = match transcript::unread_bytes(path) {
            Some(n) => n <= READ_INLINE_MAX,
            // Never read: all of it.
            None => single && meta.len() <= READ_INLINE_MAX,
        };
        if cheap {
            if let Some(u) = usage::of(path, row["model"].as_str()) {
                row["usage"] = u.to_json();
            }
        } else if full_loading.contains(&id) || d.warm.enqueue_usage(&id) {
            // (A full load reads its usage too.)
            row["usage_loading"] = json!(true);
        }
    }
    methods::save_cursors(&d.db());
}

/// Saved read positions (DB) of the JSONL transcripts among `paths` that
/// have none in memory.
pub(crate) fn saved_positions<'a>(
    d: &Daemon,
    paths: impl Iterator<Item = &'a Path>,
) -> HashMap<PathBuf, CursorState> {
    let forgotten: Vec<&str> = paths
        .filter(|p| opencode::locate(p).is_none() && transcript::unread_bytes(p).is_none())
        .filter_map(|p| p.to_str())
        .collect();
    if forgotten.is_empty() {
        return HashMap::new();
    }
    d.db()
        .transcript_cursors_for(&forgotten)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// Put `path`'s saved position (from `saved_positions`) back in memory, if
/// it has none there, right before reading it.
pub(crate) fn restore(saved: &mut HashMap<PathBuf, CursorState>, path: &Path) {
    if transcript::unread_bytes(path).is_none() {
        if let Some(c) = saved.remove(path) {
            transcript::seed_cursors(vec![(path.to_path_buf(), c)]);
        }
    }
}

/// `path`'s saved position back in memory (if it has none there), so a
/// read goes forward from it rather than from the start.
pub(crate) fn recall(d: &Daemon, path: &Path) {
    let mut saved = saved_positions(d, std::iter::once(path));
    restore(&mut saved, path);
}

/// Bring a session's usage up to date on a request path: read what's new
/// if that's at most `READ_INLINE_MAX`; otherwise (never read, far behind)
/// queue it on the warm-up. Nothing if the warm-up already has it queued.
pub(crate) fn refresh_bounded(d: &Daemon, id: &str, path: &Path, model: Option<&str>) {
    if d.warm.usage_queued(id) {
        return;
    }
    recall(d, path);
    match transcript::unread_bytes(path) {
        _ if opencode::locate(path).is_some() => {}
        Some(n) if n <= READ_INLINE_MAX => {
            usage::of(path, model);
        }
        _ if path.is_file() => {
            d.warm.enqueue_usage(id);
        }
        _ => {}
    }
}

/// The OpenCode rows' usage: one connection per database.
fn opencode_usage(rows: &[Value], sources: &HashMap<String, PathBuf>) -> HashMap<String, Usage> {
    // Per database: (row id, (conversation id, configured model)).
    type Rows<'a> = Vec<(&'a str, (&'a str, Option<&'a str>))>;
    let mut by_db: HashMap<&Path, Rows> = HashMap::new();
    for row in rows {
        let Some(id) = row["id"].as_str() else {
            continue;
        };
        let Some((db, conv)) = sources.get(id).and_then(|p| opencode::locate(p)) else {
            continue;
        };
        by_db
            .entry(db)
            .or_default()
            .push((id, (conv, row["model"].as_str())));
    }
    let mut out = HashMap::new();
    for (db, items) in by_db {
        let convs: Vec<(&str, Option<&str>)> = items.iter().map(|(_, c)| *c).collect();
        let got = usage::of_opencode(db, &convs);
        for (id, (conv, _)) in items {
            if let Some(u) = got.get(conv) {
                out.insert(id.to_string(), u.clone());
            }
        }
    }
    out
}

/// Order rows by usage (`metric`), most first (`reverse`: least first);
/// pinned rows stay on top, and rows whose usage is unknown go last. Stable:
/// equals keep the order they came in.
pub fn sort(rows: &mut [Value], metric: UsageMetric, reverse: bool) {
    let key = |r: &Value| Usage::from_json(&r["usage"]).and_then(|u| u.value(metric));
    rows.sort_by(|a, b| {
        let pinned = |r: &Value| r["pinned"].as_bool().unwrap_or(false);
        pinned(b)
            .cmp(&pinned(a))
            .then_with(|| match (key(a), key(b)) {
                (Some(x), Some(y)) if reverse => x.cmp(&y),
                (Some(x), Some(y)) => y.cmp(&x),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
    });
}

/// `session.list`'s rows: usage on each, sorted by it when asked.
pub fn list(d: &Daemon, params: &Value, rows: &mut [Value]) {
    annotate(d, rows, false);
    let sort = params["sort"].as_str().and_then(SortMode::parse);
    if sort == Some(SortMode::Usage) {
        let reverse = params["reverse"].as_bool().unwrap_or(false);
        self::sort(rows, metric(d, params), reverse);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_sort_puts_pinned_then_most_then_unknown() {
        let row = |id: &str, pinned: bool, turns: Option<u64>| {
            json!({"id": id, "pinned": pinned,
                   "usage": turns.map(|t| Usage { turns: t, ..Default::default() }.to_json())})
        };
        let mut rows = vec![
            row("a", false, Some(3)),
            row("b", false, None),
            row("c", true, Some(1)),
            row("d", false, Some(9)),
            row("e", false, Some(3)),
        ];
        let ids = |rows: &[Value]| -> Vec<String> {
            rows.iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect()
        };
        sort(&mut rows, UsageMetric::Turns, false);
        assert_eq!(ids(&rows), ["c", "d", "a", "e", "b"]);
        sort(&mut rows, UsageMetric::Turns, true);
        assert_eq!(ids(&rows), ["c", "a", "e", "d", "b"]);
        // Context % with no window known: unknown, last.
        sort(&mut rows, UsageMetric::Context, false);
        assert_eq!(ids(&rows)[0], "c");
    }
}
