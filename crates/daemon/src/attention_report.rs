//! `rlord attention report` (`attention.report`): the attention queue's
//! shadow run, read from rlord's database (`attention_*` session events,
//! written by `attention.rs`), against the go/no-go numbers for turning
//! announcements on (`docs/working/attention-queue.md`, "Measuring before
//! widening"). Read-only; the report itself is pure and unit-tested.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use serde_json::{json, Value};

use rlord_core::types::now_ms;

use crate::Daemon;

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;
/// A your-turn announcement whose entry cleared by itself this soon after
/// it began (its 10 s settle plus 30 s) was a flap.
const FLAP_MS: i64 = 40_000;
/// An entry still open this long after its session's pane was found dead
/// was never cleared.
const STALE_MS: i64 = 12 * HOUR;

/// The time the report covers: the window asked for, or since the shadow
/// run's first event when that is shorter.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Span {
    pub days: f64,
    /// The shadow run began inside the window.
    pub since_start: bool,
}

impl Span {
    pub fn of(window_ms: i64, first: Option<i64>, now: i64) -> Span {
        match first {
            None => Span::default(),
            Some(f) if now - f < window_ms => Span {
                days: (now - f).max(HOUR) as f64 / DAY as f64,
                since_start: true,
            },
            Some(_) => Span {
                days: window_ms as f64 / DAY as f64,
                since_start: false,
            },
        }
    }
}

/// One shadow event.
#[derive(Debug, Clone)]
pub struct Ev {
    pub session: String,
    pub ts: i64,
    pub kind: String,
    pub payload: Value,
}

/// When each session's pane was found dead (ms).
pub type Deaths = HashMap<String, Vec<i64>>;

/// `attention.report`: the report over the last `days` (default 7; 1 to
/// 90), as text.
pub fn rpc(d: &Daemon, params: &Value) -> Result<Value> {
    let days = params["days"].as_i64().unwrap_or(7).clamp(1, 90);
    let now = now_ms();
    let from = now - days * DAY;
    let (events, deaths, first) = {
        let db = d.db();
        let events: Vec<Ev> = db
            .attention_events(from)?
            .into_iter()
            .map(|(session, ts, kind, payload)| Ev {
                session,
                ts,
                kind,
                payload,
            })
            .collect();
        // Deaths from when the oldest entry in the window began (it may
        // have begun before the window).
        let oldest = events
            .iter()
            .filter_map(|e| match e.kind.as_str() {
                "attention_entered" => e.payload["since"].as_i64(),
                "attention_cleared" => e.payload["waited_ms"].as_i64().map(|w| e.ts - w),
                _ => None,
            })
            .min()
            .unwrap_or(from)
            .min(from);
        (events, db.pane_deaths(oldest)?, db.first_attention_event()?)
    };
    let span = Span::of(days * DAY, first, now);
    Ok(json!({"text": render(&report(&events, &deaths, now, span))}))
}

/// Counts for one kind.
#[derive(Debug, Default, PartialEq)]
pub struct KindStats {
    pub entered: usize,
    pub announced: usize,
    /// Not announced because the session was being looked at.
    pub withheld: usize,
    /// Not announced: looked at already before its settle time.
    pub seen_before: usize,
    /// Not announced: its turn was announced once already.
    pub repeats: usize,
    /// Cleared, by what (`by`).
    pub cleared: BTreeMap<String, usize>,
    /// Announced, then cleared by the agent itself (`self`); for your
    /// turn, only within `FLAP_MS`.
    pub announced_then_self: usize,
    /// Waits ended by the user (answer or prompt), ms, sorted.
    pub answered_ms: Vec<i64>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub span: Span,
    pub needs_you: KindStats,
    pub your_turn: KindStats,
    /// Entries still open `STALE_MS` after their session's pane was found
    /// dead (`dead_at` on the clear; or still open, with a pane death
    /// since they began). Not an agent that ended itself, nor a session
    /// the user stopped or archived.
    pub stale_dead: usize,
}

