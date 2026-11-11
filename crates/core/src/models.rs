//! Per-tool model catalog and freshness check.
//!
//! There is no tool-agnostic way to ask "what is the newest Opus?", so rlord
//! carries a small hand-maintained table per agent CLI — update it when a
//! model ships. The check compares it against two facts:
//!
//! - the **configured** model (an alias like `opus`, or a pinned id), and
//! - the **observed** model: what the agent actually ran, read from its
//!   transcript. This catches an alias resolving to an old model because the
//!   agent CLI itself is out of date.

use serde::Serialize;

/// Newest model per family, per agent. Keep current with each model release.
const CLAUDE_LATEST: &[(&str, &str)] = &[
    ("opus", "claude-opus-5-5"),
    ("sonnet", "claude-sonnet-5-5"),
    ("haiku", "claude-haiku-5-5"),
    ("fable", "claude-fable-5-2"),
];

pub fn latest_table(agent: &str) -> &'static [(&'static str, &'static str)] {
    match agent {
        "claude" => CLAUDE_LATEST,
        _ => &[],
    }
}

/// `(family, version)` of a model id or alias. Version is every numeric
/// token except 8-digit date stamps: `claude-opus-4-1-20250805` → (opus, [4,1]),
/// `claude-3-opus-20240229` → (opus, [3]), `opus` → (opus, []).
pub fn parse(model: &str) -> Option<(String, Vec<u32>)> {
    let m = model.trim().to_lowercase();
    let m = m.split('[').next().unwrap_or(""); // `opus[1m]` context suffixes
    let mut family = None;
    let mut version = Vec::new();
    for tok in m.split(['-', '.', '@', '_']) {
        if let Ok(n) = tok.parse::<u32>() {
            if tok.len() < 8 {
                version.push(n);
            }
        } else if ["opus", "sonnet", "haiku", "fable"].contains(&tok) {
            family = Some(tok.to_string());
        }
    }
    family.map(|f| (f, version))
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Freshness {
    pub family: Option<String>,
    /// Newest id in the family, per the catalog.
    pub latest: Option<String>,
    /// True when the session is (or will be) running an older model.
    pub outdated: bool,
    /// Human explanation when outdated (or when the catalog looks stale).
    pub note: Option<String>,
    /// What to set the model to in order to get the latest.
    pub suggest: Option<String>,
}

pub fn check(agent: &str, configured: Option<&str>, observed: Option<&str>) -> Freshness {
    let configured = configured.map(str::trim).filter(|s| !s.is_empty());
    let cfg = configured.and_then(parse);
    let obs = observed.and_then(parse);
    let family = obs
        .as_ref()
        .map(|(f, _)| f.clone())
        .or_else(|| cfg.as_ref().map(|(f, _)| f.clone()));
    let latest = family.as_deref().and_then(|f| {
        latest_table(agent)
            .iter()
            .find(|(fam, _)| *fam == f)
            .map(|(_, id)| id.to_string())
    });
    let mut out = Freshness {
        family: family.clone(),
        latest: latest.clone(),
        outdated: false,
        note: None,
        suggest: None,
    };
    let Some(latest) = latest else {
        return out;
    };
    let latest_v = parse(&latest).map(|(_, v)| v).unwrap_or_default();
    let pinned_old = cfg
        .as_ref()
        .is_some_and(|(f, v)| Some(f) == family.as_ref() && !v.is_empty() && *v < latest_v);
    let observed_old = obs
        .as_ref()
        .is_some_and(|(_, v)| !v.is_empty() && *v < latest_v);
    let fam = family.unwrap_or_default();
    if pinned_old {
        out.outdated = true;
        out.suggest = Some(fam.clone());
        out.note = Some(format!(
            "pinned to {}; the latest {fam} is {latest}",
            configured.unwrap_or("?")
        ));
    } else if observed_old {
        out.outdated = true;
        out.note = Some(format!(
            "running {}; the latest {fam} is {latest}{}",
            observed.unwrap_or("?"),
            if cfg.as_ref().is_some_and(|(_, v)| v.is_empty()) {
                format!(
                    " — `{}` resolves to an old model, so update the {agent} CLI",
                    configured.unwrap_or("")
                )
            } else {
                String::new()
            }
        ));
        out.suggest = Some(fam);
    } else if obs.as_ref().is_some_and(|(_, v)| *v > latest_v) {
        out.note = Some(format!(
            "running {}, newer than rlord's catalog ({latest}) — update rlord's model table",
            observed.unwrap_or("?")
        ));
    }
    out
}

/// The model of the last assistant turn in a Claude transcript (JSONL),
/// reading only the file's tail.
pub fn observed_claude_model(transcript: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(transcript).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(512 * 1024);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines().rev().find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let m = v.get("message")?.get("model")?.as_str()?;
        m.starts_with("claude").then(|| m.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ids_and_aliases() {
        assert_eq!(parse("opus"), Some(("opus".into(), vec![])));
        assert_eq!(parse("claude-opus-5-5"), Some(("opus".into(), vec![5, 5])));
        assert_eq!(
            parse("claude-opus-4-1-20250805"),
            Some(("opus".into(), vec![4, 1]))
        );
        assert_eq!(
            parse("claude-3-opus-20240229"),
            Some(("opus".into(), vec![3]))
        );
        assert_eq!(parse("opus[1m]"), Some(("opus".into(), vec![])));
        assert_eq!(parse("gpt-5"), None);
    }

    #[test]
    fn freshness() {
        // Alias that resolves to the latest: fine.
        let f = check("claude", Some("opus"), Some("claude-opus-5-5"));
        assert!(!f.outdated);
        assert_eq!(f.latest.as_deref(), Some("claude-opus-5-5"));
        // Alias, never run yet: fine (nothing observed).
        assert!(!check("claude", Some("opus"), None).outdated);
        // Pinned old id.
        let f = check("claude", Some("claude-opus-4-6"), None);
        assert!(f.outdated);
        assert_eq!(f.suggest.as_deref(), Some("opus"));
        // Alias resolving to an old model: the CLI is stale.
        let f = check("claude", Some("opus"), Some("claude-opus-4-6"));
        assert!(f.outdated);
        assert!(f.note.unwrap().contains("update the claude CLI"));
        // Running something newer than the catalog: not outdated, but noted.
        let f = check("claude", Some("opus"), Some("claude-opus-6-0"));
        assert!(!f.outdated);
        assert!(f.note.is_some());
        // Unknown agent: no opinion.
        assert!(!check("codex", Some("gpt-5"), None).outdated);
    }
}
