//! The overlord: a singleton, long-running agent session whose job is to
//! drive rlord itself. It is an ordinary session in every respect (tmux,
//! hooks, resume) plus three things: `role = 'overlord'`, a fixed tmux name
//! (so the TUI can embed it without a lookup), and rlord's MCP
//! server wired in with its tools pre-allowed.
//!
//! It lives in its own folder under the data dir. On every `ensure` that
//! folder's CLAUDE.md / AGENTS.md / GEMINI.md (the same standing instructions,
//! from `overlord.md` plus the user's own) and mcp.json are regenerated, so
//! upgrades to rlord reach an existing overlord on its next restart.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use rlord_core::config::Config;
use rlord_core::db::Db;
use rlord_core::types::Status;
use rlord_core::{models, paths, tmux};

use crate::methods;

/// Fixed tmux session name. The TUI embeds this session's pane.
pub const TMUX_NAME: &str = "rlord_overlord";
pub const NAME: &str = "overlord";
pub const ROLE: &str = "overlord";

pub fn dir() -> PathBuf {
    paths::data_dir().join("overlord")
}

/// User-authored standing instructions appended to the generated ones.
pub fn user_instructions_path() -> PathBuf {
    paths::config_path()
        .parent()
        .map(|p| p.join("overlord.md"))
        .unwrap_or_else(|| PathBuf::from("overlord.md"))
}

/// Write the overlord's folder (instruction files + mcp.json); return the adapter
/// flags it must always be launched with.
fn write_files() -> Result<Vec<String>> {
    let d = dir();
    std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    let mcp_path = d.join("mcp.json");
    let mcp = json!({"mcpServers": {"rlord": {
        "type": "stdio",
        "command": methods::rlord_bin(),
        "args": ["mcp"],
    }}});
    std::fs::write(&mcp_path, serde_json::to_string_pretty(&mcp)?)?;

    let mut md = format!("{GENERATED_NOTE}{INSTRUCTIONS}");
    if let Ok(user) = std::fs::read_to_string(user_instructions_path()) {
        if !user.trim().is_empty() {
            md.push_str("\n## Standing instructions from the user\n\n");
            md.push_str(user.trim());
            md.push('\n');
        }
    }
    for name in CONTEXT_FILES {
        std::fs::write(d.join(name), &md)?;
    }
    // Subagents the overlord delegates to (Claude's project agents dir).
    let agents = d.join(".claude").join("agents");
    std::fs::create_dir_all(&agents)?;
    std::fs::write(agents.join("rlord-tidy.md"), TIDY_AGENT)?;

    Ok(vec![
        "--mcp-config".into(),
        mcp_path.to_string_lossy().into_owned(),
        // The whole point is a cheap, prompt-free control surface. Read-only
        // file tools are pre-allowed for new-session groundwork (reading a
        // repo's README or the files a request names); anything that writes
        // or runs commands still asks.
        "--allowedTools".into(),
        "mcp__rlord,Read,Grep,Glob".into(),
    ])
}

/// Make sure the overlord exists and is running; returns its session.
/// Creates it on first use (with `agent`/`model` from params, else config),
/// restarts it (resuming the conversation) if stopped. An existing
/// overlord's model is its own — changed only via `set_model`.
pub fn ensure(db: &Db, cfg: &Config, params: &Value) -> Result<Value> {
    let v = ensure_running(db, cfg, params)?;
    // Viewed full screen it should be unmistakable — but otherwise look like
    // any session.
    if let Err(e) = tmux::style_session(
        TMUX_NAME,
        "bg=magenta,fg=black",
        "#[bold] ◆ OVERLORD #[nobold]· ⌥D: list  ",
    ) {
        tracing::debug!(error = %e, "styling overlord session");
    }
    Ok(v)
}

fn ensure_running(db: &Db, cfg: &Config, params: &Value) -> Result<Value> {
    let extra_args = write_files()?;
    let pick = |k: &str, dflt: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(dflt)
            .to_string()
    };
    match db.overlord()? {
        None => {
            let model = pick("model", cfg.overlord.model.trim());
            let agent = pick("agent", &cfg.overlord.agent);
            let d = dir();
            methods::session_new(
                db,
                cfg,
                &json!({
                    "agent": agent,
                    "project": d.to_string_lossy(),
                    "workspace": false,
                    "name": NAME,
                    "model": model,
                    "role": ROLE,
                    "tmux_name": TMUX_NAME,
                    "extra_args": extra_args,
                }),
            )
        }
        Some(s) => {
            // Fresh adapter flags (e.g. the rlord binary path moved).
            let mut spec: Value =
                serde_json::from_str(&db.launch_spec(&s.id)?).unwrap_or(json!({}));
            spec["extra_args"] = json!(extra_args);
            db.set_launch_spec(&s.id, &spec.to_string())?;
            if s.archived_at.is_some() {
                db.set_archived(&s.id, None)?;
            }
            let alive = s
                .tmux_session
                .as_deref()
                .map(tmux::has_session)
                .unwrap_or(false);
            if alive && s.status != Status::Stopped {
                return Ok(serde_json::to_value(db.resolve_ref(&s.id)?)?);
            }
            methods::session_restart(db, &json!({"ref": s.id}))
        }
    }
}

