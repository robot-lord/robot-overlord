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

pub fn launch_argv(session_uuid: &str, first_prompt: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "claude".to_string(),
        "--session-id".to_string(),
        session_uuid.to_string(),
    ];
    if let Some(p) = first_prompt {
        argv.push(p.to_string());
    }
    argv
}

/// Resume argv. `--resume` only when a transcript actually exists (a resume of
/// a conversation Claude never wrote errors out); otherwise re-declare the id
/// so the *next* restart can resume. Scavenged ids never resume.
pub fn resume_argv(session_uuid: &str, trust: &str, project_path: &str) -> Result<Vec<String>> {
    if trust == "scavenged" {
        bail!("refusing to resume a scavenged session id (untrusted attribution)");
    }
    let flag = if transcript_path(project_path, session_uuid).exists() {
        "--resume"
    } else {
        "--session-id"
    };
    Ok(vec![
        "claude".to_string(),
        flag.to_string(),
        session_uuid.to_string(),
    ])
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
        let argv = launch_argv("u-1", Some("hello"));
        assert_eq!(argv, ["claude", "--session-id", "u-1", "hello"]);
        // No transcript on disk for this fake id → re-declare, don't resume.
        let argv = resume_argv(
            "00000000-0000-0000-0000-000000000000",
            "minted",
            "/nonexistent",
        )
        .unwrap();
        assert_eq!(argv[1], "--session-id");
        assert!(resume_argv("u-1", "scavenged", "/tmp").is_err());
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
