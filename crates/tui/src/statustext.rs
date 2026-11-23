//! The list's Status column and what each of its marks means. The list
//! draws its marks from `status_marks`; the hover popup and `rlord show`
//! explain them line by line (`describe`); the details card takes the
//! sentences (`describe_status`, `remote_parts`); the help screen
//! lists them all (`legend`). Pure, so it is tested without a terminal.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::theme;

use crate::theme::status_parts;

/// What each stored status means (its glyph and label are
/// `theme::status_parts`'s, as the list draws them).
const MEANINGS: [(&str, &str); 6] = [
    ("working", "the agent is working"),
    (
        "waiting_input",
        "blocked on a permission prompt or a question",
    ),
    ("idle", "it finished; the next prompt is yours"),
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
            crate::theme::background_short("N"),
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
    v.push(("⚙N".into(), "N sub-agents running"));
    v.push((
        "✉".into(),
        "a message from the overlord waits (✉ draft: held by unsent text)",
    ));
    v.push((
        "✉ stash".into(),
        "waiting for Claude to restore a stashed draft",
    ));
    v.push((
        "⎘".into(),
        "a draft rlord stashed may still be stashed: with the box empty, press Ctrl+S to restore (Ctrl+S with text in the box overwrites the stash), or rlord stash-dismiss",
    ));
    v.push(("⋯".into(), "still loading its sub-agent counts"));
    v
}

use rlord_core::timefmt::rel_age_at;

/// The status itself, after its mark: what it means.
pub fn status_sentence(s: &Value, now: i64) -> String {
    let l = &s["limit"];
    let shown = crate::shown(s);
    if shown == "limited" {
        let until = l["until"].as_str().unwrap_or("?");
        let resume = if l["resume"] == true {
            " It resumes by itself."
        } else {
            ""
        };
        return if l["reset"] == true {
            format!("its usage limit just reset.{resume}")
        } else {
            format!("stopped on a usage limit until {until}.{resume}")
        };
    }
    if shown == "background" {
        let n = s["async_tasks"].as_u64().unwrap_or(0);
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
            "waiting on {n} background task{}",
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
            out.push_str(&format!(", started {} ago", rel_age_at(t, now)));
        }
        out.push_str(". Its turn is over; it goes on when they finish.");
        return out;
    }
    match MEANINGS.iter().find(|(k, _)| *k == shown) {
        Some((_, what)) => format!("{what}."),
        None if matches!(shown, "-" | "") => "rlord doesn't know this session's state yet.".into(),
        None => format!("{shown} (not known to this rlord version)."),
    }
}

/// Each Remote Control state the list draws a ⇄ for: its colour and
/// what it means. None: no mark (off, or a state the list doesn't draw).
pub fn remote_parts(r: &Value) -> Option<(Style, String)> {
    let why = r["reason"].as_str();
    let dim = theme::faint();
    Some(match r["state"].as_str()? {
        "on" => (
            Style::default().fg(theme::green()),
            "Remote Control connected: reachable from the Claude app.".into(),
        ),
        "connecting" => (dim, "Remote Control connecting.".into()),
        "pending" => (
            dim,
            format!(
                "Remote Control on, not connected yet: {}.",
                why.unwrap_or("needs a restart")
            ),
        ),
        "stopped" => (
            dim,
            "Remote Control on: connects when the session starts.".into(),
        ),
        "disconnecting" => (
            dim,
            "Remote Control off: disconnects when the session restarts.".into(),
        ),
        st @ ("failed" | "unavailable") => (
            Style::default().fg(theme::red()),
            format!("Remote Control {st}: {}.", why.unwrap_or("unknown reason")),
        ),
        _ => return None,
    })
}

/// Background shells (dev servers, watchers) still running, if any:
/// information, never a status, and no mark in the list.
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
                Some(at) => format!("{label} ({} ago)", rel_age_at(at, now)),
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

/// One mark from the row's Status column and what it means. `mark` is
/// the column's own text and colour (empty for a line with no mark).
pub struct Item {
    /// The details row it belongs under (`status`, `agents`, `remote`,
    /// `shells`).
    pub label: &'static str,
    pub mark: Span<'static>,
    pub text: String,
    /// The column has room for it (`status_marks`'s width).
    pub shown: bool,
}

impl Item {
    /// `mark — text`, or the text alone.
    pub fn plain(&self) -> String {
        if self.mark.content.is_empty() {
            self.text.clone()
        } else {
            format!("{} — {}", self.mark.content, self.text)
        }
    }

    /// The line, the mark in its column colour.
    pub fn line(&self, text: Style) -> Line<'static> {
        let mut v = Vec::new();
        if !self.mark.content.is_empty() {
            v.push(self.mark.clone());
            v.push(Span::styled(" — ", text));
        }
        v.push(Span::styled(self.text.clone(), text));
        Line::from(v)
    }
}

