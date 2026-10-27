//! Claude Code adapter.
//!
//! Identity model (see docs/architecture.md): rlord MINTS the Claude
//! session UUID and launches with `--session-id <uuid>`, so the transcript
//! path is known from second one and `--resume <uuid>` is always attributable.
//! Ids scavenged from disk scans are never allowed to authorize a resume.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::types::Status;

pub fn mint_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `~/.claude` or `$CLAUDE_CONFIG_DIR`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".claude")
        })
}

/// Claude Code's project-directory slug: every non-alphanumeric char of the
/// absolute project path becomes `-` (verified against real `~/.claude/projects`
/// entries; format is internal to Claude Code and pinned by tests).
pub fn project_slug(project_path: &str) -> String {
    project_path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn transcript_path(project_path: &str, session_uuid: &str) -> PathBuf {
    config_dir()
        .join("projects")
        .join(project_slug(project_path))
        .join(format!("{session_uuid}.jsonl"))
}

pub fn launch_argv(
    session_uuid: &str,
    first_prompt: Option<&str>,
    model: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        "claude".to_string(),
        "--session-id".to_string(),
        session_uuid.to_string(),
    ];
    if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
        argv.push("--model".to_string());
        argv.push(m.trim().to_string());
    }
    if let Some(p) = first_prompt {
        argv.push(p.to_string());
    }
    argv
}

/// Resume argv. `--resume` only when a transcript actually exists (a resume of
/// a conversation Claude never wrote errors out); otherwise re-declare the id
/// so the *next* restart can resume. Scavenged ids never resume.
pub fn resume_argv(
    session_uuid: &str,
    trust: &str,
    project_path: &str,
    model: Option<&str>,
) -> Result<Vec<String>> {
    if trust == "scavenged" {
        bail!("refusing to resume a scavenged session id (untrusted attribution)");
    }
    let flag = if transcript_path(project_path, session_uuid).exists() {
        "--resume"
    } else {
        "--session-id"
    };
    let mut argv = vec![
        "claude".to_string(),
        flag.to_string(),
        session_uuid.to_string(),
    ];
    if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
        argv.push("--model".to_string());
        argv.push(m.trim().to_string());
    }
    Ok(argv)
}

/// Map a Claude hook event to (event kind for our log, optional new status).
pub fn map_hook_event(event: &str) -> (&'static str, Option<Status>) {
    match event {
        "SessionStart" => ("session_start", Some(Status::Idle)),
        "UserPromptSubmit" => ("prompt_submitted", Some(Status::Working)),
        "Stop" => ("agent_stopped", Some(Status::Idle)),
        "Notification" => ("notification", Some(Status::WaitingInput)),
        "SessionEnd" => ("session_end", Some(Status::Stopped)),
        "PreCompact" => ("pre_compact", None),
        "SubagentStart" => ("subagent_started", None),
        "SubagentStop" => ("subagent_stopped", None),
        _ => ("hook_other", None),
    }
}

const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "Notification",
    "SessionEnd",
    "PreCompact",
    "SubagentStart",
    "SubagentStop",
];

