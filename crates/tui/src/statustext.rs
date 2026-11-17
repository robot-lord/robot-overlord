//! A session's status in words: what each symbol on its row means. Used
//! by the hover popup over a list row and the details panel. Pure, so it
//! is tested without a terminal.

use serde_json::Value;

/// Every status symbol the list shows, and what it means (the help
/// screen's legend).
pub const LEGEND: &[(&str, &str)] = &[
    ("● working", "the agent is working"),
    (
        "◐ needs you",
        "blocked on a permission prompt or a question",
    ),
    ("○ your turn", "finished: the next prompt is yours"),
    ("◔ bg N", "turn over, N background subagents still running"),
    (
        "◷ till …",
        "stopped on a usage limit until then (↻: resumes)",
    ),
    ("◌ starting", "the agent is starting"),
    ("■ stopped", "not running (can be resumed)"),
    ("✕ error", "the agent failed"),
    ("⇄", "Remote Control: green on, dim connecting, red failing"),
];

/// "4m", "2h", "3d": how long ago `at` (ms) was, from `now`.
pub fn ago(at: i64, now: i64) -> String {
    let s = ((now - at) / 1000).max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// The status itself, as a sentence.
pub fn status_sentence(s: &Value, now: i64) -> String {
    let l = &s["limit"];
    if let Some(until) = l["until"].as_str() {
        let resume = if l["resume"] == true {
            " It resumes by itself then."
        } else {
            ""
        };
        return if l["reset"] == true {
            format!("Usage limit just reset.{resume}")
        } else {
            format!("Stopped on a usage limit until {until}.{resume}")
        };
    }
    if let Some(n) = s["background_tasks"]
        .as_u64()
        .filter(|_| s["shown_status"] == "background")
    {
        let tasks: Vec<&Value> = s["background_work"]
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        let labels: Vec<&str> = tasks.iter().filter_map(|t| t["label"].as_str()).collect();
        let oldest = tasks
            .iter()
            .filter_map(|t| t["started_at"].as_i64())
            .filter(|t| *t > 0)
            .min();
        let mut out = format!(
            "Waiting on {n} background task{}",
            if n == 1 { "" } else { "s" }
        );
        if !labels.is_empty() {
            out.push_str(": ");
            out.push_str(&labels.join(", "));
        }
        if let Some(t) = oldest {
            out.push_str(&format!(", started {} ago", ago(t, now)));
        }
        out.push_str(". Its turn is over; it goes on when they finish.");
        return out;
    }
    match s["status"].as_str().unwrap_or("") {
        "working" => "Working.".into(),
        "waiting_input" => "Needs you: blocked on a permission prompt or a question.".into(),
        "idle" => "Your turn: it finished, the next prompt is yours.".into(),
        "starting" => "Starting.".into(),
        "stopped" => "Stopped: not running.".into(),
        "error" => "Error: the agent failed.".into(),
        other => format!("Status: {other}."),
    }
}

/// The Remote Control mark (⇄) in words, if it shows one.
pub fn remote_sentence(s: &Value) -> Option<String> {
    let r = &s["remote"];
    let why = r["reason"].as_str();
    Some(match r["state"].as_str()? {
        "on" => "Remote Control connected (⇄): reachable from the Claude app.".into(),
        "connecting" => "Remote Control connecting (⇄, dim).".into(),
        "pending" => format!(
            "Remote Control on (⇄, dim): {}.",
            why.unwrap_or("needs a restart")
        ),
        "stopped" => "Remote Control on (⇄, dim): connects when it starts.".into(),
        "disconnecting" => "Remote Control off (⇄, dim): disconnects at its restart.".into(),
        other => format!(
            "Remote Control {other} (⇄, red): {}.",
            why.unwrap_or("unknown reason")
        ),
    })
}

/// Background shells (dev servers, watchers) still running, if any:
/// information, never a status.
pub fn shells_sentence(s: &Value, now: i64) -> Option<String> {
    let sh = &s["background_shells"];
    let n = sh["count"].as_u64().filter(|n| *n > 0)?;
    let tasks: Vec<&Value> = sh["tasks"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let mut out = format!("{n} background shell{}", if n == 1 { "" } else { "s" });
    let items: Vec<String> = tasks
        .iter()
        .filter_map(|t| {
            let label = t["label"].as_str()?;
            Some(match t["started_at"].as_i64().filter(|t| *t > 0) {
                Some(at) => format!("{label} ({} ago)", ago(at, now)),
                None => label.to_string(),
            })
        })
        .collect();
    if !items.is_empty() {
        out.push_str(": ");
        out.push_str(&items.join(", "));
    }
    out.push('.');
    Some(out)
}

/// Everything the row's symbols say, one sentence per line.
pub fn describe(s: &Value, now: i64) -> Vec<String> {
    let mut v = vec![status_sentence(s, now)];
    v.extend(shells_sentence(s, now));
    v.extend(remote_sentence(s));
    v
}

/// Rows `lines` take word-wrapped at `width` columns.
pub fn wrapped_rows(lines: &[String], width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let mut rows = 0;
    for l in lines {
        let mut n = 1;
        let mut col = 0;
        for word in l.split_whitespace() {
            let len = word.chars().count();
            let need = if col == 0 { len } else { col + 1 + len };
            if need <= width || col == 0 {
                col = need;
                // A word longer than the line breaks across rows.
                while col > width {
                    n += 1;
                    col -= width;
                }
            } else {
                n += 1;
                col = len;
                while col > width {
                    n += 1;
                    col -= width;
                }
            }
        }
        rows += n;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wrapping() {
        let l = |s: &str| vec![s.to_string()];
        assert_eq!(wrapped_rows(&l("aaa bbb"), 7), 1);
        assert_eq!(wrapped_rows(&l("aaa bbb"), 6), 2);
        assert_eq!(wrapped_rows(&l("aaaaaaaaaa"), 4), 3);
        assert_eq!(wrapped_rows(&["a".into(), "b".into()], 10), 2);
    }

    #[test]
    fn a_refresh_keeps_the_popup_only_over_its_row() {
        use crate::hover_still_there;
        use crate::mouse::Hit;
        let rows = [json!({"id": "a"}), json!({"id": "b"})];
        assert!(hover_still_there(Some(Hit::Row(1)), &rows, "b"));
        // Reordered: another session under the pointer now.
        assert!(!hover_still_there(Some(Hit::Row(0)), &rows, "b"));
        // Gone from the list, or no row there.
        assert!(!hover_still_there(Some(Hit::Row(1)), &rows[..1], "b"));
        assert!(!hover_still_there(None, &rows, "b"));
    }

    #[test]
    fn details_layout_and_popup_place() {
        use crate::{details_split, hover_rect};
        use ratatui::layout::Rect;
        // The details take most of a tall column; the preview keeps 6+.
        assert_eq!(details_split(40, 12), Some(24));
        assert_eq!(details_split(20, 12), Some(12));
        assert_eq!(details_split(12, 12), Some(6));
        assert_eq!(details_split(9, 12), None);
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(hover_rect(area, 10, 5, 30, 4), Rect::new(12, 6, 30, 4));
        // Near the right and bottom edges: kept inside, above the pointer.
        assert_eq!(hover_rect(area, 70, 22, 30, 4), Rect::new(50, 18, 30, 4));
    }

    #[test]
    fn plain_statuses() {
        let s = json!({"status": "idle"});
        assert_eq!(
            describe(&s, 0),
            ["Your turn: it finished, the next prompt is yours."]
        );
        let s = json!({"status": "waiting_input"});
        assert!(status_sentence(&s, 0).starts_with("Needs you"));
    }

    #[test]
    fn background_work_with_labels_and_age() {
        let s = json!({"status": "idle", "shown_status": "background", "background_tasks": 2, "background_work": [
            {"label": "Run tests", "started_at": 60_000},
            {"label": "Review PR", "started_at": 120_000}]});
        assert_eq!(
            status_sentence(&s, 300_000),
            "Waiting on 2 background tasks: Run tests, Review PR, started 4m ago. \
             Its turn is over; it goes on when they finish."
        );
        let s = json!({"status": "idle", "shown_status": "background", "background_tasks": 1});
        assert!(status_sentence(&s, 0).starts_with("Waiting on 1 background task."));
    }

    #[test]
    fn limit_wins_and_remote_is_spelled_out() {
        let s = json!({"status": "idle", "shown_status": "background", "background_tasks": 1,
            "limit": {"until": "3:00pm", "resume": true},
            "remote": {"state": "on"}});
        assert_eq!(
            describe(&s, 0),
            [
                "Stopped on a usage limit until 3:00pm. It resumes by itself then.",
                "Remote Control connected (⇄): reachable from the Claude app.",
            ]
        );
        let s = json!({"remote": {"state": "failed", "reason": "not logged in"}});
        assert_eq!(
            remote_sentence(&s).unwrap(),
            "Remote Control failed (⇄, red): not logged in."
        );
        assert_eq!(remote_sentence(&json!({})), None);
    }

    #[test]
    fn shells_are_information_not_status() {
        let s = json!({"status": "idle", "background_shells": {"count": 2, "tasks": [
            {"label": "Dev server", "started_at": 0},
            {"label": "Watch tests", "started_at": 60_000}]}});
        assert_eq!(
            describe(&s, 120_000),
            [
                "Your turn: it finished, the next prompt is yours.",
                "2 background shells: Dev server, Watch tests (1m ago).",
            ]
        );
    }

    #[test]
    fn ages() {
        assert_eq!(ago(0, 59_000), "59s");
        assert_eq!(ago(0, 3_600_000), "1h");
        assert_eq!(ago(10, 0), "0s");
    }
}
