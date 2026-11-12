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
    // Older only if it differs from the latest within the parts both name:
    // `claude-opus-5` doesn't say which 5.x, so it isn't called outdated.
    let older = |v: &Vec<u32>| {
        v.iter()
            .zip(&latest_v)
            .find(|(a, b)| a != b)
            .is_some_and(|(a, b)| a < b)
    };
    let pinned_old = cfg
        .as_ref()
        .is_some_and(|(f, v)| Some(f) == family.as_ref() && older(v));
    let observed_old = obs.as_ref().is_some_and(|(_, v)| older(v));
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

/// Context window (tokens) per model family, first match wins. A model
/// none of these name has no known window: its context % shows as "–".
/// Codex reports its window itself (`model_context_window`), which wins.
/// Keep current with model releases, like `CLAUDE_LATEST`.
const CONTEXT_WINDOWS: &[(&str, u64)] = &[
    ("gpt-4.1", 1_047_576),
    ("gpt-4o", 128_000),
    ("gpt-5", 272_000),
    ("codex", 272_000),
    ("o3", 200_000),
    ("o4", 200_000),
    ("gemini", 1_048_576),
    ("claude", 200_000),
    ("opus", 200_000),
    ("sonnet", 200_000),
    ("haiku", 200_000),
    ("fable", 200_000),
];

/// The context window of a model id or alias (`provider/model` too), from
/// `CONTEXT_WINDOWS`; a `[1m]` suffix (Claude's 1M-context variants) is
/// 1M. None: unknown.
pub fn context_window(model: &str) -> Option<u64> {
    let m = model.trim().to_lowercase();
    if m.ends_with("[1m]") {
        return Some(1_000_000);
    }
    let m = m.rsplit('/').next().unwrap_or("");
    CONTEXT_WINDOWS
        .iter()
        .find(|(fam, _)| m.starts_with(fam) || m.contains(&format!("-{fam}")))
        .map(|(_, w)| *w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_windows_by_family() {
        assert_eq!(context_window("claude-opus-5-5"), Some(200_000));
        assert_eq!(context_window("opus"), Some(200_000));
        assert_eq!(context_window("sonnet[1m]"), Some(1_000_000));
        assert_eq!(context_window("anthropic/claude-sonnet-4-5"), Some(200_000));
        assert_eq!(context_window("gpt-5.1-codex"), Some(272_000));
        assert_eq!(context_window("openai/gpt-4.1-mini"), Some(1_047_576));
        assert_eq!(context_window("o3"), Some(200_000));
        assert_eq!(context_window("google/gemini-2.5-pro"), Some(1_048_576));
        assert_eq!(context_window("llama-3-70b"), None);
        assert_eq!(context_window(""), None);
    }

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
    fn version_prefix_is_not_outdated() {
        // `claude-opus-5` doesn't say which 5.x: not called outdated.
        assert!(!check("claude", Some("claude-opus-5"), None).outdated);
        assert!(check("claude", Some("claude-opus-4-1"), None).outdated);
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
