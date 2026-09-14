//! RPC method implementations. All synchronous; called under the Db mutex via
//! `Daemon::call`. tmux operations are subprocess calls in the low-ms range.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::claude;
use rlord_core::config::{expand_tilde, Config};
use rlord_core::db::{Db, NewSession, SessionQuery};
use rlord_core::types::{new_id, now_ms, Status};
use rlord_core::{git, tmux};

/// Known repos for the picker: the configured default first (even if never
/// used), then most-recently-used.
pub fn repo_list(db: &Db, cfg: &Config) -> Result<Value> {
    let default = cfg.session.default_repo_path();
    let mut out: Vec<Value> = Vec::new();
    if let Some(d) = &default {
        out.push(json!({"path": d, "default": true, "last_used_at": null}));
    }
    for r in db.list_repos()? {
        if !std::path::Path::new(&r.path).is_dir() {
            continue; // deleted since; don't offer a dead path
        }
        if default.as_deref() == Some(r.path.as_str()) {
            out[0]["last_used_at"] = json!(r.last_used_at);
            continue;
        }
        out.push(json!({"path": r.path, "default": false, "last_used_at": r.last_used_at}));
    }
    Ok(Value::Array(out))
}

/// Everything the repo finder can offer: known repos (MRU, default first)
/// followed by repos discovered under `repo_roots`, deduplicated.
pub fn repo_find(db: &Db, cfg: &Config, scanned: Vec<String>) -> Result<Value> {
    let mut out = repo_list(db, cfg)?;
    let known: std::collections::HashSet<String> = out
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["path"].as_str().map(str::to_string))
        .collect();
    for p in scanned {
        if !known.contains(&p) {
            out.as_array_mut()
                .unwrap()
                .push(json!({"path": p, "default": false, "last_used_at": null}));
        }
    }
    let roots: Vec<String> = cfg
        .session
        .repo_roots
        .iter()
        .map(|r| expand_tilde(r))
        .collect();
    Ok(json!({"repos": out, "roots": roots, "depth": cfg.session.repo_scan_depth}))
}

pub fn session_defaults(cfg: &Config) -> Result<Value> {
    Ok(serde_json::to_value(&cfg.session)?)
}

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

pub fn rlord_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "rlord".to_string())
}

fn pane_env(session_id: &str) -> Vec<(String, String)> {
    vec![("RLORD_SESSION_ID".to_string(), session_id.to_string())]
}

pub fn session_new(db: &Db, cfg: &Config, params: &Value) -> Result<Value> {
    let p_opt = |k: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    let agent = p_opt("agent").unwrap_or_else(|| cfg.session.agent.clone());
    if agent != "claude" {
        bail!("agent '{agent}' not supported yet (claude only)");
    }
    let model = p_opt("model").or_else(|| {
        let m = cfg.session.model.trim();
        (!m.is_empty()).then(|| m.to_string())
    });
    let prompt = p_opt("prompt");

    // Where the session lives: an explicit repo, an explicit plain folder,
    // or the configured default repo.
    let (repo, project): (Option<String>, String) = if let Some(r) = p_opt("repo") {
        let r = expand_tilde(&r);
        let root =
            git::main_repo_root(&r).with_context(|| format!("{r} is not a git repository"))?;
        (Some(root.clone()), root)
    } else if let Some(p) = p_opt("project") {
        let p = std::fs::canonicalize(expand_tilde(&p)).context("project path does not exist")?;
        let p = p.to_str().context("non-utf8 project path")?.to_string();
        (git::main_repo_root(&p), p)
    } else if let Some(d) = cfg.session.default_repo_path() {
        let root = git::main_repo_root(&d)
            .with_context(|| format!("default_repo {d} is not a git repository"))?;
        (Some(root.clone()), root)
    } else {
        bail!("no repo or project given and no default_repo configured");
    };

    let id = new_id();
    let name = p_opt("name").unwrap_or_else(|| {
        let base = std::path::Path::new(&project)
            .file_name()
            .and_then(|b| b.to_str())
            .unwrap_or("session");
        format!("{}-{}", base, &id[id.len() - 4..])
    });

    // Workspace: a dedicated worktree, on by default, only when there's a repo.
    let want_ws = params
        .get("workspace")
        .and_then(Value::as_bool)
        .unwrap_or(cfg.session.workspace);
    let (project_path, workspace_path, branch) = match (&repo, want_ws) {
        (Some(repo), true) => {
            let branch = p_opt("branch").unwrap_or_else(|| git::sanitize_branch(&name));
            let dir = git::workspace_path(repo, &cfg.session.workspace_dir, &branch);
            git::ensure_excluded(repo, &cfg.session.workspace_dir)?;
            git::worktree_add(repo, &branch, &dir)?;
            let dir = dir.to_string_lossy().into_owned();
            (dir.clone(), Some(dir), Some(branch))
        }
        _ => (project, None, None),
    };

    let claude_sid = claude::mint_session_id();
    let argv = claude::launch_argv(&claude_sid, prompt.as_deref(), model.as_deref());
    let tmux_name = tmux::session_name(&id);
    let launch_spec = json!({"agent": agent, "model": model, "prompt": prompt}).to_string();
    let now = now_ms();

    db.insert_session(&NewSession {
        id: &id,
        name: &name,
        agent: &agent,
        agent_session: Some(&claude_sid),
        project_path: &project_path,
        tmux_session: &tmux_name,
        launch_spec: &launch_spec,
        created_at: now,
        repo_path: repo.as_deref(),
        workspace_path: workspace_path.as_deref(),
        branch: branch.as_deref(),
    })?;
    if let Some(r) = &repo {
        db.touch_repo(r, now)?;
    }
    db.upsert_transcript_source(
        &id,
        claude::transcript_path(&project_path, &claude_sid)
            .to_str()
            .unwrap_or(""),
    )?;

    let cmd = tmux::pane_command(&pane_env(&id), &argv);
    tmux::new_session(&tmux_name, &project_path, &cmd)?;
    db.insert_event(
        &id,
        now,
        "launched",
        &json!({"argv": argv, "workspace": workspace_path, "branch": branch}).to_string(),
    )?;
    tracing::info!(session = %id, name = %name, workspace = ?workspace_path, "launched claude session");

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
    let spec: Value = serde_json::from_str(&db.launch_spec(&s.id)?).unwrap_or(json!({}));
    let model = spec
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let argv = claude::resume_argv(
        sid,
        &s.agent_session_trust,
        &s.project_path,
        model.as_deref(),
    )?;
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

/// Ingest one agent hook event (from `rlord hook emit`, live or spooled).
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
    format!("{} hook emit claude", rlord_bin())
}
