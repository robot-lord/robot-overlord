//! RPC method implementations. All synchronous; called under the Db mutex via
//! `Daemon::call`. tmux operations are subprocess calls in the low-ms range.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::{claude, install, transcript, Agent, Effect};
use rlord_core::config::{expand_tilde, Config};
use rlord_core::db::{Db, NewSession, SessionQuery};
use rlord_core::types::{new_id, now_ms, Status, Subagent};
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

/// `[session]` as configured, plus what each agent offers: `agents` lists
/// every agent rlord can run with its default model and picker choices
/// (`models` empty: no catalog, the model is typed in).
pub fn session_defaults(cfg: &Config) -> Result<Value> {
    let mut v = serde_json::to_value(&cfg.session)?;
    v["agents"] = install::status(false)
        .into_iter()
        .map(|st| {
            let agent = Agent::parse(st.name);
            json!({
                "name": st.name, "label": st.label, "installed": st.installed,
                // false: rlord knows it but can't run sessions on it yet.
                "supported": st.supported,
                // How to install it (the user runs it; rlord never does).
                "install": st.install,
                "model": agent.map(|a| cfg.session.model_for(a)).unwrap_or(""),
                "models": agent.map(|a| cfg.session.models_for(a)).unwrap_or_default(),
            })
        })
        .collect();
    Ok(v)
}

/// `agents.list`: every coding agent rlord knows, whether it's installed
/// (and its version), whether rlord's hooks are in place (and, for Codex,
/// trusted), and the install command to suggest when it's missing. Plain
/// look-ups; rlord never installs anything.
pub fn agents_list() -> Result<Value> {
    Ok(json!({
        "agents": install::status(true),
        "note": "rlord never installs an agent: give the user `install.command` to run themselves; \
                 after installing, `rlord install-hooks` (and for Codex, trust them with /hooks)",
    }))
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

/// The overlord's row is fixed: always on top, never renamed, grouped,
/// described, annotated or archived. Its meta-context is its instructions.
fn refuse_overlord(s: &rlord_core::types::Session, what: &str) -> Result<()> {
    if crate::overlord::is_overlord_role(&s.role) {
        bail!("the overlord can't be {what}");
    }
    Ok(())
}

pub fn session_rename(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    refuse_overlord(&s, "renamed")?;
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
    // Its tmux session follows the name.
    let s = db.resolve_ref(&s.id)?;
    if let Err(e) = sync_tmux_name(db, &s) {
        tracing::warn!(error = %e, "renaming tmux session");
    }
    let s = db.resolve_ref(&s.id)?;
    if let Some(t) = s.tmux_session.as_deref().filter(|t| tmux::has_session(t)) {
        if let Err(e) = tmux::name_agent_window(t, name.trim()) {
            tracing::debug!(error = %e, "renaming tmux window");
        }
    }
    Ok(json!({"renamed": s.id, "name": name.trim(), "tmux": s.tmux_session}))
}

pub fn session_tag(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    let add = str_list(params, "add");
    // Removing stays allowed, so tags it got before its row was fixed can go.
    if !add.is_empty() {
        refuse_overlord(&s, "tagged (it isn't in any group)")?;
    }
    for t in add {
        db.add_tag(&s.id, &t)?;
    }
    for t in str_list(params, "remove") {
        db.remove_tag(&s.id, &t)?;
    }
    Ok(serde_json::to_value(db.resolve_ref(&s.id)?)?)
}

/// Tags are rlord's groups: `{groups: [{tag, count}], untagged}` over live
/// sessions.
pub fn tag_list(db: &Db) -> Result<Value> {
    let groups: Vec<Value> = db
        .tag_counts()?
        .into_iter()
        .map(|(tag, count)| json!({"tag": tag, "count": count}))
        .collect();
    let untagged = db.untagged_count()?;
    Ok(json!({"groups": groups, "untagged": untagged, "order": db.group_order()?}))
}

/// The favourite groups, in order: `{}` reads, `{order: [tags]}` replaces.
pub fn tag_order(db: &Db, params: &Value) -> Result<Value> {
    let order = match params.get("order").and_then(Value::as_array) {
        Some(a) => {
            let v: Vec<String> = a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            db.set_group_order(&v)?
        }
        None => db.group_order()?,
    };
    Ok(json!({"order": order}))
}

/// Rename a group (tag) on every session.
pub fn tag_rename(db: &Db, params: &Value) -> Result<Value> {
    let from = p_str(params, "from")?.trim();
    let to = p_str(params, "to")?.trim();
    if to.is_empty() || to.contains(char::is_whitespace) {
        bail!("a tag is one word (no spaces)");
    }
    let n = db.rename_tag(from, to)?;
    Ok(json!({"from": from, "to": to, "sessions": n}))
}

pub fn session_describe(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    refuse_overlord(&s, "given a description")?;
    let d = params
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    db.set_description(&s.id, d)?;
    db.insert_event(
        &s.id,
        now_ms(),
        "described",
        &json!({"description": d}).to_string(),
    )?;
    Ok(serde_json::to_value(db.resolve_ref(&s.id)?)?)
}

pub fn session_notes(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    refuse_overlord(&s, "given notes")?;
    let n = params.get("notes").and_then(Value::as_str).unwrap_or("");
    db.set_notes(&s.id, n.trim_end())?;
    db.insert_event(
        &s.id,
        now_ms(),
        "notes_edited",
        &json!({"bytes": n.len()}).to_string(),
    )?;
    Ok(serde_json::to_value(db.resolve_ref(&s.id)?)?)
}

pub fn session_pin(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    let pinned = params
        .get("pinned")
        .and_then(Value::as_bool)
        .context("missing bool param 'pinned'")?;
    // Unpinning stays allowed, to clear a pin from before its row was fixed.
    if pinned {
        refuse_overlord(&s, "pinned (it always sits on top)")?;
    }
    db.set_pinned(&s.id, pinned)?;
    Ok(json!({"id": s.id, "pinned": pinned}))
}

pub fn session_group(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    let group = p_str(params, "group")?;
    db.set_group(&s.id, group)?;
    Ok(json!({"id": s.id, "group": group}))
}

/// A session's agent flags from its launch spec: the adapter flags it was
/// created with, plus (Claude) `--remote-control <name>` while Remote
/// Control is on (so every start, resume and fresh start reconnects it).
/// Turned off, it stays off with a settings override: `--resume` would
/// otherwise reconnect a conversation that was last connected.
pub(crate) fn spec_args(agent: Agent, spec: &Value) -> Vec<String> {
    let mut a = str_list(spec, "extra_args");
    if agent != Agent::Claude {
        return a;
    }
    // One --settings override for everything rlord sets.
    let mut settings = serde_json::Map::new();
    if let Some(n) = spec["remote_control"].as_str().filter(|n| !n.is_empty()) {
        a.push("--remote-control".into());
        a.push(n.to_string());
    } else if spec["remote_control_off"] == true {
        settings.insert("disableRemoteControl".into(), json!(true));
    }
    // The renderer (`[session] claude_tui`): inline keeps tmux's scrollback.
    let tui = CLAUDE_TUI.get().map(String::as_str).unwrap_or("").trim();
    if !tui.is_empty() {
        settings.insert("tui".into(), json!(tui));
    }
    if !settings.is_empty() {
        a.push("--settings".into());
        a.push(Value::Object(settings).to_string());
    }
    a
}

/// `[session] claude_tui`, set once at daemon start (see `set_claude_tui`).
static CLAUDE_TUI: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record the configured Claude renderer for every launch.
pub fn set_claude_tui(cfg: &Config) {
    let _ = CLAUDE_TUI.set(cfg.session.claude_tui.clone());
}

/// A string-array param (missing / wrong type = empty).
pub(crate) fn str_list(params: &Value, key: &str) -> Vec<String> {
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
}

pub(crate) fn p_str<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string param '{key}'"))
}