/// Merge our hook command into Claude's settings.json, preserving everything
/// already there (including other tools' hooks). Idempotent. Returns true if
/// the file was modified.
pub fn install_hooks(settings_path: &Path, hook_cmd: &str) -> Result<bool> {
    let mut root: Value = match std::fs::read_to_string(settings_path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).context("parsing existing settings.json")?
        }
        _ => json!({}),
    };
    if !root.is_object() {
        bail!("settings.json is not a JSON object; refusing to modify");
    }
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        bail!("settings.json 'hooks' is not an object; refusing to modify");
    }
    let mut changed = false;
    for event in HOOK_EVENTS {
        let arr = hooks
            .as_object_mut()
            .unwrap()
            .entry(*event)
            .or_insert_with(|| json!([]));
        if !arr.is_array() {
            bail!("settings.json hooks.{event} is not an array; refusing to modify");
        }
        // Our entry is any command of the form `<path> hook emit ...` from a
        // current or former binary name. If it exists with a stale path
        // (moved/renamed binary), fix it in place; otherwise append.
        // ("overlord", the pre-rename binary, also contains "rlord".)
        let is_ours = |h: &Value| {
            h["command"]
                .as_str()
                .is_some_and(|c| c.contains("hook emit") && c.contains("rlord"))
        };
        let mut found = false;
        for entry in arr.as_array_mut().unwrap().iter_mut() {
            if let Some(hs) = entry["hooks"].as_array_mut() {
                for h in hs.iter_mut() {
                    if is_ours(h) {
                        found = true;
                        if h["command"].as_str() != Some(hook_cmd) {
                            h["command"] = json!(hook_cmd);
                            changed = true;
                        }
                    }
                }
            }
        }
        if !found {
            arr.as_array_mut()
                .unwrap()
                .push(json!({"hooks": [{"type": "command", "command": hook_cmd}]}));
            changed = true;
        }
    }
    if changed {
        if let Some(dir) = settings_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let backup = settings_path.with_extension("json.pre-rlord.bak");
        if settings_path.exists() && !backup.exists() {
            std::fs::copy(settings_path, &backup)?;
        }
        let tmp = settings_path.with_extension("json.rlord-tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&root)? + "\n")?;
        std::fs::rename(&tmp, settings_path)?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_observed_claude_encoding() {
        assert_eq!(
            project_slug("/home/alice/ko/myapp"),
            "-home-alice-ko-myapp"
        );
        assert_eq!(
            project_slug("/home/alice/ko/myapp/.worktrees/feature/x"),
            "-home-alice-ko-myapp--worktrees-feature-x"
        );
    }

    #[test]
    fn launch_and_resume_argv() {
        let argv = launch_argv("u-1", Some("hello"), None);
        assert_eq!(argv, ["claude", "--session-id", "u-1", "hello"]);
        let argv = launch_argv("u-1", None, Some("opus"));
        assert_eq!(argv, ["claude", "--session-id", "u-1", "--model", "opus"]);
        // No transcript on disk for this fake id → re-declare, don't resume.
        let argv = resume_argv(
            "00000000-0000-0000-0000-000000000000",
            "minted",
            "/nonexistent",
            None,
        )
        .unwrap();
        assert_eq!(argv[1], "--session-id");
        assert!(resume_argv("u-1", "scavenged", "/tmp", None).is_err());
    }

    #[test]
    fn hook_install_is_idempotent_and_preserving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"other-tool hook-handler"}]}]}}"#,
        )
        .unwrap();
        assert!(install_hooks(&path, "/bin/rlord hook emit claude").unwrap());
        assert!(!install_hooks(&path, "/bin/rlord hook emit claude").unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["model"], "opus"); // untouched
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2); // other-tool entry preserved, ours appended
        assert!(v["hooks"]["UserPromptSubmit"].as_array().unwrap().len() == 1);
        assert!(path.with_extension("json.pre-rlord.bak").exists());
    }
}

// ---- sub-agents on disk ----

use crate::types::Subagent;

/// `<project dir>/<session uuid>/subagents/` for a session transcript path.
pub fn subagents_dir(transcript_path: &str) -> PathBuf {
    let p = Path::new(transcript_path);
    let stem = p.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
    p.with_file_name(stem).join("subagents")
}

/// Parse ISO-8601 `2026-10-27T17:03:44.498Z` to ms epoch (lenient).
fn parse_iso_ms(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = rest.trim_end_matches('Z');
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next()?.parse().ok()?;
    // days from civil (Howard Hinnant)
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86_400_000 + h * 3_600_000 + mi * 60_000 + (sec * 1000.0) as i64)
}

