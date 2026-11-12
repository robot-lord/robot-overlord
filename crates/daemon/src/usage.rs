//! Usage on session rows (the list's Usage column, the details pane, the
//! overlord's `list_sessions`), and the usage sort.
//!
//! Each row gets `usage` (turns, tokens, context, window, context_pct; null
//! while unknown) from `rlord_core::usage::of`, read here without the DB
//! lock. A transcript rlord hasn't read yet, or one far behind, is left to
//! the warm-up worker (the row shows as loading) so a list never stalls on
//! a first full read of a large file.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{json, Value};

use rlord_core::db::SortMode;
use rlord_core::usage::{self, Usage, UsageMetric};

use crate::{methods, Daemon};

/// More than this left to read: the warm-up reads it, not the list.
const READ_INLINE_MAX: u64 = 4 << 20;

/// The metric a request asks for (`usage`), else the configured one.
pub fn metric(d: &Daemon, params: &Value) -> UsageMetric {
    params["usage"]
        .as_str()
        .and_then(UsageMetric::parse)
        .unwrap_or(d.config().usage)
}

/// Fill in each row's `usage`. Rows still `loading` (the warm-up) are
/// left null. `inline`: read a transcript here even if rlord hasn't yet
/// (one session's details), rather than queue it.
pub fn annotate(d: &Daemon, rows: &mut [Value], loading: &HashSet<String>, inline: bool) {
    if rows.is_empty() {
        return;
    }
    let sources: HashMap<String, String> = d
        .db()
        .transcript_sources()
        .unwrap_or_default()
        .into_iter()
        .collect();
    for row in rows.iter_mut() {
        let id = row["id"].as_str().unwrap_or("").to_string();
        row["usage"] = Value::Null;
        if loading.contains(&id) {
            continue;
        }
        let Some(src) = sources.get(&id) else {
            continue;
        };
        let path = Path::new(src);
        // (No such file: nothing to read, nothing to queue.)
        let unread = usage_unread(path);
        let Some(unread) = unread else { continue };
        if !inline && unread.is_none_or(|n| n > READ_INLINE_MAX) && d.warm.enqueue_usage(&id) {
            row["loading"] = json!(true);
            continue;
        }
        if let Some(u) = usage::of(path, row["model"].as_str()) {
            row["usage"] = u.to_json();
        }
    }
    methods::save_cursors(&d.db());
}

/// Some(None): the transcript exists but rlord hasn't read it; Some(Some(n)):
/// n bytes appended since it last did; None: there's no transcript.
fn usage_unread(path: &Path) -> Option<Option<u64>> {
    use rlord_core::agents::transcript;
    match transcript::unread_bytes(path) {
        Some(n) => Some(Some(n)),
        None if path.is_file() => Some(None),
        None => None,
    }
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
pub fn list(d: &Daemon, params: &Value, rows: &mut [Value], loading: &HashSet<String>) {
    annotate(d, rows, loading, false);
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