/// Absolute path used for hook commands and tmux bindings. Prefers the path
/// we were invoked through when it is a PATH entry or symlink that resolves to
/// this executable (e.g. `~/.local/bin/rlord` → `target/debug/rlord`): that
/// path stays valid across rebuilds, repo moves, and `cargo install`.
pub fn rlord_bin() -> String {
    let exe = std::env::current_exe().ok();
    let real = exe.as_ref().and_then(|e| std::fs::canonicalize(e).ok());
    if let Some(arg0) = std::env::args_os().next() {
        let arg0 = std::path::PathBuf::from(arg0);
        let candidate = if arg0.components().count() == 1 {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|d| d.join(&arg0))
                    .find(|c| c.is_file())
            })
        } else {
            Some(arg0)
        };
        if let (Some(c), Some(real)) = (candidate, &real) {
            if c.is_absolute() && std::fs::canonicalize(&c).ok().as_ref() == Some(real) {
                return c.to_string_lossy().into_owned();
            }
        }
    }
    exe.and_then(|p| p.to_str().map(String::from))
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
    let agent = Agent::named(&p_opt("agent").unwrap_or_else(|| cfg.session.agent.clone()))?;
    // Internal-only knobs (the overlord uses them; the MCP surface never
    // forwards them): adapter flags that must survive restarts, a role, and a
    // fixed tmux name.
    let extra_args = str_list(params, "extra_args");
    let role = p_opt("role").unwrap_or_default();
    if crate::overlord::is_overlord_role(&role) {
        agent.overlord_seat()?;
    }
    // Not installed: say how to get it, rather than fail in the pane.
    if let Some(why) = install::missing(agent) {
        bail!("{why}");
    }
    let remote_control = params["remote_control"].as_bool().unwrap_or(false);
    if remote_control && !agent.has_remote_control() {
        bail!(
            "Remote Control is a Claude Code feature; {} has none",
            agent.label()
        );
    }
    // Each agent has its own models: the default is that agent's.
    let model = p_opt("model").or_else(|| {
        let m = cfg.session.model_for(agent).trim();
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

    // Remote Control from the start (the caller checked it's available).
    let remote = remote_control.then(|| crate::remote::remote_name(&name, &role));
    let mut spec = json!({"agent": agent.name(), "model": model, "prompt": prompt,
                          "extra_args": extra_args});
    if let Some(n) = &remote {
        spec["remote_control"] = json!(n);
    }
    let launch = agent.launch(
        &project_path,
        prompt.as_deref(),
        model.as_deref(),
        &spec_args(agent, &spec),
    );
    let argv = &launch.argv;
    let tmux_name = match p_opt("tmux_name") {
        Some(n) => n,
        None => tmux_name_for(db, &name, None),
    };
    let launch_spec = spec.to_string();
    let now = now_ms();

    db.insert_session(&NewSession {
        id: &id,
        name: &name,
        agent: agent.name(),
        agent_session: launch.session_id.as_deref(),
        project_path: &project_path,
        tmux_session: Some(&tmux_name),
        launch_spec: &launch_spec,
        created_at: now,
        repo_path: repo.as_deref(),
        workspace_path: workspace_path.as_deref(),
        branch: branch.as_deref(),
        role: &role,
    })?;
    if let Some(r) = &repo {
        db.touch_repo(r, now)?;
    }
    if let Some(t) = &launch.transcript {
        db.upsert_transcript_source(&id, &t.to_string_lossy())?;
    }
    if !agent.mints_id() {
        // Its conversation is bound once the agent reports it (or it turns
        // up in the agent's own index): one started from now on.
        db.kv_set(&bind_since_key(&id), &now.to_string())?;
        db.kv_set(&bind_prompt_key(&id), prompt.as_deref().unwrap_or(""))?;
    }

    let cmd = tmux::pane_command(&pane_env(&id), argv);
    tmux::new_session(&tmux_name, &project_path, &cmd)?;
    db.insert_event(
        &id,
        now,
        "launched",
        &json!({"argv": argv, "workspace": workspace_path, "branch": branch}).to_string(),
    )?;
    tracing::info!(session = %id, name = %name, agent = agent.name(), workspace = ?workspace_path, "launched session");

    let s = db.resolve_ref(&id)?;
    Ok(serde_json::to_value(s)?)
}

/// Add an importable conversation (see `import::import`, which finds it and
/// recreates its worktree outside the DB lock) as a stopped session that
/// resumes it. `recreated`: its missing folder was just recreated.
pub(crate) fn import_candidate(
    db: &Db,
    c: &rlord_core::import::Candidate,
    params: &Value,
    recreated: bool,
) -> Result<Value> {
    // Checked again here, under the lock: two imports of one conversation
    // may race through the unlocked scan.
    if let Some(s) = db.session_by_agent_session(&c.agent_session)? {
        bail!("already in rlord as '{}'", s.name);
    }
    let p_opt = |k: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let archive = params["archive"].as_bool().unwrap_or(false);
    if !c.dir_exists && !recreated {
        match () {
            _ if archive => {} // history only: it can't be resumed
            _ if c.recreatable => bail!(
                "{} is gone; pass recreate_worktree (branch {} still exists) or archive",
                c.cwd,
                c.branch.as_deref().unwrap_or("?")
            ),
            _ => bail!(
                "{} is gone and can't be recreated; pass archive to keep it as history only",
                c.cwd
            ),
        }
    }

    let id = new_id();
    let name = p_opt("name").map(str::to_string).unwrap_or_else(|| {
        let base = std::path::Path::new(&c.cwd)
            .file_name()
            .and_then(|b| b.to_str())
            .unwrap_or("session");
        format!("{}-{}", base, &id[id.len() - 4..])
    });
    let launch_spec =
        json!({"agent": c.agent, "model": null, "prompt": null, "extra_args": []}).to_string();
    let created_at = c.started_at.unwrap_or(c.last_active_at);
    db.insert_session(&NewSession {
        id: &id,
        name: &name,
        agent: &c.agent,
        agent_session: Some(&c.agent_session),
        project_path: &c.cwd,
        // Named on first resume (session_restart mints one).
        tmux_session: None,
        launch_spec: &launch_spec,
        created_at,
        repo_path: c.repo.as_deref(),
        workspace_path: c.worktree.then_some(c.cwd.as_str()),
        branch: c.branch.as_deref(),
        role: "",
    })?;
    // The id is the transcript's own file name: as authoritative as a hook
    // report, so restarts resume it (unlike a scavenged guess).
    db.set_agent_session(&id, &c.agent_session, "hook")?;
    db.set_status(&id, Status::Stopped, c.last_active_at)?;
    if let Some(t) = c.last_user_at {
        db.touch_last_user_msg(&id, t)?;
    }
    if let Some(t) = c.last_agent_at {
        db.touch_last_agent_msg(&id, t)?;
    }
    db.upsert_transcript_source(&id, &c.transcript)?;
    if let Some(v) = p_opt("description") {
        db.set_description(&id, v)?;
    }
    if let Some(v) = p_opt("notes") {
        db.set_notes(&id, v)?;
    }
    for t in str_list(params, "tags") {
        let t = t.trim().trim_start_matches('#');
        if !t.is_empty() {
            db.add_tag(&id, t)?;
        }
    }
    let now = now_ms();
    if archive {
        db.set_archived(&id, Some(now))?;
    }
    if let Some(r) = &c.repo {
        db.touch_repo(r, c.last_active_at)?;
    }
    db.insert_event(
        &id,
        now,
        "imported",
        &json!({"transcript": c.transcript, "entrypoint": c.entrypoint,
                "recreated_worktree": recreated})
        .to_string(),
    )?;
    tracing::info!(session = %id, agent_session = %c.agent_session, "imported session");

    let mut out = serde_json::to_value(db.resolve_ref(&id)?)?;
    if c.open_elsewhere || c.recently_written {
        out["warning"] = json!(
            "this conversation may be open elsewhere; close that copy before resuming it here, \
             or the two will fork the transcript"
        );
    }
    Ok(out)
}

/// A fresh `rlord-adjective-noun`, unused in the database and on tmux.
/// A tmux name for a session called `name` (see `names::tmux_name_for`),
/// not taken by another session or live tmux session; `own` is the
/// session's current one (free for itself).
fn tmux_name_for(db: &Db, name: &str, own: Option<&str>) -> String {
    // A DB error counts as free: worst case a duplicate name, which tmux's
    // own check still catches.
    // The seats' fixed names are reserved, even before the seats exist.
    let reserved =
        |n: &str| n == crate::overlord::FRONT.tmux || n == crate::overlord::MASTERMIND.tmux;
    rlord_core::names::tmux_name_for(name, |n| {
        Some(n) != own
            && (reserved(n) || db.tmux_name_taken(n).unwrap_or(false) || tmux::has_session(n))
    })
}

/// The tmux name a session should have: a seat's fixed one, else its name
/// plus `-rlord`.
fn wanted_tmux_name(db: &Db, s: &rlord_core::types::Session) -> Option<String> {
    if crate::overlord::is_overlord_role(&s.role) {
        return Some(crate::overlord::seat(&s.role).tmux.to_string());
    }
    let cur = s.tmux_session.as_deref();
    if cur.is_some_and(|c| rlord_core::names::is_tmux_name_for(c, &s.name)) {
        return None; // already named after it
    }
    Some(tmux_name_for(db, &s.name, cur))
}

/// Give a session the tmux name it should have, renaming its live tmux
/// session in place.
fn sync_tmux_name(db: &Db, s: &rlord_core::types::Session) -> Result<()> {
    let Some(new) = wanted_tmux_name(db, s) else {
        return Ok(());
    };
    let Some(old) = s.tmux_session.as_deref() else {
        return Ok(()); // gets one when it starts
    };
    if old == new {
        return Ok(());
    }
    if tmux::has_session(old) {
        if tmux::has_session(&new) {
            bail!("tmux session {new} already exists");
        }
        tmux::rename_session(old, &new)?;
    }
    db.set_tmux_session(&s.id, &new)?;
    tracing::info!(session = %s.id, from = %old, to = %new, "renamed tmux session");
    Ok(())
}

/// Name every session's tmux session after it (older ones had random or
/// `rlord_<id>` names). Idempotent.
pub fn sync_tmux_names(db: &Db) -> Result<()> {
    let all = db.sessions(&SessionQuery {
        archived: rlord_core::db::ArchivedMode::All,
        include_overlord: true,
        ..Default::default()
    })?;
    // The seats first: their fixed names must never be taken by another.
    let (seats, rest): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|s| crate::overlord::is_overlord_role(&s.role));
    for s in seats.into_iter().chain(rest) {
        if let Err(e) = sync_tmux_name(db, &s) {
            tracing::warn!(session = %s.id, error = %e, "renaming tmux session");
        }
        // Only the agent's pane stays when its program exits (older rlord
        // kept every pane in the window).
        if let Ok(s) = db.resolve_ref(&s.id) {
            if let Some(t) = s.tmux_session.as_deref().filter(|t| tmux::has_session(t)) {
                if let Err(e) = tmux::keep_agent_pane_on_exit(t) {
                    tracing::debug!(session = %s.id, error = %e, "remain-on-exit");
                }
            }
        }
    }
    Ok(())
}