/// Everything the startup check needs: does the overlord exist, what model
/// is it set to, what did it actually run, and is that the latest.
pub fn status(db: &Db, cfg: &Config) -> Result<Value> {
    let catalog = |agent: &str| -> Value {
        models::latest_table(agent)
            .iter()
            .map(|(fam, id)| json!({"family": fam, "latest": id}))
            .collect()
    };
    let Some(s) = db.overlord()? else {
        return Ok(json!({
            "exists": false,
            "default_agent": cfg.overlord.agent,
            "default_model": cfg.overlord.model,
            "agents": ["claude"],
            "catalog": catalog(&cfg.overlord.agent),
        }));
    };
    let observed = match s.agent.as_str() {
        "claude" => db
            .transcript_source(&s.id)?
            .and_then(|p| models::observed_claude_model(std::path::Path::new(&p))),
        _ => None,
    };
    let fresh = models::check(&s.agent, s.model.as_deref(), observed.as_deref());
    let running =
        s.status != Status::Stopped && s.tmux_session.as_deref().is_some_and(tmux::has_session);
    Ok(json!({
        "exists": true,
        "id": s.id,
        "status": s.status,
        "running": running,
        "agent": s.agent,
        "model": s.model,
        "observed_model": observed,
        "freshness": fresh,
        "catalog": catalog(&s.agent),
    }))
}

/// Change the overlord's model and restart it onto that model (the
/// conversation resumes).
pub fn set_model(db: &Db, cfg: &Config, params: &Value) -> Result<Value> {
    let model = methods::p_str(params, "model")?.trim().to_string();
    let Some(s) = db.overlord()? else {
        return ensure(db, cfg, &json!({"model": model}));
    };
    let mut spec: Value = serde_json::from_str(&db.launch_spec(&s.id)?).unwrap_or(json!({}));
    spec["model"] = if model.is_empty() {
        Value::Null
    } else {
        json!(model)
    };
    db.set_launch_spec(&s.id, &spec.to_string())?;
    db.insert_event(
        &s.id,
        rlord_core::types::now_ms(),
        "model_changed",
        &json!({"from": s.model, "to": model}).to_string(),
    )?;
    write_files()?;
    methods::session_restart(db, &json!({"ref": s.id}))?;
    ensure(db, cfg, &json!({}))
}

/// Built-in standing instructions; maintained as markdown next to this file.
const INSTRUCTIONS: &str = include_str!("overlord.md");

/// The `rlord-tidy` subagent: upkeep passes run in their own context.
const TIDY_AGENT: &str = include_str!("tidy.md");

/// Every agent's project-instructions file name. The overlord gets the same
/// text under each, so whichever agent runs it reads them, alongside the
/// user's own global file (`~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`, …).
const CONTEXT_FILES: &[&str] = &["CLAUDE.md", "AGENTS.md", "GEMINI.md"];

/// Header on the generated files so nobody edits a file that gets rewritten.
const GENERATED_NOTE: &str = "<!-- Generated by rlord on every overlord start; edits here are lost.\n     Add your own standing instructions to ~/.config/robot-overlord/overlord.md\n     (appended below). -->\n\n";

#[cfg(test)]
mod tests {
    use super::*;

    /// Every MCP tool must be explained in the overlord's instructions or its
    /// subagents' (see the repo's CLAUDE.md): a tool it isn't told about goes
    /// unused.
    #[test]
    fn instructions_mention_every_tool() {
        let tools = crate::mcp::tools();
        let missing: Vec<&str> = tools
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .filter(|n| {
                let q = format!("`{n}`");
                !INSTRUCTIONS.contains(&q) && !TIDY_AGENT.contains(&q)
            })
            .collect();
        assert!(
            missing.is_empty(),
            "add to crates/daemon/src/overlord.md: {missing:?}"
        );
    }

    /// The tidy subagent may only use rlord tools that exist.
    #[test]
    fn tidy_agent_tools_exist() {
        let names: Vec<String> = crate::mcp::tools()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str().map(|n| format!("mcp__rlord__{n}")))
            .collect();
        let line = TIDY_AGENT
            .lines()
            .find_map(|l| l.strip_prefix("tools:"))
            .expect("tools line");
        for t in line.split(',').map(str::trim) {
            assert!(names.iter().any(|n| n == t), "unknown tool in tidy.md: {t}");
        }
    }
}
