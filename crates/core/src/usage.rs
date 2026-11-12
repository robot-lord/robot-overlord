//! The session list's Usage column: how much a session has used, as turns
//! (prompts typed), tokens (taken in new or written) or context % (how full
//! the context window is).
//!
//! The numbers come from data rlord already reads: `transcript_stats` for
//! JSONL transcripts (incremental, so a growing file costs only what was
//! appended) and `opencode::usage` for OpenCode (cached until its database
//! changes). No model calls.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agents::{opencode, transcript};
use crate::models::context_window;

/// What the Usage column shows (and the usage sort sorts by).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageMetric {
    #[default]
    Turns,
    Tokens,
    Context,
}

impl UsageMetric {
    pub const ALL: [UsageMetric; 3] = [
        UsageMetric::Turns,
        UsageMetric::Tokens,
        UsageMetric::Context,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_lowercase().as_str() {
            "turns" | "turn" | "prompts" => UsageMetric::Turns,
            "tokens" | "token" => UsageMetric::Tokens,
            "context" | "ctx" | "context%" | "context-pct" | "context_pct" | "%" => {
                UsageMetric::Context
            }
            _ => return None,
        })
    }

    /// The value in config, views and the API (`parse` reads it).
    pub fn label(self) -> &'static str {
        match self {
            UsageMetric::Turns => "turns",
            UsageMetric::Tokens => "tokens",
            UsageMetric::Context => "context",
        }
    }

    /// What the user reads (launcher, toolbar).
    pub fn title(self) -> &'static str {
        match self {
            UsageMetric::Turns => "turns",
            UsageMetric::Tokens => "tokens",
            UsageMetric::Context => "context %",
        }
    }
}

/// A conversation's usage.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// Prompts the human typed.
    pub turns: u64,
    /// Tokens taken in new (not read from a cache) or written.
    pub tokens: u64,
    /// The latest request's whole context, in tokens.
    pub context: u64,
    /// The model's context window; None when unknown.
    pub window: Option<u64>,
}

impl Usage {
    /// How full the context window is, 0–100. None when the window is
    /// unknown.
    pub fn context_pct(&self) -> Option<u64> {
        let w = self.window.filter(|w| *w > 0)?;
        Some(((self.context * 100 + w / 2) / w).min(100))
    }

    /// The number shown (and sorted by) for `m`.
    pub fn value(&self, m: UsageMetric) -> Option<u64> {
        match m {
            UsageMetric::Turns => Some(self.turns),
            UsageMetric::Tokens => Some(self.tokens),
            UsageMetric::Context => self.context_pct(),
        }
    }

    /// The column's cell: always 4 characters (see `cell`).
    pub fn cell(&self, m: UsageMetric) -> String {
        cell(self.value(m), m)
    }

    /// As the API gives it (session rows' `usage`).
    pub fn to_json(&self) -> Value {
        json!({
            "turns": self.turns,
            "tokens": self.tokens,
            "context": self.context,
            "window": self.window,
            "context_pct": self.context_pct(),
        })
    }

    pub fn from_json(v: &Value) -> Option<Usage> {
        Some(Usage {
            turns: v["turns"].as_u64()?,
            tokens: v["tokens"].as_u64().unwrap_or(0),
            context: v["context"].as_u64().unwrap_or(0),
            window: v["window"].as_u64(),
        })
    }
}

/// A Usage cell: `value` (a percentage for `Context`) in exactly 4
/// characters, right-aligned, so the column never shifts as numbers grow:
/// `  42`, `1.2k`, ` 45k`, `3.1M`, ` 42%`; `   –` when unknown.
pub fn cell(value: Option<u64>, m: UsageMetric) -> String {
    match (value, m) {
        (None, _) => format!("{:>4}", "–"),
        (Some(p), UsageMetric::Context) => format!("{:>3}%", p.min(100)),
        (Some(n), _) => format!("{:>4}", compact(n)),
    }
}

/// A count in at most 4 characters: up to 3 digits, then a k/M/G/T suffix
/// (`999`, `1.2k`, `45k`, `999k`, `1.0M`, `3.1M`).
pub fn compact(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    for (unit, suffix) in [(1e3, "k"), (1e6, "M"), (1e9, "G"), (1e12, "T")] {
        let x = n as f64 / unit;
        if x < 9.95 {
            return format!("{x:.1}{suffix}");
        }
        if x.round() < 1000.0 {
            return format!("{:.0}{suffix}", x.round());
        }
    }
    "999T".to_string()
}

/// A conversation's usage from its transcript (`configured`: the model the
/// session was launched with, for the window when the transcript names
/// none yet). None: there's no transcript to read (yet).
pub fn of(path: &Path, configured: Option<&str>) -> Option<Usage> {
    let st = match opencode::locate(path) {
        Some((db, id)) => opencode::usage(db, id)?,
        None => transcript::transcript_stats(path)?,
    };
    Some(from_stats(&st, configured))
}

/// `of` for several OpenCode conversations in one database `db` (a list's
/// rows), over one connection: (conversation id, configured model) in,
/// usage per conversation id out (those found).
pub fn of_opencode(db: &Path, items: &[(&str, Option<&str>)]) -> HashMap<String, Usage> {
    let ids: Vec<&str> = items.iter().map(|(id, _)| *id).collect();
    let stats = opencode::usage_many(db, &ids);
    items
        .iter()
        .filter_map(|(id, configured)| {
            let st = stats.get(*id)?;
            Some((id.to_string(), from_stats(st, *configured)))
        })
        .collect()
}