/// Token totals and time span from a sub-agent transcript.
pub fn transcript_stats(path: &Path) -> Option<(i64, i64, Option<i64>, Option<i64>)> {
    let content = std::fs::read_to_string(path).ok()?;
    let (mut input, mut output) = (0i64, 0i64);
    let (mut first, mut last) = (None, None);
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(ts) = v["timestamp"].as_str().and_then(parse_iso_ms) {
            first.get_or_insert(ts);
            last = Some(ts);
        }
        if let Some(u) = v["message"]["usage"].as_object() {
            input += u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
            output += u.get("output_tokens").and_then(Value::as_i64).unwrap_or(0);
        }
    }
    Some((input, output, first, last))
}

/// Every sub-agent described by a `*.meta.json` in the session's subagents
/// dir (the Agent tool writes one per real sub-agent; Claude's internal
/// helper agents have none and are deliberately excluded).
pub fn scan_subagents(session_id: &str, transcript_path: &str) -> Vec<Subagent> {
    let dir = subagents_dir(transcript_path);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(id) = name
            .strip_prefix("agent-")
            .and_then(|n| n.strip_suffix(".meta.json"))
        else {
            continue;
        };
        let Ok(meta) = std::fs::read_to_string(e.path())
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .ok_or(())
        else {
            continue;
        };
        let jsonl = dir.join(format!("agent-{id}.jsonl"));
        let stats = transcript_stats(&jsonl);
        let finished = meta["finished"].as_bool().unwrap_or(false);
        out.push(Subagent {
            agent_id: id.to_string(),
            session_id: session_id.to_string(),
            agent_type: meta["agentType"].as_str().unwrap_or("").to_string(),
            description: meta["description"].as_str().unwrap_or("").to_string(),
            tool_use_id: meta["toolUseId"].as_str().map(str::to_string),
            spawn_depth: meta["spawnDepth"].as_i64().unwrap_or(1),
            background: meta["requestShape"].as_str() == Some("background"),
            started_at: stats.and_then(|s| s.2),
            finished_at: if finished {
                stats.and_then(|s| s.3)
            } else {
                None
            },
            transcript_path: jsonl.exists().then(|| jsonl.to_string_lossy().into_owned()),
            result: String::new(),
            input_tokens: stats.map(|s| s.0).unwrap_or(0),
            output_tokens: stats.map(|s| s.1).unwrap_or(0),
        });
    }
    out
}

#[cfg(test)]
mod subagent_tests {
    use super::*;

    #[test]
    fn iso_to_ms() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_iso_ms("2026-09-11T20:20:47.939Z"),
            Some(1_789_158_047_939)
        );
    }

    #[test]
    fn dir_layout_and_scan() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("abc.jsonl");
        let sub = subagents_dir(transcript.to_str().unwrap());
        assert!(sub.ends_with("abc/subagents"));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("agent-a1.meta.json"),
            r#"{"agentType":"Explore","description":"Find X","toolUseId":"toolu_1","spawnDepth":1,"requestShape":"background","finished":true}"#,
        )
        .unwrap();
        std::fs::write(
            sub.join("agent-a1.jsonl"),
            "{\"type\":\"user\",\"timestamp\":\"2026-10-27T17:03:44.000Z\"}\n\
             {\"type\":\"assistant\",\"timestamp\":\"2026-10-27T17:03:48.000Z\",\"message\":{\"usage\":{\"input_tokens\":6,\"output_tokens\":260}}}\n",
        )
        .unwrap();
        std::fs::write(sub.join("junk.txt"), "x").unwrap();
        let v = scan_subagents("s1", transcript.to_str().unwrap());
        assert_eq!(v.len(), 1);
        let a = &v[0];
        assert_eq!(a.agent_id, "a1");
        assert_eq!(a.description, "Find X");
        assert!(a.background);
        assert_eq!(a.output_tokens, 260);
        assert_eq!(a.finished_at.unwrap() - a.started_at.unwrap(), 4000);
    }
}