/// The report over `events` (oldest first); `deaths`: when each
/// session's pane was found dead.
pub fn report(events: &[Ev], deaths: &Deaths, now: i64, span: Span) -> Report {
    let mut r = Report {
        span,
        ..Default::default()
    };
    // Entries still open: (session, kind) -> since.
    let mut open: HashMap<(String, String), i64> = HashMap::new();
    for e in events {
        let kind = e.payload["kind"].as_str().unwrap_or("");
        let k = match kind {
            "needs_you" => &mut r.needs_you,
            "your_turn" => &mut r.your_turn,
            _ => continue,
        };
        let key = (e.session.clone(), kind.to_string());
        match e.kind.as_str() {
            "attention_entered" => {
                k.entered += 1;
                open.insert(key, e.payload["since"].as_i64().unwrap_or(e.ts));
            }
            "attention_settled" => {
                if e.payload["announced"] == true {
                    k.announced += 1;
                } else if e.payload["viewed"] == true {
                    k.withheld += 1;
                } else if e.payload["repeat"] == true {
                    k.repeats += 1;
                } else {
                    k.seen_before += 1;
                }
            }
            "attention_cleared" => {
                open.remove(&key);
                let by = e.payload["by"].as_str().unwrap_or("?").to_string();
                let waited = e.payload["waited_ms"].as_i64().unwrap_or(0);
                if e.payload["announced"] == true
                    && by == "self"
                    && (kind == "needs_you" || waited <= FLAP_MS)
                {
                    k.announced_then_self += 1;
                }
                if matches!(by.as_str(), "answer" | "prompt") && e.payload["carried"] != true {
                    k.answered_ms.push(waited);
                }
                let dead_for = e.payload["dead_at"].as_i64().map(|d| e.ts - d);
                if by == "ended" && dead_for.is_some_and(|t| t > STALE_MS) {
                    r.stale_dead += 1;
                }
                *k.cleared.entry(by).or_default() += 1;
            }
            _ => {}
        }
    }
    r.stale_dead += open
        .iter()
        .filter(|((s, _), since)| {
            deaths
                .get(s)
                .and_then(|d| d.iter().find(|t| **t >= **since))
                .is_some_and(|dead| now - dead > STALE_MS)
        })
        .count();
    r.needs_you.answered_ms.sort_unstable();
    r.your_turn.answered_ms.sort_unstable();
    r
}

fn pct(n: usize, of: usize) -> String {
    if of == 0 {
        "-".into()
    } else {
        format!("{:.1}%", 100.0 * n as f64 / of as f64)
    }
}

