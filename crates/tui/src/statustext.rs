//! A session's status in words: what each symbol on its row means. Used
//! by the hover popup over a list row and the details panel. Pure, so it
//! is tested without a terminal.

use serde_json::Value;

use crate::theme::status_parts;

/// What each stored status means (its glyph and label are
/// `theme::status_parts`'s, as the list draws them).
const MEANINGS: [(&str, &str); 6] = [
    ("working", "the agent is working"),
    (
        "waiting_input",
        "blocked on a permission prompt or a question",
    ),
    ("idle", "it finished, the next prompt is yours"),
    ("starting", "the agent is starting"),
    ("stopped", "not running (can be resumed)"),
    ("error", "the agent failed"),
];

/// Every status symbol the list shows, and what it means (the help
/// screen's legend).
pub fn legend() -> Vec<(String, &'static str)> {
    let mut v: Vec<(String, &'static str)> = MEANINGS
        .iter()
        .map(|(st, what)| {
            let (glyph, label, _) = status_parts(st);
            (format!("{glyph} {label}"), *what)
        })
        .collect();
    v.insert(
        3,
        (
            "◔ bg N".into(),
            "turn over, N background subagents still running",
        ),
    );
    v.insert(
        4,
        (
            "◷ till …".into(),
            "stopped on a usage limit until then (↻: resumes)",
        ),
    );
    v.push((
        "⇄".into(),
        "Remote Control: green on, dim connecting, red failing",
    ));
    v
}

/// "4m", "2h", "3d": how long ago `at` (ms) was, from `now`.
pub fn ago(at: i64, now: i64) -> String {
    rlord_core::timefmt::rel_age_at(at, now)
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
    if let Some(n) = s["async_tasks"]
        .as_u64()
        .filter(|_| s["shown_status"] == "background")
    {
        let tasks: Vec<&Value> = s["async_work"]
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
            if n as usize > labels.len() {
                out.push_str(&format!(", +{} more", n as usize - labels.len()));
            }
        }
        if let Some(t) = oldest {
            out.push_str(&format!(", started {} ago", ago(t, now)));
        }
        out.push_str(". Its turn is over; it goes on when they finish.");
        return out;
    }
    let st = s["status"].as_str().unwrap_or("");
    let (_, label, _) = status_parts(st);
    match MEANINGS.iter().find(|(k, _)| *k == st) {
        Some((_, what)) => {
            let mut c = label.chars();
            let first: String = c
                .next()
                .map(|f| f.to_uppercase().collect())
                .unwrap_or_default();
            format!("{first}{}: {what}.", c.as_str())
        }
        None => format!("Status: {st}."),
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
    let sh = &s["async_shells"];
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
        if n as usize > items.len() {
            out.push_str(&format!(", +{} more", n as usize - items.len()));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(
            status_sentence(&json!({"status": "stopped"}), 0),
            "Stopped: not running (can be resumed)."
        );
        // The legend uses the list's own glyphs and labels.
        let l = legend();
        assert_eq!(l[0].0, "● working");
        assert!(l.iter().any(|(k, _)| k == "○ your turn"));
    }

    #[test]
    fn background_work_with_labels_and_age() {
        let s = json!({"status": "idle", "shown_status": "background", "async_tasks": 2, "async_work": [
            {"label": "Run tests", "started_at": 60_000},
            {"label": "Review PR", "started_at": 120_000}]});
        assert_eq!(
            status_sentence(&s, 300_000),
            "Waiting on 2 background tasks: Run tests, Review PR, started 4m ago. \
             Its turn is over; it goes on when they finish."
        );
        let s = json!({"status": "idle", "shown_status": "background", "async_tasks": 1});
        assert!(status_sentence(&s, 0).starts_with("Waiting on 1 background task."));
    }

    #[test]
    fn limit_wins_and_remote_is_spelled_out() {
        let s = json!({"status": "idle", "shown_status": "background", "async_tasks": 1,
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
    fn more_than_listed_says_how_many_more() {
        let tasks: Vec<Value> = (0..5)
            .map(|i| json!({"label": format!("t{i}"), "started_at": 0}))
            .collect();
        let s = json!({"status": "idle", "shown_status": "background", "async_tasks": 7,
            "async_work": tasks.clone(), "async_shells": {"count": 6, "tasks": tasks}});
        let d = describe(&s, 0);
        assert!(d[0].contains("t4, +2 more."), "{}", d[0]);
        assert!(d[1].ends_with("t4, +1 more."), "{}", d[1]);
    }

    #[test]
    fn shells_are_information_not_status() {
        let s = json!({"status": "idle", "async_shells": {"count": 2, "tasks": [
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