pub fn session_get(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    Ok(serde_json::to_value(s)?)
}

pub fn session_restart(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    if s.archived_at.is_some() {
        bail!("{} is archived: unarchive it first", s.name);
    }
    let agent = Agent::named(&s.agent)?;
    let spec: Value = serde_json::from_str(&db.launch_spec(&s.id)?).unwrap_or(json!({}));
    let model = spec
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let (argv, resumed) = agent.resume(
        s.agent_session.as_deref(),
        &s.agent_session_trust,
        &s.project_path,
        model.as_deref(),
        &spec_args(agent, &spec),
    )?;
    let tmux_name = match s.tmux_session.clone() {
        Some(n) => n,
        None => {
            let n = tmux_name_for(db, &s.name, None);
            db.set_tmux_session(&s.id, &n)?;
            n
        }
    };
    let cmd = tmux::pane_command(&pane_env(&s.id), &argv);
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

/// Restart a session's agent on a **fresh conversation** in its pane (a new
/// conversation id, not a resume). The old conversation stays on disk and
/// can still be resumed with the agent's own `--resume`.
pub fn session_fresh(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    let agent = Agent::named(&s.agent)?;
    let spec: Value = serde_json::from_str(&db.launch_spec(&s.id)?).unwrap_or(json!({}));
    let model = spec.get("model").and_then(Value::as_str);
    let launch = agent.launch(&s.project_path, None, model, &spec_args(agent, &spec));
    let argv = &launch.argv;
    let tmux_name = s
        .tmux_session
        .clone()
        .context("session has no tmux session")?;
    let cmd = tmux::pane_command(&pane_env(&s.id), argv);
    let now = now_ms();
    if tmux::has_session(&tmux_name) {
        tmux::respawn_pane(&tmux_name, &s.project_path, &cmd)?;
    } else {
        tmux::new_session(&tmux_name, &s.project_path, &cmd)?;
    }
    match (&launch.session_id, &launch.transcript) {
        (Some(sid), transcript) => {
            db.set_agent_session(&s.id, sid, "minted")?;
            if let Some(t) = transcript {
                db.upsert_transcript_source(&s.id, &t.to_string_lossy())?;
            }
        }
        // The agent assigns the new id: nothing is bound until it reports
        // one started from now on.
        (None, _) => {
            db.clear_agent_session(&s.id)?;
            db.kv_set(&bind_since_key(&s.id), &now.to_string())?;
            db.kv_set(&bind_prompt_key(&s.id), "")?;
        }
    }
    db.set_status(&s.id, Status::Starting, now)?;
    db.insert_event(
        &s.id,
        now,
        "fresh_conversation",
        &json!({"from": s.agent_session, "to": launch.session_id}).to_string(),
    )?;
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
    refuse_overlord(&s, "archived (stop it instead)")?;
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
    // Events that fire often (every tool call, every notification) and
    // change nothing for this session: done, with no tmux call, no write
    // and no log line.
    let own_agent = Agent::parse(agent).filter(|a| a.name() == s.agent);
    if let Some(a) = own_agent {
        let (kind, effect) = a.map_hook_event(event, &payload);
        if matches!(kind, "tool" | "notification") && effect.apply(s.status).is_none() {
            return Ok(json!({"ok": true, "unchanged": true}));
        }
    }
    if let Some(why) = foreign_hook(&s, params) {
        // Same RLORD_SESSION_ID, another process (e.g. Claude's background
        // daemon forked the conversation out of the pane): it must not
        // retarget the session or change its state.
        tracing::info!(session = %s.id, why, "ignoring a hook from outside the session's pane");
        return Ok(json!({"ignored": why}));
    }

    // The hooks of the agent the session runs (another agent started by
    // hand in its pane inherits RLORD_SESSION_ID: logged, nothing more).
    if let Some(agent) = own_agent {
        let sid = payload.get("session_id").and_then(Value::as_str);
        let whose = sid.map_or(Whose::Ours, |sid| {
            // (Asked of the agent's index only when the ids differ.)
            let other = s.agent_session.as_deref().is_some_and(|cur| cur != sid);
            conversation(
                agent,
                event,
                s.agent_session.as_deref(),
                &s.agent_session_trust,
                sid,
                if other {
                    agent.is_user_conversation(sid)
                } else {
                    None
                },
            )
        });
        match (whose, sid) {
            (Whose::Bind, Some(sid)) => db.set_agent_session(&s.id, sid, "hook")?,
            // rlord's own claim (minted at launch, or read from the agent's
            // index), now confirmed by the agent.
            (Whose::Ours, Some(sid))
                if s.agent_session.is_some() && s.agent_session_trust == "minted" =>
            {
                db.set_agent_session(&s.id, sid, "hook")?
            }
            (Whose::Adopt, Some(sid)) => {
                // The agent is the truth about what it's writing; adopt, but log it.
                db.insert_event(
                    &s.id,
                    now,
                    "agent_session_changed",
                    &json!({"from": s.agent_session, "to": sid}).to_string(),
                )?;
                db.set_agent_session(&s.id, sid, "hook")?;
            }
            (Whose::Unbound, _) => {
                return Ok(json!({"ignored": "no conversation bound yet"}));
            }
            _ => {}
        }
        let (kind, effect) = agent.map_hook_event(event, &payload);
        // Who a "needs you" is for: a sub-agent (Claude names it; Codex's
        // has its own thread), else the session's own agent ("").
        let by = payload["agent_id"]
            .as_str()
            .or(sid.filter(|_| whose == Whose::Other))
            .unwrap_or("");
        let needs_you = effect == Effect::Set(Status::WaitingInput);
        if whose == Whose::Other {
            // Another conversation in the pane (a sub-agent's thread): it
            // never rebinds the session or ends its turn, but a prompt it
            // is blocked on is the user's to answer here.
            if !needs_you && effect != Effect::Resume {
                return Ok(json!({"ignored": "another conversation in the pane"}));
            }
        } else if let Some(tp) = payload.get("transcript_path").and_then(Value::as_str) {
            db.upsert_transcript_source(&s.id, tp)?;
        }
        // The agent moved on: only the one that was blocked can say so (a
        // parallel sub-agent finishing a tool doesn't answer the prompt).
        if effect == Effect::Resume
            && s.status == Status::WaitingInput
            && db.kv_get(&needs_you_key(&s.id))?.unwrap_or_default() != by
        {
            return Ok(json!({"ok": true, "unchanged": true}));
        }
        let status = effect.apply(s.status);
        if needs_you {
            db.kv_set(&needs_you_key(&s.id), by)?;
        }
        // Only a prompt and a finished turn are messages: a permission
        // prompt, a question or a tool call never moves "updated".
        match kind {
            "prompt_submitted" => db.touch_last_user_msg(&s.id, now)?,
            "agent_stopped" => db.touch_last_agent_msg(&s.id, now)?,
            _ => {}
        }
        // A late hook from an agent that is exiting after its session was
        // stopped (a kill, a reset) must not mark it alive again: a stopped
        // session only comes back if its tmux session really exists.
        let revives_dead = s.status == Status::Stopped
            && !s.tmux_session.as_deref().is_some_and(tmux::has_session);
        if let Some(st) = status.filter(|st| *st == Status::Stopped || !revives_dead) {
            db.set_status(&s.id, st, now)?;
        }
        if kind.starts_with("subagent_") && agent.tracks_subagents() {
            ingest_subagent_hook(db, &s.id, kind, &payload, now)?;
        } else {
            // Keep the log light: the event name only.
            db.insert_event(&s.id, now, kind, &json!({"hook": event}).to_string())?;
        }
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

/// kv key: who the session's "needs you" is for ("" its own agent, else a
/// sub-agent's id or thread).
fn needs_you_key(id: &str) -> String {
    format!("needs_you_by:{id}")
}

/// Whose conversation a hook event speaks for, relative to the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Whose {
    /// The session's bound conversation (or the event names none).
    Ours,
    /// Nothing was bound: this is the session's conversation from now on.
    Bind,
    /// The session moved to this conversation.
    Adopt,
    /// Another conversation in the pane (a sub-agent's thread).
    Other,
    /// Nothing is bound and this event can't bind: not the session's.
    Unbound,
}

/// Decide whose conversation `sid` is. `bound`/`trust`: the session's
/// current one and how it was learned. `user`: what the agent's own index
/// says about `sid` when it differs (Some(false): a sub-agent's thread).
fn conversation(
    agent: Agent,
    event: &str,
    bound: Option<&str>,
    trust: &str,
    sid: &str,
    user: Option<bool>,
) -> Whose {
    match bound {
        Some(cur) if cur == sid => Whose::Ours,
        // An agent that assigns its own id starts a conversation with a
        // SessionStart: until one arrives (after a launch or a fresh start)
        // an event is a late one from the process that was just replaced,
        // and must not bind its old conversation or stop the session.
        None if agent.mints_id() || agent.adopts_new_id_on(event) => Whose::Bind,
        None => Whose::Unbound,
        // A sub-agent's thread never becomes the session's.
        Some(_) if user == Some(false) => Whose::Other,
        // A new or resumed conversation in the pane.
        Some(_) if agent.adopts_new_id_on(event) => Whose::Adopt,
        // rlord's own guess from the agent's index, and the agent now says
        // otherwise: the hook is the authority.
        Some(_) if trust == "minted" && !agent.mints_id() => Whose::Adopt,
        Some(_) => Whose::Other,
    }
}

/// Why a hook isn't from the session's own agent process, if it isn't:
/// it ran outside rlord's tmux, or in another pane than the session's.
/// Hooks from older clients (no `pane` field) are taken as before.
fn foreign_hook(s: &rlord_core::types::Session, params: &Value) -> Option<&'static str> {
    let Some(pane) = params.get("pane")?.as_str() else {
        return Some("not in a tmux pane");
    };
    let socket = tmux::socket_name();
    if params["tmux"]
        .as_str()
        .is_some_and(|t| t.rsplit('/').next() != Some(socket.as_str()))
    {
        return Some("another tmux server");
    }
    let own = s
        .tmux_session
        .as_deref()
        .and_then(|n| tmux::pane_id(n).ok());
    match own {
        Some(own) if own != pane => Some("another pane"),
        // No live pane to compare with (stopped, starting): accept.
        _ => None,
    }
}

/// SubagentStart/SubagentStop → `subagents` rows. Claude's internal helper
/// agents (empty agent_type, no meta.json) are ignored. The on-disk meta file
/// supplies the description/tool-use id the hook payload lacks.
fn ingest_subagent_hook(
    db: &Db,
    session_id: &str,
    kind: &str,
    payload: &Value,
    now: i64,
) -> Result<()> {
    let agent_id = payload["agent_id"].as_str().unwrap_or("").to_string();
    if agent_id.is_empty() {
        return Ok(());
    }
    let agent_type = payload["agent_type"].as_str().unwrap_or("").to_string();
    let source = db.transcript_source(session_id)?;
    let from_disk = source
        .as_deref()
        // Only this hook's agent, not every sub-agent of the session.
        .map(|src| claude::scan_subagents_where(session_id, src, true, &|id| id == agent_id))
        .unwrap_or_default()
        .into_iter()
        .find(|a| a.agent_id == agent_id);
    if agent_type.is_empty() && from_disk.is_none() && !db.subagent_exists(&agent_id)? {
        return Ok(()); // internal helper (title/suggestion generators)
    }
    let mut a = from_disk.unwrap_or(Subagent {
        agent_id: agent_id.clone(),
        session_id: session_id.to_string(),
        agent_type: agent_type.clone(),
        description: String::new(),
        tool_use_id: None,
        spawn_depth: 1,
        background: false,
        started_at: None,
        finished_at: None,
        transcript_path: None,
        result: String::new(),
        input_tokens: 0,
        output_tokens: 0,
    });
    if !agent_type.is_empty() {
        a.agent_type = agent_type;
    }
    match kind {
        "subagent_started" => {
            a.started_at = Some(a.started_at.unwrap_or(now));
            a.finished_at = None;
        }
        _ => {
            a.finished_at = Some(now);
            if let Some(tp) = payload["agent_transcript_path"].as_str() {
                a.transcript_path = Some(tp.to_string());
                if let Some(st) = transcript::transcript_stats(std::path::Path::new(tp)) {
                    a.input_tokens = st.input_tokens;
                    a.output_tokens = st.output_tokens;
                }
            }
            if let Some(r) = payload["last_assistant_message"].as_str() {
                a.result = r.chars().take(4000).collect();
            }
        }
    }
    db.upsert_subagent(&a)?;
    db.insert_event(
        session_id,
        now,
        kind,
        &json!({"agent_id": a.agent_id, "agent_type": a.agent_type, "description": a.description})
            .to_string(),
    )?;
    Ok(())
}

/// Save transcript read positions that moved (so a restart reads forward).
pub(crate) fn save_cursors(db: &Db) {
    let dirty = transcript::take_dirty_cursors();
    if let Err(e) = db.save_transcript_cursors(&dirty) {
        tracing::warn!(error = %e, "saving transcript read positions");
    }
}

/// Startup reconcile: sessions marked running whose tmux is gone are stopped
/// (resumable). Never auto-revives — crash vs. intentional kill is ambiguous.
pub fn reconcile(db: &Db) -> Result<()> {
    // Sub-agents left "running" in sessions that have stopped (Claude didn't
    // record their end): close them.
    let closed = db.close_open_subagents(None, now_ms())?;
    if closed > 0 {
        tracing::info!(closed, "closed sub-agents of stopped sessions");
    }
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

/// kv key: when a session whose agent assigns its own conversation id was
/// last launched on a new conversation (ms). The conversation bound to it
/// must have started since.
pub(crate) fn bind_since_key(id: &str) -> String {
    format!("bind_since:{id}")
}

/// kv key: the prompt that conversation was launched with ("" for none):
/// how its thread is told from another started in the same folder.
pub(crate) fn bind_prompt_key(id: &str) -> String {
    format!("bind_prompt:{id}")
}

#[cfg(test)]
mod tests {

    #[test]
    fn remote_control_flags_follow_the_spec() {
        let on = json!({"extra_args": ["--x"], "remote_control": "web"});
        assert_eq!(
            spec_args(Agent::Claude, &on),
            ["--x", "--remote-control", "web"]
        );
        // Remote Control and the renderer are Claude's: not Codex flags.
        assert_eq!(spec_args(Agent::Codex, &on), ["--x"]);
        let off = json!({"remote_control_off": true});
        // (No renderer set in tests: CLAUDE_TUI is unset.)
        assert_eq!(
            spec_args(Agent::Claude, &off),
            ["--settings", r#"{"disableRemoteControl":true}"#]
        );
        assert!(spec_args(Agent::Claude, &json!({})).is_empty());
    }

    use super::*;

    #[test]
    fn whose_conversation_a_hook_speaks_for() {
        use Whose::*;
        let codex = |event: &str, bound: Option<&str>, trust: &str, user: Option<bool>| {
            conversation(Agent::Codex, event, bound, trust, "t-2", user)
        };
        assert_eq!(codex("Stop", Some("t-2"), "hook", None), Ours);
        // Nothing bound: a SessionStart binds; a late event from the
        // process just replaced (a fresh start) does not.
        assert_eq!(codex("SessionStart", None, "minted", None), Bind);
        assert_eq!(codex("SessionEnd", None, "minted", None), Unbound);
        assert_eq!(codex("Stop", None, "minted", None), Unbound);
        // A new thread in the pane is adopted, a sub-agent's never is.
        assert_eq!(codex("SessionStart", Some("t-1"), "hook", None), Adopt);
        assert_eq!(
            codex("SessionStart", Some("t-1"), "hook", Some(true)),
            Adopt
        );
        assert_eq!(
            codex("SessionStart", Some("t-1"), "hook", Some(false)),
            Other
        );
        assert_eq!(
            codex("SessionStart", Some("t-1"), "minted", Some(false)),
            Other
        );
        // Bound from the index (a guess): any hook corrects it.
        assert_eq!(codex("Stop", Some("t-1"), "minted", None), Adopt);
        assert_eq!(codex("Stop", Some("t-1"), "minted", Some(true)), Adopt);
        // Confirmed by a hook: another thread's Stop is another thread's.
        assert_eq!(codex("Stop", Some("t-1"), "hook", None), Other);
        // Claude chose its id and reports what it writes on every event.
        let claude = |event: &str, bound: Option<&str>| {
            conversation(Agent::Claude, event, bound, "minted", "t-2", None)
        };
        assert_eq!(claude("Stop", None), Bind);
        assert_eq!(claude("Stop", Some("t-1")), Adopt);
        assert_eq!(claude("Stop", Some("t-2")), Ours);
    }

    #[test]
    fn overlord_row_is_fixed() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&NewSession {
            id: "01o",
            name: "overlord",
            agent: "claude",
            agent_session: None,
            project_path: "/tmp/o",
            tmux_session: Some("rlord-test-overlord"),
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "overlord",
        })
        .unwrap();
        let r = json!({"ref": "01o"});
        let with = |k: &str, v: Value| {
            let mut p = r.clone();
            p[k] = v;
            p
        };
        // Each guard runs before any side effect (DB write or tmux call).
        for (what, res) in [
            ("rename", session_rename(&db, &with("name", json!("boss")))),
            ("pin", session_pin(&db, &with("pinned", json!(true)))),
            (
                "describe",
                session_describe(&db, &with("description", json!("x"))),
            ),
            ("notes", session_notes(&db, &with("notes", json!("x")))),
            ("tag", session_tag(&db, &with("add", json!(["wip"])))),
            ("archive", session_archive(&db, &r)),
        ] {
            let e = res.expect_err(what).to_string();
            assert!(e.starts_with("the overlord can't be"), "{what}: {e}");
        }
        // Clearing tags or a pin from before the row was fixed still works.
        session_tag(&db, &with("remove", json!(["wip"]))).unwrap();
        session_pin(&db, &with("pinned", json!(false))).unwrap();
        let s = db.resolve_ref("01o").unwrap();
        assert_eq!(s.name, "overlord");
        assert!(!s.pinned && s.tags.is_empty() && s.archived_at.is_none());
    }

    fn candidate(cwd: &str, dir_exists: bool) -> rlord_core::import::Candidate {
        rlord_core::import::Candidate {
            agent: "claude".into(),
            agent_session: "11111111-1111-4111-8111-111111111111".into(),
            transcript: "/x/t.jsonl".into(),
            cwd: cwd.into(),
            dir_exists,
            repo: Some("/repo".into()),
            branch: Some("feat/x".into()),
            worktree: true,
            recreatable: false,
            title: Some("Fix login".into()),
            first_prompt: Some("fix login".into()),
            last_prompt: None,
            last_reply: None,
            pr_url: None,
            entrypoint: Some("cli".into()),
            started_at: Some(1_000),
            last_active_at: 5_000,
            last_user_at: Some(4_000),
            last_agent_at: Some(5_000),
            open_elsewhere: true,
            recently_written: false,
        }
    }

    #[test]
    fn import_makes_a_stopped_resumable_session() {
        let db = Db::open_in_memory().unwrap();
        let c = candidate("/repo/.worktrees/feat-x", true);
        let p = json!({"name": "login-fix", "description": "fixing login",
                       "tags": ["#web"], "notes": "PR #7"});
        let out = import_candidate(&db, &c, &p, false).unwrap();
        assert!(out["warning"].is_string(), "open elsewhere must warn");
        let s = db.resolve_ref("login-fix").unwrap();
        assert_eq!(s.status, Status::Stopped);
        assert_eq!(s.agent_session.as_deref(), Some(c.agent_session.as_str()));
        // Trusted, so a restart resumes it (scavenged ids never resume).
        assert_eq!(s.agent_session_trust, "hook");
        assert_eq!(s.tmux_session, None);
        assert_eq!(s.project_path, c.cwd);
        assert_eq!(s.workspace_path.as_deref(), Some(c.cwd.as_str()));
        assert_eq!(s.branch.as_deref(), Some("feat/x"));
        assert_eq!(s.tags, ["web"]);
        assert_eq!(
            (s.description.as_str(), s.notes.as_str()),
            ("fixing login", "PR #7")
        );
        assert_eq!(s.created_at, 1_000);
        assert_eq!(s.last_user_msg_at, Some(4_000));
        assert_eq!(s.last_agent_msg_at, Some(5_000));
        assert!(s.archived_at.is_none());
        assert_eq!(
            db.session_by_agent_session(&c.agent_session)
                .unwrap()
                .map(|s| s.id),
            Some(s.id)
        );
    }

    #[test]
    fn import_of_a_gone_folder_needs_archive() {
        let db = Db::open_in_memory().unwrap();
        let c = candidate("/nonexistent/rlord-import-test", false);
        let e = import_candidate(&db, &c, &json!({}), false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("archive"), "{e}");
        assert!(
            db.agent_session_ids().unwrap().is_empty(),
            "nothing written on refusal"
        );
        import_candidate(&db, &c, &json!({"archive": true}), false).unwrap();
        let again = import_candidate(&db, &c, &json!({"archive": true}), false);
        assert!(again.unwrap_err().to_string().contains("already in rlord"));
        let s = db
            .session_by_agent_session(&c.agent_session)
            .unwrap()
            .unwrap();
        assert!(s.archived_at.is_some());
    }
}