fn quantile(sorted: &[i64], q: f64) -> String {
    if sorted.is_empty() {
        return "-".into();
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    let s = sorted[i] / 1000;
    if s < 120 {
        format!("{s}s")
    } else {
        format!("{}m", s / 60)
    }
}

pub fn render(r: &Report) -> String {
    let days = r.span.days;
    let mut out = if r.span.since_start {
        format!("attention shadow run: since it began, {days:.1} days ago\n\n")
    } else {
        format!("attention shadow run: the last {days:.0} days\n\n")
    };
    for (name, k) in [("needs you", &r.needs_you), ("your turn", &r.your_turn)] {
        let per_day = if days > 0.0 {
            format!("{:.1}/day", k.announced as f64 / days)
        } else {
            "-".into()
        };
        let by: Vec<String> = k.cleared.iter().map(|(b, n)| format!("{b} {n}")).collect();
        out.push_str(&format!(
            "{name}: {} entered, {} would announce ({per_day}); not announced: {} looked at, {} seen before, {} repeats\n  cleared by: {}\n  answered after: median {}, p90 {}\n",
            k.entered,
            k.announced,
            k.withheld,
            k.seen_before,
            k.repeats,
            if by.is_empty() { "-".into() } else { by.join(", ") },
            quantile(&k.answered_ms, 0.5),
            quantile(&k.answered_ms, 0.9),
        ));
    }
    let check = |ok: bool| if ok { "ok  " } else { "FAIL" };
    let ny = &r.needs_you;
    let yt = &r.your_turn;
    let flaps = pct(yt.announced_then_self, yt.announced);
    let flap_ok = yt.announced == 0 || (yt.announced_then_self as f64) < 0.02 * yt.announced as f64;
    out.push_str("\ngo/no-go (a week of use or more):\n");
    out.push_str(&format!(
        "  {} needs-you announcements cleared by the agent itself: {} (want 0)\n",
        check(ny.announced_then_self == 0),
        ny.announced_then_self
    ));
    out.push_str(&format!(
        "  {} your-turn announcements cleared by the agent within 30 s: {} of {} ({flaps}; want under 2%)\n",
        check(flap_ok),
        yt.announced_then_self,
        yt.announced
    ));
    out.push_str(&format!(
        "  {} entries still open 12 h after their pane was found dead: {} (want 0)\n",
        check(r.stale_dead == 0),
        r.stale_dead
    ));
    out.push_str("  look  announcements per day, by kind: above, with the user\n");
    out.push_str(KNOWN_LIMITS);
    out
}

/// What the shadow numbers can't see yet: the reader discounts them.
const KNOWN_LIMITS: &str = "\nknown limits (discount these):\n\
  - an overlay (background work, a limit) that ended while the daemon was down has no recorded end\n\
  - attention list reads overlay ends as of its call, the watcher as of its last look\n\
  - a background end is recorded by the background scan (every 5 s), only after two up-to-date reads\n";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(session: &str, ts: i64, kind: &str, payload: Value) -> Ev {
        Ev {
            session: session.into(),
            ts,
            kind: format!("attention_{kind}"),
            payload,
        }
    }

    #[test]
    fn the_report_counts_what_would_have_happened() {
        let s = 1_000;
        let evs = vec![
            // A question answered after 80 s.
            ev("a", s, "entered", json!({"kind": "needs_you", "since": s})),
            ev(
                "a",
                s + 3_000,
                "settled",
                json!({"kind": "needs_you", "announced": true, "viewed": false}),
            ),
            ev(
                "a",
                s + 80_000,
                "cleared",
                json!({"kind": "needs_you", "by": "answer", "waited_ms": 80_000, "announced": true}),
            ),
            // A turn that carried on by itself 15 s in: a flap.
            ev("b", s, "entered", json!({"kind": "your_turn", "since": s})),
            ev(
                "b",
                s + 10_000,
                "settled",
                json!({"kind": "your_turn", "announced": true, "viewed": false}),
            ),
            ev(
                "b",
                s + 15_000,
                "cleared",
                json!({"kind": "your_turn", "by": "self", "waited_ms": 15_000, "announced": true}),
            ),
            // One the user was looking at: withheld.
            ev("c", s, "entered", json!({"kind": "your_turn", "since": s})),
            ev(
                "c",
                s + 10_000,
                "settled",
                json!({"kind": "your_turn", "announced": false, "viewed": true}),
            ),
            ev(
                "c",
                s + 50_000,
                "cleared",
                json!({"kind": "your_turn", "by": "prompt", "waited_ms": 50_000, "announced": false}),
            ),
            // Carried over a restart, cleared a day later, 13 h after its
            // pane died; and one cleared a minute after (not stale).
            ev(
                "d",
                s + DAY,
                "cleared",
                json!({"kind": "needs_you", "by": "ended", "ended": "pane", "dead_at": s + 11 * HOUR, "waited_ms": DAY, "carried": true}),
            ),
            ev(
                "h",
                s + DAY,
                "cleared",
                json!({"kind": "needs_you", "by": "ended", "ended": "pane", "dead_at": s + DAY - 60_000, "waited_ms": DAY}),
            ),
            // A day, then the user archived it: an ordinary ending.
            ev(
                "f",
                s + DAY,
                "cleared",
                json!({"kind": "needs_you", "by": "ended", "ended": "archive", "waited_ms": DAY}),
            ),
            // Open, its pane found dead 13 h ago; and one dead an hour ago.
            ev("e", s, "entered", json!({"kind": "needs_you", "since": s})),
            ev("i", s, "entered", json!({"kind": "needs_you", "since": s})),
            // Open for 13 h, its pane died before it began: not this one.
            ev("g", s, "entered", json!({"kind": "your_turn", "since": s})),
        ];
        let now = s + 14 * HOUR;
        let deaths: HashMap<String, Vec<i64>> = [
            ("e".to_string(), vec![s + HOUR]),
            ("g".to_string(), vec![s - HOUR]),
            ("i".to_string(), vec![now - HOUR]),
        ]
        .into();
        let span = Span::of(7 * DAY, Some(s), now);
        assert!(span.since_start);
        let r = report(&evs, &deaths, now, span);
        assert_eq!(r.needs_you.entered, 3);
        assert_eq!(r.needs_you.announced, 1);
        assert_eq!(r.needs_you.answered_ms, [80_000]);
        assert_eq!(r.needs_you.announced_then_self, 0);
        assert_eq!(r.your_turn.announced, 1);
        assert_eq!(r.your_turn.withheld, 1);
        assert_eq!((r.your_turn.seen_before, r.your_turn.repeats), (0, 0));
        assert_eq!(r.your_turn.announced_then_self, 1);
        assert_eq!(r.your_turn.cleared.get("prompt"), Some(&1));
        assert_eq!(r.stale_dead, 2);
        let text = render(&r);
        assert!(
            text.contains("ok   needs-you announcements cleared by the agent itself: 0"),
            "{text}"
        );
        assert!(
            text.contains("FAIL your-turn announcements cleared by the agent within 30 s: 1 of 1"),
            "{text}"
        );
        assert!(
            text.contains("FAIL entries still open 12 h after their pane was found dead: 2"),
            "{text}"
        );
        assert!(text.contains("answered after: median 80s"), "{text}");
        assert!(text.contains("known limits (discount these)"), "{text}");
    }

    /// The log, as the daemon reads it for the report and at a restart.
    #[test]
    fn the_log_is_read_back() {
        let db = rlord_core::db::Db::open_in_memory().unwrap();
        let ev = |id: &str, ts: i64, kind: &str, p: Value| {
            db.insert_event(id, ts, kind, &p.to_string()).unwrap()
        };
        ev(
            "a",
            10,
            "attention_entered",
            json!({"kind": "your_turn", "since": 10}),
        );
        ev("a", 11, "agent_stopped", json!({}));
        ev(
            "a",
            12,
            "attention_cleared",
            json!({"kind": "your_turn", "by": "prompt"}),
        );
        ev(
            "a",
            13,
            "attention_entered",
            json!({"kind": "your_turn", "since": 13}),
        );
        ev(
            "b",
            14,
            "attention_entered",
            json!({"kind": "needs_you", "since": 14}),
        );
        ev("b", 15, "session_end", json!({"from": "pane"}));
        ev("b", 16, "session_end", json!({"hook": "SessionEnd"}));
        ev("b", 17, "reconciled_dead", json!({}));
        ev("c", 18, "status_rechecked", json!({"why": "turn_ended"}));
        let evs = db.attention_events(11).unwrap();
        assert_eq!(evs.len(), 3, "attention events only, from 11 on");
        let deaths = db.pane_deaths(0).unwrap();
        assert_eq!(deaths.get("b"), Some(&vec![15, 17]));
        assert!(!deaths.contains_key("a") && !deaths.contains_key("c"));
        // A payload that isn't JSON, or has no kind: its row says None,
        // and the rest still comes back.
        db.insert_event("d", 19, "attention_entered", "not json")
            .unwrap();
        db.insert_event("e", 20, "attention_entered", r#"{"since": 20}"#)
            .unwrap();
        let mut open: Vec<(String, Option<String>, Option<i64>)> = db
            .open_attention_entries()
            .unwrap()
            .into_iter()
            .map(|e| (e.session, e.kind, e.since))
            .collect();
        open.sort();
        assert_eq!(
            open,
            [
                ("a".to_string(), Some("your_turn".to_string()), Some(13)),
                ("b".to_string(), Some("needs_you".to_string()), Some(14)),
                ("d".to_string(), None, None),
                ("e".to_string(), None, Some(20)),
            ]
        );
    }

    /// Not announced, split by why.
    #[test]
    fn not_announced_says_why() {
        let ev = |p: Value| Ev {
            session: "a".into(),
            ts: 1,
            kind: "attention_settled".into(),
            payload: p,
        };
        let evs = [
            ev(json!({"kind": "your_turn", "announced": false, "viewed": true})),
            ev(json!({"kind": "your_turn", "announced": false, "viewed": false, "repeat": true})),
            ev(json!({"kind": "your_turn", "announced": false, "viewed": false, "seen": true})),
            ev(json!({"kind": "your_turn", "announced": true, "viewed": false})),
        ];
        let r = report(&evs, &HashMap::new(), 1, Span::default());
        let k = &r.your_turn;
        assert_eq!(
            (k.announced, k.withheld, k.repeats, k.seen_before),
            (1, 1, 1, 1)
        );
        assert!(render(&r).contains("not announced: 1 looked at, 1 seen before, 1 repeats"));
    }

    #[test]
    fn an_empty_run_says_so() {
        let r = report(&[], &HashMap::new(), 0, Span::of(7 * DAY, None, 0));
        assert_eq!(r.span.days, 0.0);
        assert!(render(&r).contains("cleared by: -"));
        // A run older than the window: the window, said so.
        let span = Span::of(7 * DAY, Some(0), 30 * DAY);
        assert!(!span.since_start && span.days == 7.0);
        assert!(render(&report(&[], &HashMap::new(), 30 * DAY, span)).contains("the last 7 days"));
    }
}
