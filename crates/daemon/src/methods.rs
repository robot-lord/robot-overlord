//! RPC method implementations. All synchronous; called under the Db mutex via
//! `Daemon::call`. tmux operations are subprocess calls in the low-ms range.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use overlord_core::agents::claude;
use overlord_core::db::{Db, NewSession, SessionQuery};
use overlord_core::tmux;
use overlord_core::types::{new_id, now_ms, Status};

pub fn session_list(db: &Db, params: &Value) -> Result<Value> {
    let q: SessionQuery =
        serde_json::from_value(params.clone()).context("invalid session query")?;
    Ok(serde_json::to_value(db.sessions(&q)?)?)
}

pub fn session_unarchive(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    db.set_archived(&s.id, None)?;
    db.insert_event(&s.id, now_ms(), "unarchived", "{}")?;
    Ok(json!({"unarchived": s.id}))
}

pub fn session_rename(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    let name = p_str(params, "name")?;
    if name.trim().is_empty() {
        bail!("name must not be empty");
    }
    let old = s.name.clone();
    db.rename(&s.id, name.trim())?;
    db.insert_event(
        &s.id,
        now_ms(),
        "renamed",
        &json!({"from": old, "to": name.trim()}).to_string(),
    )?;
    Ok(json!({"renamed": s.id, "name": name.trim()}))
}

pub fn session_tag(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    let as_list = |key: &str| -> Vec<String> {
        params
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    for t in as_list("add") {
        db.add_tag(&s.id, &t)?;
    }
    for t in as_list("remove") {
        db.remove_tag(&s.id, &t)?;
    }
    Ok(serde_json::to_value(db.resolve_ref(&s.id)?)?)
}

pub fn session_pin(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    let pinned = params
        .get("pinned")
        .and_then(Value::as_bool)
        .context("missing bool param 'pinned'")?;
    db.set_pinned(&s.id, pinned)?;
    Ok(json!({"id": s.id, "pinned": pinned}))
}

pub fn session_group(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    let group = p_str(params, "group")?;
    db.set_group(&s.id, group)?;
    Ok(json!({"id": s.id, "group": group}))
}

fn p_str<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string param '{key}'"))
}

fn overlord_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "overlord".to_string())
}

fn pane_env(session_id: &str) -> Vec<(String, String)> {
    vec![("OVERLORD_SESSION_ID".to_string(), session_id.to_string())]
}

pub fn session_new(db: &Db, params: &Value) -> Result<Value> {
    let agent = params
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or("claude");
    if agent != "claude" {
        bail!("agent '{agent}' not supported yet (phase 1 is claude-only)");
    }
    let project =
        std::fs::canonicalize(p_str(params, "project")?).context("project path does not exist")?;
    let project = project
        .to_str()
        .context("non-utf8 project path")?
        .to_string();
    let prompt = params.get("prompt").and_then(Value::as_str);

    let id = new_id();
    let name = match params.get("name").and_then(Value::as_str) {
        Some(n) => n.to_string(),
        None => {
            let base = std::path::Path::new(&project)
                .file_name()
                .and_then(|b| b.to_str())
                .unwrap_or("session");
            format!("{}-{}", base, &id[id.len() - 4..])
        }
    };
    let claude_sid = claude::mint_session_id();
    let argv = claude::launch_argv(&claude_sid, prompt);
    let tmux_name = tmux::session_name(&id);
    let launch_spec = json!({"argv_base": ["claude"], "prompt": prompt}).to_string();
    let now = now_ms();

    db.insert_session(&NewSession {
        id: &id,
        name: &name,
        agent,
        agent_session: Some(&claude_sid),
        project_path: &project,
        tmux_session: &tmux_name,
        launch_spec: &launch_spec,
        created_at: now,
    })?;
    db.upsert_transcript_source(
        &id,
        claude::transcript_path(&project, &claude_sid)
            .to_str()
            .unwrap_or(""),
    )?;

    let cmd = tmux::pane_command(&pane_env(&id), &argv);
    tmux::new_session(&tmux_name, &project, &cmd)?;
    db.insert_event(&id, now, "launched", &json!({"argv": argv}).to_string())?;
    tracing::info!(session = %id, name = %name, "launched claude session");

    let s = db.resolve_ref(&id)?;
    Ok(serde_json::to_value(s)?)
}

pub fn session_get(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    Ok(serde_json::to_value(s)?)
}