fn from_stats(st: &transcript::TranscriptStats, configured: Option<&str>) -> Usage {
    // A configured `[1m]` model (`opus[1m]`) runs with a 1M window, which
    // its transcript doesn't say: it names the model alone.
    let one_m = configured.is_some_and(|m| m.trim().to_lowercase().ends_with("[1m]"));
    let mut window = st.window.or_else(|| {
        if one_m {
            return Some(1_000_000);
        }
        st.model
            .as_deref()
            .and_then(context_window)
            .or_else(|| configured.and_then(context_window))
    });
    // More context than a 200k window holds: a 1M-context variant too
    // (one switched to mid-session, say).
    if window == Some(200_000) && st.context > 200_000 {
        window = Some(1_000_000);
    }
    Usage {
        turns: st.turns,
        tokens: st.tokens.max(0) as u64,
        context: st.context,
        window,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_fits_four_characters() {
        let cases = [
            (0, "0"),
            (7, "7"),
            (999, "999"),
            (1000, "1.0k"),
            (1234, "1.2k"),
            (9949, "9.9k"),
            (9950, "10k"),
            (45_000, "45k"),
            (999_499, "999k"),
            (999_500, "1.0M"),
            (3_140_000, "3.1M"),
            (31_400_000, "31M"),
            (2_000_000_000, "2.0G"),
        ];
        for (n, want) in cases {
            assert_eq!(compact(n), want, "{n}");
            assert_eq!(cell(Some(n), UsageMetric::Tokens).chars().count(), 4);
        }
        assert_eq!(cell(Some(42), UsageMetric::Turns), "  42");
        assert_eq!(cell(Some(45_000), UsageMetric::Tokens), " 45k");
        assert_eq!(cell(Some(42), UsageMetric::Context), " 42%");
        assert_eq!(cell(Some(100), UsageMetric::Context), "100%");
        assert_eq!(cell(None, UsageMetric::Context), "   –");
        assert_eq!(cell(None, UsageMetric::Context).chars().count(), 4);
    }

    #[test]
    fn context_pct_needs_a_window() {
        let mut u = Usage {
            turns: 3,
            tokens: 10,
            context: 84_000,
            window: Some(200_000),
        };
        assert_eq!(u.context_pct(), Some(42));
        assert_eq!(u.value(UsageMetric::Context), Some(42));
        assert_eq!(u.value(UsageMetric::Turns), Some(3));
        u.context = 400_000;
        assert_eq!(u.context_pct(), Some(100), "never past 100");
        u.window = None;
        assert_eq!(u.cell(UsageMetric::Context), "   –");
        assert_eq!(Usage::from_json(&u.to_json()), Some(u));
    }

    #[test]
    fn metric_names_round_trip() {
        for m in UsageMetric::ALL {
            assert_eq!(UsageMetric::parse(m.label()), Some(m));
        }
        assert_eq!(UsageMetric::parse("ctx"), Some(UsageMetric::Context));
        assert_eq!(UsageMetric::parse("bogus"), None);
    }

    #[test]
    fn claude_usage_from_a_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc.jsonl");
        // Two prompts; one message written over two lines (its usage
        // counts once); a 1M-context run (more than 200k sent).
        let a = |id: &str, ctx: u64| {
            format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"2026-10-27T17:03:48.000Z\",\
                 \"message\":{{\"id\":\"{id}\",\"model\":\"claude-opus-5-5\",\"role\":\"assistant\",\
                 \"content\":[{{\"type\":\"text\",\"text\":\"ok\"}}],\
                 \"usage\":{{\"input_tokens\":10,\"cache_creation_input_tokens\":100,\
                 \"cache_read_input_tokens\":{ctx},\"output_tokens\":5}}}}}}\n"
            )
        };
        let u = |t: &str| {
            format!(
                "{{\"type\":\"user\",\"timestamp\":\"2026-10-27T17:03:40.000Z\",\
                 \"message\":{{\"role\":\"user\",\"content\":\"{t}\"}}}}\n"
            )
        };
        let text = [
            u("one"),
            a("m1", 1000),
            a("m1", 1000),
            u("two"),
            a("m2", 2000),
        ]
        .concat();
        std::fs::write(&p, &text).unwrap();
        let got = of(&p, None).unwrap();
        assert_eq!(got.turns, 2);
        assert_eq!(got.tokens, 2 * 115);
        assert_eq!(got.context, 2110);
        assert_eq!(got.window, Some(200_000));
        // Launched as `opus[1m]`: a 1M window, though the transcript names
        // only `claude-opus-5-5`.
        assert_eq!(of(&p, Some("opus[1m]")).unwrap().window, Some(1_000_000));
        assert_eq!(of(&p, Some("opus")).unwrap().window, Some(200_000));
        std::fs::write(&p, [text, a("m3", 300_000)].concat()).unwrap();
        let got = of(&p, None).unwrap();
        assert_eq!(got.window, Some(1_000_000));
        assert_eq!(got.context_pct(), Some(30));
    }
}