/// The row's Status column: each mark as the list draws it and what it
/// means. `narrow`: the status as its glyph alone. Only the marks that
/// fit in `width` columns are kept, so the popup explains just what the
/// list shows. The list and the popup both use this, so they can't
/// drift apart. A row with no status keeps its mark with empty text.
pub fn status_marks(s: &Value, narrow: bool, width: usize, now: i64) -> Vec<Item> {
    let chip = crate::state_chip(s, false);
    let shown = crate::shown(s);
    let mut text = status_sentence(s, now);
    let mark = if narrow {
        let mut c = chip.content.chars();
        let glyph: String = c.next().map(String::from).unwrap_or_default();
        // The label the glyph stands for, unless the sentence already
        // names it (a status this version doesn't know).
        let known_label = status_parts(shown).1 != "unknown" || shown == "background";
        if shown == "limited" {
            text = narrow_limit(&s["limit"]);
        } else if known_label {
            text = format!("{}: {text}", c.as_str().trim());
        }
        Span::styled(glyph, chip.style)
    } else {
        chip
    };
    let mut v = vec![Item {
        label: "status",
        mark,
        text,
        shown: true,
    }];
    // Its sub-agents are still loading (warm-up): the status is known,
    // the agent counts aren't yet.
    if s["loading"] == true {
        v.push(Item {
            label: "agents",
            mark: Span::styled("⋯", theme::faint()),
            text: "still loading its sub-agent counts.".into(),
            shown: true,
        });
    }
    if let Some((style, text)) = remote_parts(&s["remote"]) {
        v.push(Item {
            label: "remote",
            mark: Span::styled("⇄", style),
            text,
            shown: true,
        });
    }
    let n = s["agents_running"].as_i64().unwrap_or(0);
    if n != 0 {
        v.push(Item {
            label: "running",
            mark: Span::styled(format!("⚙{n}"), Style::default().fg(theme::overlord())),
            text: format!("{n} sub-agent{} running.", if n == 1 { "" } else { "s" }),
            shown: true,
        });
    }
    v.extend(mail_marks(s));
    // The status always shows (clipped if it must); the rest only whole.
    let mut used = v[0].mark.width();
    for m in &mut v[1..] {
        used += 1 + m.mark.width();
        m.shown = used <= width;
    }
    v
}

/// The overlord's messages waiting for the session (`mail`: `✉`, or `✉
/// draft` when a draft in its input holds them) and a draft rlord
/// stashed that may still be in the stash (`draft_stash`: "stale").
fn mail_marks(s: &Value) -> Vec<Item> {
    let mut v = Vec::new();
    let m = &s["mail"];
    if m.is_object() {
        let (mark, text) = match m["held"].as_str() {
            Some(why) if m["kind"] == "stash" => (
                "✉ stash",
                format!("waiting for Claude to restore a stashed draft: {why}."),
            ),
            Some(why) if m["draft"] == true => ("✉ draft", format!("a message is held: {why}.")),
            Some(why) => ("✉", format!("a message is held: {why}.")),
            None if m["first"] == true => ("✉", "its first prompt waits to go in.".to_string()),
            None => ("✉", "a message waits for its turn to end.".to_string()),
        };
        v.push(Item {
            label: "mail",
            mark: Span::styled(mark, theme::faint()),
            text,
            shown: true,
        });
    }
    if s["draft_stash"] == "stale" {
        v.push(Item {
            label: "stash",
            mark: Span::styled("⎘", Style::default().fg(theme::red())),
            text: "draft may still be stashed: with the box empty, press Ctrl+S to restore (Ctrl+S with text in the box overwrites the stash), or `rlord stash-dismiss`. Messages to it wait until then.".into(),
            shown: true,
        });
    }
    v
}

/// A limited row in a narrow list (its `◷` alone): the limit in words,
/// time once, and what `↻` means.
fn narrow_limit(l: &Value) -> String {
    let resume = if l["resume"] == true {
        ", resumes by itself (↻)"
    } else {
        ""
    };
    if l["reset"] == true {
        format!("usage limit just reset{resume}.")
    } else {
        let until = l["until"].as_str().unwrap_or("?");
        format!("usage limit until {until}{resume}.")
    }
}

/// The Status column's spans, space-separated, in `width` columns.
pub fn column_spans(s: &Value, narrow: bool, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let marks = status_marks(s, narrow, width, 0);
    for (i, m) in marks.into_iter().filter(|m| m.shown).enumerate() {
        if i > 0 {
            out.push(Span::raw(" "));
        }
        out.push(m.mark);
    }
    out
}