pub fn session_restart(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    if s.agent != "claude" {
        bail!("restart not supported for agent '{}'", s.agent);
    }
    let sid = s
        .agent_session
        .as_deref()
        .context("session has no agent session id; cannot resume")?;
    let argv = claude::resume_argv(sid, &s.agent_session_trust, &s.project_path)?;
    let tmux_name = s
        .tmux_session
        .clone()
        .unwrap_or_else(|| tmux::session_name(&s.id));
    let cmd = tmux::pane_command(&pane_env(&s.id), &argv);
    let resumed = argv.iter().any(|a| a == "--resume");
    if tmux::has_session(&tmux_name) {
        tmux::respawn_pane(&tmux_name, &s.project_path, &cmd)?;
    } else {
        tmux::new_session(&tmux_name, &s.project_path, &cmd)?;
    }
    let now = now_ms();
    db.set_status(&s.id, Status::Starting, now)?;
    db.insert_event(
        &s.id,
        now,
        if resumed { "resumed" } else { "restarted" },
        &json!({"argv": argv}).to_string(),
    )?;
    tracing::info!(session = %s.id, resumed, "restarted session");
    Ok(serde_json::to_value(db.resolve_ref(&s.id)?)?)
}

pub fn session_stop(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    if let Some(t) = &s.tmux_session {
        tmux::kill_session(t)?;
    }
    let now = now_ms();
    db.set_status(&s.id, Status::Stopped, now)?;
    db.insert_event(&s.id, now, "stopped", "{}")?;
    Ok(json!({"stopped": s.id}))
}

pub fn session_archive(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    if let Some(t) = &s.tmux_session {
        tmux::kill_session(t)?;
    }
    let now = now_ms();
    db.set_status(&s.id, Status::Stopped, now)?;
    db.set_archived(&s.id, Some(now))?;
    db.insert_event(&s.id, now, "archived", "{}")?;
    Ok(json!({"archived": s.id}))
}

pub fn session_log(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    let limit = params.get("limit").and_then(Value::as_i64).unwrap_or(50);
    Ok(serde_json::to_value(db.list_events(&s.id, limit)?)?)
}

/// Ingest one agent hook event (from `overlord hook emit`, live or spooled).
pub fn hook_emit(db: &Db, params: &Value) -> Result<Value> {
    let instance = p_str(params, "instance")?;
    let agent = p_str(params, "agent")?;
    let event = p_str(params, "event")?;
    let payload = params.get("payload").cloned().unwrap_or(json!({}));
    let Ok(s) = db.resolve_ref(instance) else {
        // Env var pointed at a session we don't know (deleted?); drop quietly.
        tracing::debug!(instance, "hook event for unknown session");
        return Ok(json!({"ignored": true}));
    };
    let now = now_ms();

    if agent == "claude" {
        if let Some(sid) = payload.get("session_id").and_then(Value::as_str) {
            match s.agent_session.as_deref() {
                None => db.set_agent_session(&s.id, sid, "hook")?,
                Some(cur) if cur != sid => {
                    // Claude is the truth about what it's writing; adopt, but log it.
                    db.insert_event(
                        &s.id,
                        now,
                        "agent_session_changed",
                        &json!({"from": cur, "to": sid}).to_string(),
                    )?;
                    db.set_agent_session(&s.id, sid, "hook")?;
                }
                Some(_) if s.agent_session_trust == "minted" => {
                    db.set_agent_session(&s.id, sid, "hook")? // confirmed
                }
                _ => {}
            }
        }
        if let Some(tp) = payload.get("transcript_path").and_then(Value::as_str) {
            db.upsert_transcript_source(&s.id, tp)?;
        }
        let (kind, status) = claude::map_hook_event(event);
        match kind {
            "prompt_submitted" => db.touch_last_user_msg(&s.id, now)?,
            "agent_stopped" => db.touch_last_agent_msg(&s.id, now)?,
            _ => {}
        }
        if let Some(st) = status {
            db.set_status(&s.id, st, now)?;
        }
        // Keep the log light: store the event name, not the full payload.
        db.insert_event(&s.id, now, kind, &json!({"hook": event}).to_string())?;
    } else {
        db.insert_event(
            &s.id,
            now,
            "hook_other",
            &json!({"agent": agent, "hook": event}).to_string(),
        )?;
    }
    Ok(json!({"ok": true}))
}

/// Startup reconcile: sessions marked running whose tmux is gone are stopped
/// (resumable). Never auto-revives — crash vs. intentional kill is ambiguous.
pub fn reconcile(db: &Db) -> Result<()> {
    for s in db.running_sessions()? {
        let alive = s
            .tmux_session
            .as_deref()
            .map(tmux::has_session)
            .unwrap_or(false);
        if !alive {
            let now = now_ms();
            db.set_status(&s.id, Status::Stopped, now)?;
            db.insert_event(&s.id, now, "reconciled_dead", "{}")?;
            tracing::info!(session = %s.id, "tmux gone; marked stopped (resumable)");
        }
    }
    Ok(())
}

/// The hook command installed into Claude settings: absolute path, so hooks
/// work regardless of PATH.
pub fn hook_command() -> String {
    format!("{} hook emit claude", overlord_bin())
}