/// Every mark in the row's Status column, each with what it means: the
/// ones the list shows in its order, then those it has no room for
/// (said so), then background shells, which have no mark. Empty when
/// nothing is known.
pub fn describe(s: &Value, narrow: bool, width: usize, now: i64) -> Vec<Item> {
    let (shown, mut cut): (Vec<Item>, Vec<Item>) = status_marks(s, narrow, width, now)
        .into_iter()
        .partition(|m| m.shown);
    for m in &mut cut {
        let t = m.text.strip_suffix('.').unwrap_or(&m.text).to_string();
        m.text = format!("{t} (not shown in this width).");
    }
    let mut v = shown;
    v.extend(cut);
    v.extend(shells_line(s, now));
    v
}

/// The details card's status row: the status in words and any shells,
/// without marks (the headline shows the status chip; its own `remote`
/// and `agents` rows cover the rest).
pub fn describe_status(s: &Value, now: i64) -> Vec<(&'static str, String)> {
    let mut v = Vec::new();
    v.push(("status", status_sentence(s, now)));
    v.extend(shells_sentence(s, now).map(|t| ("shells", t)));
    v
}

fn shells_line(s: &Value, now: i64) -> Option<Item> {
    shells_sentence(s, now).map(|text| Item {
        label: "shells",
        mark: Span::raw(""),
        text,
        shown: true,
    })
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

    fn texts(s: &Value, now: i64) -> Vec<String> {
        describe(s, false, usize::MAX, now)
            .iter()
            .map(Item::plain)
            .collect()
    }

    #[test]
    fn plain_statuses() {
        assert_eq!(
            texts(&json!({"status": "idle"}), 0),
            ["○ your turn — it finished; the next prompt is yours."]
        );
        assert!(texts(&json!({"status": "waiting_input"}), 0)[0].starts_with("◐ needs you — "));
        assert_eq!(
            texts(&json!({"status": "stopped"}), 0),
            ["■ stopped — not running (can be resumed)."]
        );
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
            texts(&s, 300_000),
            [
                "◔ bg 2 — waiting on 2 background tasks: Run tests, Review PR, started 4m ago. \
             Its turn is over; it goes on when they finish."
            ]
        );
        let s = json!({"status": "idle", "shown_status": "background", "async_tasks": 1});
        assert!(status_sentence(&s, 0).starts_with("waiting on 1 background task."));
    }

    #[test]
    fn every_mark_in_column_order_with_its_colour() {
        let s = json!({"status": "idle", "shown_status": "limited", "async_tasks": 1,
            "limit": {"until": "3:00pm", "resume": true}, "loading": true,
            "remote": {"state": "on"}, "agents_running": 1});
        let d = describe(&s, false, usize::MAX, 0);
        assert_eq!(
            d.iter().map(Item::plain).collect::<Vec<_>>(),
            [
                "◷ 3:00pm ↻ — stopped on a usage limit until 3:00pm. It resumes by itself.",
                "⋯ — still loading its sub-agent counts.",
                "⇄ — Remote Control connected: reachable from the Claude app.",
                "⚙1 — 1 sub-agent running.",
            ]
        );
        // The list's colours: a limit yellow, connected green. (The
        // palette is pinned: other tests switch it.)
        let _pin = theme::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = describe(&s, false, usize::MAX, 0);
        assert_eq!(d[0].mark.style.fg, Some(theme::yellow()));
        assert_eq!(d[1].mark.style, theme::faint());
        assert_eq!(d[2].mark.style.fg, Some(theme::green()));
        assert_eq!(d[3].mark.style.fg, Some(theme::overlord()));
        // The column draws exactly these marks, in this order.
        let col: Vec<String> = column_spans(&s, false, usize::MAX)
            .iter()
            .map(|sp| sp.content.to_string())
            .collect();
        assert_eq!(col, ["◷ 3:00pm ↻", " ", "⋯", " ", "⇄", " ", "⚙1"]);
        // The details card keeps only the status (and shells).
        assert_eq!(
            describe_status(&s, 0),
            [(
                "status",
                "stopped on a usage limit until 3:00pm. It resumes by itself.".to_string()
            )]
        );
        // Marks the column has no room for come last, said so.
        let d = describe(&s, false, 13, 0);
        assert_eq!(
            d.iter().map(Item::plain).collect::<Vec<_>>()[2..],
            [
                "⇄ — Remote Control connected: reachable from the Claude app \
                 (not shown in this width).",
                "⚙1 — 1 sub-agent running (not shown in this width).",
            ],
            "◷ 3:00pm ↻ ⋯ fits 13; ⇄ and ⚙1 don't"
        );
        assert_eq!(
            column_spans(&s, false, 13).len(),
            3,
            "the column drops the same marks"
        );
        let d = describe(&s, true, 1, 0);
        assert_eq!(d.len(), 4);
        assert_eq!(
            d[0].plain(),
            "◷ — usage limit until 3:00pm, resumes by itself (↻)."
        );
        assert!(d[1].plain().ends_with("(not shown in this width)."));
        // No status known: the column's mark is still explained.
        assert_eq!(
            describe(&json!({}), false, 13, 0)
                .iter()
                .map(Item::plain)
                .collect::<Vec<_>>(),
            ["· unknown — rlord doesn't know this session's state yet."]
        );
        let s = json!({"status": "idle", "shown_status": "limited",
            "limit": {"until": "3pm", "reset": true}});
        assert_eq!(texts(&s, 0), ["◷ reset — its usage limit just reset."]);
    }

    #[test]
    fn narrow_shows_the_glyph_and_spells_the_label() {
        let s = json!({"status": "idle", "shown_status": "background", "async_tasks": 1});
        let d = describe(&s, true, 1, 0);
        assert_eq!(d[0].mark.content, "◔");
        assert!(d[0]
            .plain()
            .starts_with("◔ — bg 1: waiting on 1 background task."));
        assert_eq!(column_spans(&s, true, 1)[0].content, "◔");
        assert_eq!(
            texts_narrow(&json!({"status": "idle"})),
            ["○ — your turn: it finished; the next prompt is yours."]
        );
    }

    fn texts_narrow(s: &Value) -> Vec<String> {
        describe(s, true, 1, 0).iter().map(Item::plain).collect()
    }

    #[test]
    fn unknown_status_names_itself() {
        let s = json!({"status": "paused"});
        assert_eq!(
            texts(&s, 0),
            ["· paused — paused (not known to this rlord version)."]
        );
        assert_eq!(
            texts_narrow(&s),
            ["· — paused (not known to this rlord version)."]
        );
    }

    #[test]
    fn remote_states() {
        let _pin = theme::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let s = json!({"remote": {"state": "failed", "reason": "not logged in"}});
        assert_eq!(
            remote_parts(&s["remote"]).unwrap(),
            (
                Style::default().fg(theme::red()),
                "Remote Control failed: not logged in.".to_string()
            )
        );
        assert_eq!(remote_parts(&json!(null)), None);
        // A state the list draws no mark for gets no line.
        assert_eq!(remote_parts(&json!({"state": "off"})), None);
    }

    #[test]
    fn more_than_listed_says_how_many_more() {
        let tasks: Vec<Value> = (0..5)
            .map(|i| json!({"label": format!("t{i}"), "started_at": 0}))
            .collect();
        let s = json!({"status": "idle", "shown_status": "background", "async_tasks": 7,
            "async_work": tasks.clone(), "async_shells": {"count": 6, "tasks": tasks}});
        let d = texts(&s, 0);
        assert!(d[0].contains("t4, +2 more."), "{}", d[0]);
        assert!(d[1].ends_with("t4, +1 more."), "{}", d[1]);
    }

    #[test]
    fn shells_are_information_not_status() {
        let s = json!({"status": "idle", "async_shells": {"count": 2, "tasks": [
            {"label": "Dev server", "started_at": 0},
            {"label": "Watch tests", "started_at": 60_000}]}});
        assert_eq!(
            texts(&s, 120_000),
            [
                "○ your turn — it finished; the next prompt is yours.",
                "2 background shells: Dev server, Watch tests (1m ago).",
            ]
        );
    }

    /// Waiting messages show `✉` (`✉ draft` when a draft holds them) and
    /// a stash rlord couldn't put back shows `⎘`.
    #[test]
    fn mail_and_stash_marks() {
        let marks = |s: &Value| -> Vec<String> {
            status_marks(s, false, 80, 0)
                .iter()
                .skip(1)
                .map(Item::plain)
                .collect()
        };
        assert!(marks(&json!({"status": "idle"})).is_empty());
        let waiting = json!({"status": "working", "mail": {"held": null, "draft": false}});
        assert_eq!(
            marks(&waiting),
            ["✉ — a message waits for its turn to end."]
        );
        let draft = json!({"status": "idle",
            "mail": {"held": "its input holds unsent text (a draft)", "draft": true}});
        assert!(marks(&draft)[0].starts_with("✉ draft — a message is held: its input"));
        let failed = json!({"status": "idle", "draft_stash": "stale"});
        assert!(marks(&failed)[0].starts_with("⎘ — draft may still be stashed"));
        let first = json!({"status": "idle", "mail": {"held": null, "first": true}});
        assert_eq!(marks(&first), ["✉ — its first prompt waits to go in."]);
        let stash =
            json!({"status": "idle", "mail": {"held": "x", "draft": true, "kind": "stash"}});
        assert!(
            marks(&stash)[0].starts_with("✉ stash — waiting for Claude to restore a stashed draft")
        );
        assert!(marks(&failed)[0].contains("with the box empty, press Ctrl+S"));
        assert!(marks(&json!({"status": "idle", "draft_stash": "pending"})).is_empty());
    }
}
