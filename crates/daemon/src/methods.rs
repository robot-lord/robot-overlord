//! RPC method implementations. All synchronous; called under the Db mutex via
//! `Daemon::call`. tmux operations are subprocess calls in the low-ms range.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::{claude, codex, install, transcript, Agent, Effect, Indexed};
use rlord_core::config::{expand_tilde, Config};
use rlord_core::db::{Db, NewSession, SessionQuery};
use rlord_core::types::{new_id, now_ms, Session, Status, Subagent};
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
                // Installed, but a version rlord can't run: why.
                "too_old": st.too_old,
                // rlord follows it through hooks it installs (false: none).
                "hooks": agent.is_some_and(Agent::uses_hooks),
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
                 after installing Claude Code or Codex, `rlord install-hooks` (and for Codex, \
                 trust them with /hooks); OpenCode needs no hooks",
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
/// otherwise reconnect a conversation that was last connected. (Codex)
/// where the conversation runs: in the pane (`--no-daemon`), or on Codex's
/// shared background server (`codex_host`, see `codex_daemon`).
pub(crate) fn spec_args(agent: Agent, spec: &Value) -> Vec<String> {
    let mut a = str_list(spec, "extra_args");
    if agent == Agent::Codex {
        let mut host = codex::host_args(codex::on_daemon(spec));
        host.append(&mut a);
        return host;
    }
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

/// Make the pane run the agent the "is it installed" check found: when its
/// command isn't on the PATH rlord's tmux panes get (the daemon's), but the
/// user's login shell has it, the pane runs that path. On PATH: the bare
/// name, as always.
/// Returns that path, when the login shell's was used.
fn pane_program(agent: Agent, argv: &mut [String]) -> Option<std::path::PathBuf> {
    let at = install::pane_program(agent.name());
    program_at(agent, argv, at.clone());
    at
}

/// Put the agent's program at `at` in its command. The program is the
/// command's first word, or (a launch that runs through `sh -c`, as
/// OpenCode's does) the first word that is the agent's name; never a later
/// one (a prompt that happens to be that word).
fn program_at(agent: Agent, argv: &mut [String], at: Option<std::path::PathBuf>) {
    let Some(p) = at else {
        return;
    };
    let i = argv.iter().position(|a| a == agent.name()).unwrap_or(0);
    if let Some(program) = argv.get_mut(i) {
        *program = p.to_string_lossy().into_owned();
    }
}

/// The pane's environment: which session it is, and, when its program was
/// found only by the login shell (`program`), that program's directory at
/// the front of PATH. Such an install often needs what sits beside it: an
/// npm or nvm install is a `#!/usr/bin/env node` script, with `node` in
/// the same directory.
///
/// A pane whose conversation runs on Codex's daemon (`daemon`) gets no
/// `RLORD_SESSION_ID` (and `pane_command` unsets one inherited from tmux):
/// its hooks run in the daemon and go by thread, and a daemon the TUI
/// starts from this pane would hand the variable to every thread's hooks.
fn pane_env(
    session_id: &str,
    program: Option<&std::path::Path>,
    daemon: bool,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if !daemon {
        env.push(("RLORD_SESSION_ID".to_string(), session_id.to_string()));
    }
    let path = std::env::var("PATH").unwrap_or_default();
    env.extend(path_with(program, &path).map(|p| ("PATH".to_string(), p)));
    env
}

/// `path` with `program`'s directory in front (None: nothing to add).
fn path_with(program: Option<&std::path::Path>, path: &str) -> Option<String> {
    let dir = program?.parent()?.to_str().filter(|d| !d.is_empty())?;
    Some(if path.is_empty() {
        dir.to_string()
    } else {
        format!("{dir}:{path}")
    })
}

/// The argv a session's pane was last started with: what it actually runs
/// (its launch spec may already hold flags for the next start).
pub fn launched_args_key(id: &str) -> String {
    format!("launched_args:{id}")
}

/// The command a session's pane runs (see `pane_env`).
fn pane_cmd(
    session_id: &str,
    program: Option<&std::path::Path>,
    daemon: bool,
    argv: &[String],
) -> String {
    let env = pane_env(session_id, program, daemon);
    if daemon {
        tmux::pane_command_unsetting(&["RLORD_SESSION_ID"], &env, argv)
    } else {
        tmux::pane_command(&env, argv)
    }
}

fn note_launch(db: &Db, id: &str, argv: &[String]) -> Result<()> {
    db.kv_set(&launched_args_key(id), &serde_json::to_string(argv)?)
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
    // On Codex's shared background server (`[session] codex_daemon`): not
    // the overlord (its wiring is `-c` overrides, which Codex runs only in
    // the pane), nor a session whose own flags need the pane.
    if agent == Agent::Codex
        && cfg.session.codex_daemon
        && !crate::overlord::is_overlord_role(&role)
        && !codex::needs_embedded(&extra_args)
    {
        spec["codex_host"] = json!(codex::HOST_DAEMON);
    }
    let daemon = codex::on_daemon(&spec);
    let mut launch = agent.launch(
        &project_path,
        prompt.as_deref(),
        model.as_deref(),
        &spec_args(agent, &spec),
    );
    let program = pane_program(agent, &mut launch.argv);
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
    hold_deferred(db, &id, &launch)?;
    if !agent.mints_id() {
        // Its conversation is bound once the agent reports it (or it turns
        // up in the agent's own index): one started from now on.
        db.kv_set(&bind_since_key(&id), &now.to_string())?;
        db.kv_set(&bind_prompt_key(&id), prompt.as_deref().unwrap_or(""))?;
    }

    let cmd = pane_cmd(&id, program.as_deref(), daemon, argv);
    note_launch(db, &id, argv)?;
    // (Its folder is in the gate before its first hook can run.)
    crate::codex_daemon::sync_gate(db);
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
    // The overlords' fixed names are reserved, even before they exist.
    let reserved = |n: &str| crate::overlord::is_tmux_name(n);
    rlord_core::names::tmux_name_for(name, |n| {
        Some(n) != own
            && (reserved(n) || db.tmux_name_taken(n).unwrap_or(false) || tmux::has_session(n))
    })
}

/// The tmux name a session should have: an overlord's fixed one, else its
/// name plus `-rlord`.
fn wanted_tmux_name(db: &Db, s: &rlord_core::types::Session) -> Option<String> {
    let cur = s.tmux_session.as_deref();
    if crate::overlord::is_overlord_role(&s.role) {
        if cur.is_some_and(crate::overlord::is_tmux_name) {
            return None; // already has one of them
        }
        return Some(crate::overlord::free_tmux_name(db, &s.agent));
    }
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
    // The overlord first: its fixed name must never be taken by another.
    let (overlord, rest): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|s| crate::overlord::is_overlord_role(&s.role));
    for s in overlord.into_iter().chain(rest) {
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
    let (mut argv, resumed) = agent.resume(
        s.agent_session.as_deref(),
        &s.agent_session_trust,
        &s.project_path,
        model.as_deref(),
        &spec_args(agent, &spec),
    )?;
    let program = pane_program(agent, &mut argv);
    // What an earlier start left to confirm is over (its clock too). A
    // session that starts anew under its id (the agent never had it) is
    // created with the launch's model: that is confirmed as at launch.
    let deferred = agent.deferred(model.as_deref()).filter(|_| !resumed);
    hold(db, &s.id, deferred.as_ref())?;
    let tmux_name = tmux_name_of(db, &s)?;
    let cmd = pane_cmd(&s.id, program.as_deref(), s.codex_daemon, &argv);
    note_launch(db, &s.id, &argv)?;
    if tmux::has_session(&tmux_name) {
        tmux::respawn_pane(&tmux_name, &s.project_path, &cmd)?;
    } else {
        tmux::new_session(&tmux_name, &s.project_path, &cmd)?;
    }
    let now = now_ms();
    db.set_status(&s.id, Status::Starting, now)?;
    // Listed in the gate again (no longer stopped). (A resumed Codex runs
    // no hook before its next prompt.)
    crate::codex_daemon::sync_gate(db);
    db.insert_event(
        &s.id,
        now,
        if resumed { "resumed" } else { "restarted" },
        &match (resumed, agent.deferred(model.as_deref())) {
            // (Resumed: whatever model the conversation has by now.)
            (true, Some(d)) => json!({"argv": argv,
                "model": format!("the session keeps its own (launched with {})", d.model)}),
            _ => json!({"argv": argv}),
        }
        .to_string(),
    )?;
    tracing::info!(session = %s.id, resumed, "restarted session");
    Ok(serde_json::to_value(db.resolve_ref(&s.id)?)?)
}

/// The tmux session a session's agent (re)starts in: its own, or (it has
/// none: imported) a new name, saved.
fn tmux_name_of(db: &Db, s: &Session) -> Result<String> {
    if let Some(n) = &s.tmux_session {
        return Ok(n.clone());
    }
    let n = tmux_name_for(db, &s.name, None);
    db.set_tmux_session(&s.id, &n)?;
    Ok(n)
}

/// Restart a session's agent on a **fresh conversation** in its pane (a new
/// conversation id, not a resume). The old conversation stays on disk and
/// can still be resumed with the agent's own `--resume`. `reason` says why
/// in the event (`idle`: the overlord's idle restart, with `idle_mins`;
/// `manual` otherwise).
pub fn session_fresh(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    let agent = Agent::named(&s.agent)?;
    let spec: Value = serde_json::from_str(&db.launch_spec(&s.id)?).unwrap_or(json!({}));
    let model = spec.get("model").and_then(Value::as_str);
    let mut launch = agent.launch(&s.project_path, None, model, &spec_args(agent, &spec));
    let program = pane_program(agent, &mut launch.argv);
    let argv = &launch.argv;
    let tmux_name = tmux_name_of(db, &s)?;
    let cmd = pane_cmd(&s.id, program.as_deref(), s.codex_daemon, argv);
    note_launch(db, &s.id, argv)?;
    let now = now_ms();
    if tmux::has_session(&tmux_name) {
        tmux::respawn_pane(&tmux_name, &s.project_path, &cmd)?;
    } else {
        tmux::new_session(&tmux_name, &s.project_path, &cmd)?;
    }
    hold_deferred(db, &s.id, &launch)?;
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
        &json!({"from": s.agent_session, "to": launch.session_id,
                "reason": params["reason"].as_str().unwrap_or("manual"),
                "idle_mins": params["idle_mins"]})
        .to_string(),
    )?;
    Ok(serde_json::to_value(db.resolve_ref(&s.id)?)?)
}

/// Keep what a launch left to confirm once the agent has the conversation
/// (its `Deferred`: OpenCode's model) for whatever follows that agent
/// (`opencode::apply_deferred`), which gives up after a while (`at`: when
/// it was held). A launch without one clears an older one.
fn hold_deferred(db: &Db, id: &str, launch: &rlord_core::agents::Launch) -> Result<()> {
    hold(db, id, launch.deferred.as_ref())
}

fn hold(db: &Db, id: &str, deferred: Option<&rlord_core::agents::Deferred>) -> Result<()> {
    let key = crate::opencode::deferred_key(id);
    match deferred {
        Some(d) => {
            let mut held = serde_json::to_value(d)?;
            held["at"] = json!(now_ms());
            db.kv_set(&key, &held.to_string())
        }
        None => db.kv_delete(&key),
    }
}

/// Stop a session: its pane goes. A session on Codex's daemon had its
/// running turn interrupted first, with no lock held (`params.interrupt`,
/// set by the caller from `codex_daemon::interrupt`): said in the answer
/// and the log.
pub fn session_stop(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    if let Some(t) = &s.tmux_session {
        tmux::kill_session(t)?;
    }
    let now = now_ms();
    db.kv_delete(&needs_you_key(&s.id))?;
    db.set_status(&s.id, Status::Stopped, now)?;
    let turn = &params["interrupt"];
    db.insert_event(&s.id, now, "stopped", &stop_detail(turn).to_string())?;
    crate::codex_daemon::sync_gate(db);
    let mut r = json!({"stopped": s.id});
    say_turn(&mut r, turn);
    Ok(r)
}

/// The log entry's detail for a stop: what became of a running turn.
fn stop_detail(turn: &Value) -> Value {
    if turn.is_null() {
        json!({})
    } else {
        json!({"turn": turn})
    }
}

/// Put what became of a running turn in a stop's answer, in a line when it
/// may still be running.
fn say_turn(r: &mut Value, turn: &Value) {
    if turn.is_null() {
        return;
    }
    r["turn"] = turn.clone();
    if turn == "still_running" {
        r["note"] = json!(
            "its turn didn't stop when interrupted: it may still be running on Codex's \
             background server (`codex resume` the thread to see it)"
        );
    }
}

pub fn session_archive(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    refuse_overlord(&s, "archived (stop it instead)")?;
    if let Some(t) = &s.tmux_session {
        tmux::kill_session(t)?;
    }
    let now = now_ms();
    db.kv_delete(&needs_you_key(&s.id))?;
    db.set_status(&s.id, Status::Stopped, now)?;
    db.set_archived(&s.id, Some(now))?;
    let turn = &params["interrupt"];
    db.insert_event(&s.id, now, "archived", &stop_detail(turn).to_string())?;
    crate::codex_daemon::sync_gate(db);
    let mut r = json!({"archived": s.id});
    say_turn(&mut r, turn);
    Ok(r)
}

pub fn session_log(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    let limit = params.get("limit").and_then(Value::as_i64).unwrap_or(50);
    Ok(serde_json::to_value(db.list_events(&s.id, limit)?)?)
}

/// Ingest one agent hook event (from `rlord hook emit`, live or spooled).
pub fn hook_emit(db: &Db, params: &Value) -> Result<Value> {
    hook_emit_with(db, params, &|agent, id| agent.indexed(id))
}

/// `hook_emit`, with how to ask an agent's own index whether a
/// conversation is one the user drives (tests answer it themselves).
pub(crate) fn hook_emit_with(
    db: &Db,
    params: &Value,
    indexed: &dyn Fn(Agent, &str) -> Indexed,
) -> Result<Value> {
    // `instance`: the hook's RLORD_SESSION_ID. A hook Codex's background
    // server ran has none (`by: "thread"`): see `codex_daemon::route`.
    let instance = params["instance"].as_str();
    let agent = p_str(params, "agent")?;
    let event = p_str(params, "event")?;
    let payload = params.get("payload").cloned().unwrap_or(json!({}));
    let named = instance.and_then(|i| db.resolve_ref(i).ok());
    let (s, by_thread) = if agent == Agent::Codex.name() {
        match crate::codex_daemon::route(db, event, &payload, named)? {
            crate::codex_daemon::Route::Pane(s) => (s, false),
            crate::codex_daemon::Route::Thread(s) => (s, true),
            crate::codex_daemon::Route::Ignore(why) => {
                tracing::debug!(?instance, why, "ignoring a Codex hook");
                return Ok(json!({"ignored": why}));
            }
        }
    } else {
        let Some(s) = named else {
            // Env var pointed at a session we don't know (deleted?); drop quietly.
            tracing::debug!(?instance, "hook event for unknown session");
            return Ok(json!({"ignored": true}));
        };
        (s, false)
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
    // A thread on Codex's daemon ends without its pane, never with it (a
    // client leaving ends nothing): a SessionEnd while the pane is up
    // doesn't stop the session (`discover` stops it once the pane goes).
    if by_thread && event == "SessionEnd" && crate::codex_daemon::pane_up(&s) {
        return Ok(json!({"ignored": "its pane is still up"}));
    }
    // (A hook matched by its thread ran in Codex's daemon: no pane of its
    // own to compare.)
    if let Some(why) = foreign_hook(&s, params).filter(|_| !by_thread) {
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
                sid,
                if other {
                    indexed(agent, sid)
                } else {
                    Indexed::NoIndex
                },
            )
        });
        let bound = matches!(whose, Whose::Bind | Whose::Adopt);
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
                // The user's own `/clear` (Claude says so when the new
                // conversation starts) is a fresh conversation like an idle
                // restart, so the history of them is complete.
                if cleared(event, &payload) {
                    db.insert_event(
                        &s.id,
                        now,
                        "fresh_conversation",
                        &json!({"from": s.agent_session, "to": sid, "reason": "clear"}).to_string(),
                    )?;
                    if crate::overlord::is_overlord_role(&s.role) {
                        crate::overlord::set_fresh_at(db, &s.id, now)?;
                    }
                } else {
                    db.insert_event(
                        &s.id,
                        now,
                        "agent_session_changed",
                        &json!({"from": s.agent_session, "to": sid}).to_string(),
                    )?;
                }
                db.set_agent_session(&s.id, sid, "hook")?;
            }
            (Whose::Unbound, _) => {
                return Ok(json!({"ignored": "no conversation bound yet"}));
            }
            _ => {}
        }
        if bound {
            // What binding from the agent's index would have needed.
            db.kv_delete(&bind_since_key(&s.id))?;
            db.kv_delete(&bind_prompt_key(&s.id))?;
        }
        // A thread that was blocked as "another conversation" and is now
        // the session's own: it is the session's own agent that waits.
        if let (Whose::Adopt, Some(sid)) = (whose, sid) {
            let key = needs_you_key(&s.id);
            let was: Vec<String> = db
                .kv_get(&key)?
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_default();
            let now_own = adopted(was.clone(), sid);
            if now_own != was {
                db.kv_set(&key, &json!(now_own).to_string())?;
            }
        }
        let (kind, effect) = agent.map_hook_event(event, &payload);
        if whose != Whose::Other {
            if let Some(tp) = payload.get("transcript_path").and_then(Value::as_str) {
                db.upsert_transcript_source(&s.id, tp)?;
            }
        }
        // Who this event is about, when it blocks or unblocks: a sub-agent
        // (Claude names it; Codex's has its own thread), else the
        // session's own agent (""). Unknown when the payload was cut off.
        let party: Option<&str> = (payload["truncated"] != true).then(|| {
            payload["agent_id"]
                .as_str()
                .or(sid.filter(|_| whose == Whose::Other))
                .unwrap_or("")
        });
        let change = block_change(whose == Whose::Other, kind, effect, party);
        // Another conversation in the pane (a sub-agent's thread) never
        // ends the session's turn or stops it; only a prompt it is blocked
        // on, and its moving on or ending, concern the session.
        if whose == Whose::Other && change == Block::Keep {
            return Ok(json!({"ignored": "another conversation in the pane"}));
        }
        let was: Vec<String> = db
            .kv_get(&needs_you_key(&s.id))?
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        let (blocked, status) = blocked_after(was.clone(), &change, effect, s.status);
        if (effect == Effect::Resume || whose == Whose::Other) && status.is_none() {
            // Someone moved on or ended, but not (all of) who the session
            // waits for.
            if blocked != was {
                db.kv_set(&needs_you_key(&s.id), &json!(blocked).to_string())?;
            }
            return Ok(json!({"ok": true, "unchanged": true}));
        }
        // Kept only while the session needs someone.
        let waiting = status.unwrap_or(s.status) == Status::WaitingInput;
        if blocked.is_empty() || !waiting {
            if !was.is_empty() {
                db.kv_delete(&needs_you_key(&s.id))?;
            }
        } else if blocked != was {
            db.kv_set(&needs_you_key(&s.id), &json!(blocked).to_string())?;
        }
        // Only a prompt and a finished turn are messages: a permission
        // prompt, a question or a tool call never moves "updated".
        // A prompt rlord typed itself (`own.rs`: matched by the hash of
        // its text, or by time when the payload has none) is logged as
        // such, and is not the user's.
        let mut source = None;
        let kind = match kind {
            _ if whose == Whose::Other => kind,
            "prompt_submitted" => {
                let text = crate::own::mark_of_payload(&payload);
                let (kind, by) =
                    crate::own::prompt(db, &s.id, text.as_ref(), now, crate::own::Via::Hook)?;
                source = by;
                kind
            }
            "agent_stopped" => {
                db.touch_last_agent_msg(&s.id, now)?;
                kind
            }
            _ => kind,
        };
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
            // Keep the log light: the event name only. (Another thread's
            // event is logged as what it did to this session.)
            let kind = match (whose, status) {
                (Whose::Other, Some(Status::WaitingInput)) => "permission_request",
                (Whose::Other, _) => "tool",
                _ => kind,
            };
            let detail = crate::own::with_source(json!({"hook": event}), source);
            db.insert_event(&s.id, now, kind, &detail.to_string())?;
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

/// Whether a hook says the user cleared the conversation: Claude's
/// SessionStart carries `source: "clear"` for the one a `/clear` starts.
fn cleared(event: &str, payload: &Value) -> bool {
    event == "SessionStart" && payload["source"] == "clear"
}

/// kv key: who the session's "needs you" is for, as a JSON list ("" its
/// own agent, else a sub-agent's id or thread). Present only while it
/// waits on someone.
fn needs_you_key(id: &str) -> String {
    format!("needs_you_by:{id}")
}

/// What an event does to the set of parties a session is blocked on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    Keep,
    /// This party is blocked on the user.
    Add(String),
    /// This party moved on (a tool ran): no longer blocked.
    MovedOn(String),
    /// This party ended (a sub-agent stopped): it can't be blocked.
    Ended(String),
    /// The session's own turn started or ended: nobody is blocked.
    Clear,
}

/// `other`: the event is from another conversation in the pane. `party`:
/// who it is about (None: not known, the payload was cut off).
fn block_change(other: bool, kind: &str, effect: Effect, party: Option<&str>) -> Block {
    let party = party.map(str::to_string);
    match effect {
        // A notification says someone is blocked, not who (Claude's carries
        // no agent id, and comes before or after the request it is about):
        // the status follows, the set is left to the events that name one.
        Effect::Set(Status::WaitingInput) if kind == "notification" => Block::Keep,
        Effect::Set(Status::WaitingInput) => party.map_or(Block::Keep, Block::Add),
        Effect::Resume => party.map_or(Block::Keep, Block::MovedOn),
        // A sub-agent's thread ended its turn or its session.
        Effect::Set(_) if other => party.map_or(Block::Keep, Block::Ended),
        // The session's own prompt, end of turn, start or end.
        Effect::Set(_) => Block::Clear,
        // Claude: a sub-agent stopped (it reports under the session's id).
        Effect::Nothing if kind == "subagent_stopped" => party
            .filter(|p| !p.is_empty())
            .map_or(Block::Keep, Block::Ended),
        Effect::Nothing => Block::Keep,
    }
}

/// The blocked set after `change`, and the status the session goes to (if
/// it changes). It needs you while anyone in the set is blocked. An empty
/// set with the session waiting is a wait whose party isn't known (set
/// before parties were tracked, or from a cut-off payload): any tool
/// running clears that.
fn blocked_after(
    mut blocked: Vec<String>,
    change: &Block,
    effect: Effect,
    current: Status,
) -> (Vec<String>, Option<Status>) {
    let waiting = current == Status::WaitingInput;
    let freed = |blocked: &[String]| (waiting && blocked.is_empty()).then_some(Status::Working);
    match change {
        Block::Add(p) => {
            if !blocked.contains(p) {
                blocked.push(p.clone());
            }
            (blocked, effect.apply(current))
        }
        Block::MovedOn(p) => {
            let unknown = blocked.is_empty();
            let was_in = blocked.contains(p);
            blocked.retain(|b| b != p);
            let status = if unknown || was_in {
                freed(&blocked)
            } else {
                None
            };
            (blocked, status)
        }
        Block::Ended(p) => {
            let was_in = blocked.contains(p);
            blocked.retain(|b| b != p);
            let status = if was_in { freed(&blocked) } else { None };
            (blocked, status)
        }
        Block::Clear => (Vec::new(), effect.apply(current)),
        // (A cut-off payload: the status follows, the set is left alone.)
        Block::Keep => {
            let status = match effect {
                Effect::Resume => None,
                _ => effect.apply(current),
            };
            (blocked, status)
        }
    }
}

/// The blocked set once thread `sid` has become the session's own: its
/// entry is the session's own agent ("").
fn adopted(blocked: Vec<String>, sid: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in blocked {
        let p = if p == sid { String::new() } else { p };
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
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
    /// Another conversation in the pane (a sub-agent's thread, or one the
    /// agent's index doesn't know yet).
    Other,
    /// Nothing is bound and this event can't bind: not the session's.
    Unbound,
}

/// Decide whose conversation `sid` is. `bound`: the session's current one.
/// `indexed`: what the agent's own index says about `sid` when it differs.
fn conversation(
    agent: Agent,
    event: &str,
    bound: Option<&str>,
    sid: &str,
    indexed: Indexed,
) -> Whose {
    match bound {
        Some(cur) if cur == sid => Whose::Ours,
        // An agent that assigns its own id starts a conversation with a
        // SessionStart: until one arrives (after a launch or a fresh start)
        // an event is a late one from the process that was just replaced,
        // and must not bind its old conversation or stop the session.
        None if agent.mints_id() || agent.adopts_new_id_on(event) => Whose::Bind,
        None => Whose::Unbound,
        // rlord chose the id and the agent reports what it writes on every
        // event (a /clear, a resume): follow it.
        Some(_) if agent.mints_id() => Whose::Adopt,
        // The agent assigns ids, and sub-agents have threads of their own
        // in the same pane: the session moves only to a thread the index
        // says the user drives (whether rlord's own guess from the index
        // or an earlier hook bound the current one). Listed as a
        // sub-agent's: never. Not listed yet: wait for a later event.
        Some(_) => match indexed {
            Indexed::User => Whose::Adopt,
            Indexed::SubAgent | Indexed::NotListed => Whose::Other,
            // No index to ask at all: a new conversation in the pane is
            // what starts one.
            Indexed::NoIndex if agent.adopts_new_id_on(event) => Whose::Adopt,
            Indexed::NoIndex => Whose::Other,
        },
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
    // Its saved read position first, so a position that fell out of memory
    // doesn't mean re-reading the whole file here (under the DB lock).
    if let Some(tp) = payload["agent_transcript_path"].as_str() {
        transcript::seed_cursors(db.transcript_cursors_for(&[tp])?);
    }
    let from_disk = source
        .as_deref()
        // Only this hook's agent, not every sub-agent of the session.
        .map(|src| claude::scan_subagents_read(session_id, src, true, &|id| id == agent_id))
        .unwrap_or_default()
        .into_iter()
        .find(|(a, _)| a.agent_id == agent_id);
    // Totals read from its transcript (whole, as `transcript_stats` keeps
    // them) may replace ones counted by older logic.
    let mut counted = from_disk.as_ref().is_some_and(|(_, read)| *read);
    let from_disk = from_disk.map(|(a, _)| a);
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
                    counted = true;
                }
            }
            if let Some(r) = payload["last_assistant_message"].as_str() {
                a.result = r.chars().take(4000).collect();
            }
        }
    }
    if counted {
        db.upsert_subagent_counted(&a)?;
    } else {
        db.upsert_subagent(&a)?;
    }
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
        // Codex runs in the pane unless its spec puts it on the daemon.
        assert_eq!(spec_args(Agent::Codex, &on), ["--no-daemon", "--x"]);
        let daemon = json!({"extra_args": ["--x"], "codex_host": "daemon"});
        assert_eq!(spec_args(Agent::Codex, &daemon), ["--x"]);
        assert!(spec_args(Agent::OpenCode, &daemon) == ["--x"]);
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
    fn the_pane_runs_the_program_the_check_found() {
        // On the panes' PATH: the bare name, as always.
        let mut argv = vec!["codex".to_string(), "--no-daemon".to_string()];
        program_at(Agent::Codex, &mut argv, None);
        assert_eq!(argv, ["codex", "--no-daemon"]);
        // Only the login shell has it: its path, the arguments as they were.
        program_at(Agent::Codex, &mut argv, Some("/opt/tools/bin/codex".into()));
        assert_eq!(argv, ["/opt/tools/bin/codex", "--no-daemon"]);
        // A launch through `sh -c`: the program is where the agent's name
        // first is, and a prompt that is that word stays a prompt.
        let mut l = Agent::OpenCode
            .launch("/src/app", Some("opencode"), None, &[])
            .argv;
        program_at(
            Agent::OpenCode,
            &mut l,
            Some("/opt/tools/bin/opencode".into()),
        );
        assert_eq!(
            (l[0].as_str(), l[3].as_str()),
            ("sh", "/opt/tools/bin/opencode")
        );
        assert_eq!(l.last().map(String::as_str), Some("opencode"));
    }

    #[test]
    fn what_a_start_left_to_confirm_doesnt_outlive_it() {
        let db = Db::open_in_memory().unwrap();
        let key = crate::opencode::deferred_key("01oc");
        let model = |m: &str| rlord_core::agents::Deferred { model: m.into() };
        hold(&db, "01oc", Some(&model("opencode/x-free"))).unwrap();
        let held: Value = serde_json::from_str(&db.kv_get(&key).unwrap().unwrap()).unwrap();
        assert_eq!(held["model"], "opencode/x-free");
        assert!(held["at"].as_i64().unwrap() > 0);
        // A later start with nothing to confirm (a resume): the old record
        // goes, so its clock can't run out on the new start.
        hold(&db, "01oc", None).unwrap();
        assert_eq!(db.kv_get(&key).unwrap(), None);
        // One that has something: a new record, with its own time.
        db.kv_set(&key, r#"{"model":"opencode/old","at":1}"#)
            .unwrap();
        hold(&db, "01oc", Some(&model("opencode/new"))).unwrap();
        let held: Value = serde_json::from_str(&db.kv_get(&key).unwrap().unwrap()).unwrap();
        assert_eq!(held["model"], "opencode/new");
        assert!(held["at"].as_i64().unwrap() > 1);
    }

    #[test]
    fn whose_conversation_a_hook_speaks_for() {
        use Whose::*;
        let codex = |event: &str, bound: Option<&str>, indexed: Indexed| {
            conversation(Agent::Codex, event, bound, "t-2", indexed)
        };
        assert_eq!(codex("Stop", Some("t-2"), Indexed::NoIndex), Ours);
        // Nothing bound: a SessionStart binds; a late event from the
        // process just replaced (a fresh start) does not.
        assert_eq!(codex("SessionStart", None, Indexed::NotListed), Bind);
        assert_eq!(codex("SessionEnd", None, Indexed::NotListed), Unbound);
        assert_eq!(codex("Stop", None, Indexed::NotListed), Unbound);
        // Bound: the session moves only to a thread the index says the
        // user drives, on any event (so an index-bound guess is corrected).
        assert_eq!(codex("SessionStart", Some("t-1"), Indexed::User), Adopt);
        assert_eq!(codex("Stop", Some("t-1"), Indexed::User), Adopt);
        // A sub-agent's never; one the index doesn't list yet: wait.
        assert_eq!(codex("SessionStart", Some("t-1"), Indexed::SubAgent), Other);
        assert_eq!(
            codex("SessionStart", Some("t-1"), Indexed::NotListed),
            Other
        );
        assert_eq!(codex("Stop", Some("t-1"), Indexed::NotListed), Other);
        // No index at all: a SessionStart is what starts a conversation.
        assert_eq!(codex("SessionStart", Some("t-1"), Indexed::NoIndex), Adopt);
        assert_eq!(codex("Stop", Some("t-1"), Indexed::NoIndex), Other);
        // Claude chose its id and reports what it writes on every event.
        let claude = |event: &str, bound: Option<&str>| {
            conversation(Agent::Claude, event, bound, "t-2", Indexed::NoIndex)
        };
        assert_eq!(claude("Stop", None), Bind);
        assert_eq!(claude("Stop", Some("t-1")), Adopt);
        assert_eq!(claude("Stop", Some("t-2")), Ours);
    }

    #[test]
    fn a_program_found_by_the_login_shell_brings_its_directory() {
        // On the panes' PATH already: nothing added.
        assert_eq!(path_with(None, "/usr/bin:/bin"), None);
        // An nvm install: `node` sits beside the `codex` script.
        let program = std::path::Path::new("/home/a/.nvm/versions/node/v22/bin/codex");
        assert_eq!(
            path_with(Some(program), "/usr/bin:/bin").as_deref(),
            Some("/home/a/.nvm/versions/node/v22/bin:/usr/bin:/bin")
        );
        assert_eq!(
            path_with(Some(program), "").as_deref(),
            Some("/home/a/.nvm/versions/node/v22/bin")
        );
        let env = pane_env("01s", Some(program), false);
        assert_eq!(env[0], ("RLORD_SESSION_ID".to_string(), "01s".to_string()));
        assert_eq!(env[1].0, "PATH");
        assert!(env[1].1.starts_with("/home/a/.nvm/versions/node/v22/bin"));
        assert_eq!(pane_env("01s", None, false).len(), 1);
    }

    /// A pane on Codex's daemon names no session: a daemon its TUI starts
    /// would pass the name to every thread's hooks.
    #[test]
    fn a_daemon_pane_names_no_session() {
        assert!(pane_env("01s", None, true).is_empty());
        let argv = ["codex".to_string()];
        assert_eq!(
            pane_cmd("01s", None, true, &argv),
            "exec env -u RLORD_SESSION_ID codex"
        );
        assert_eq!(
            pane_cmd("01s", None, false, &argv),
            "exec env RLORD_SESSION_ID=01s codex"
        );
    }

    #[test]
    fn an_adopted_thread_waits_as_the_sessions_own_agent() {
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(adopted(set(&["t-9", "a-7"]), "t-9"), set(&["", "a-7"]));
        assert_eq!(adopted(set(&["", "t-9"]), "t-9"), set(&[""]));
        assert_eq!(adopted(set(&["a-7"]), "t-9"), set(&["a-7"]));
    }

    #[test]
    fn it_needs_you_while_anyone_is_blocked() {
        use Status::*;
        let needs = Effect::Set(WaitingInput);
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let step = |blocked: &[&str], change: Block, effect: Effect, cur: Status| {
            blocked_after(set(blocked), &change, effect, cur)
        };
        // The main agent, then a sub-agent, are blocked.
        let (b, st) = step(&[], Block::Add("".into()), needs, Working);
        assert_eq!((b.clone(), st), (set(&[""]), Some(WaitingInput)));
        let (b, st) = step(&[""], Block::Add("a-7".into()), needs, WaitingInput);
        assert_eq!((b, st), (set(&["", "a-7"]), None));
        // One moves on: still needs you for the other. Then the other.
        let (b, st) = step(
            &["", "a-7"],
            Block::MovedOn("a-7".into()),
            Effect::Resume,
            WaitingInput,
        );
        assert_eq!((b, st), (set(&[""]), None));
        let (b, st) = step(
            &[""],
            Block::MovedOn("".into()),
            Effect::Resume,
            WaitingInput,
        );
        assert_eq!((b, st), (set(&[]), Some(Working)));
        // Someone who wasn't blocked moving on changes nothing.
        let (b, st) = step(
            &[""],
            Block::MovedOn("a-9".into()),
            Effect::Resume,
            WaitingInput,
        );
        assert_eq!((b, st), (set(&[""]), None));
        // A blocked sub-agent that ends without another tool frees it too.
        let (b, st) = step(
            &["a-7"],
            Block::Ended("a-7".into()),
            Effect::Nothing,
            WaitingInput,
        );
        assert_eq!((b, st), (set(&[]), Some(Working)));
        // Its ending while the main agent is blocked doesn't.
        let (b, st) = step(
            &["", "a-7"],
            Block::Ended("a-7".into()),
            Effect::Nothing,
            WaitingInput,
        );
        assert_eq!((b, st), (set(&[""]), None));
        // The session's own turn ending clears everyone.
        let (b, st) = step(&["", "a-7"], Block::Clear, Effect::Set(Idle), WaitingInput);
        assert_eq!((b, st), (set(&[]), Some(Idle)));
        // A wait whose party isn't known: any tool running clears it; a
        // sub-agent merely ending doesn't.
        let (_, st) = step(
            &[],
            Block::MovedOn("a-1".into()),
            Effect::Resume,
            WaitingInput,
        );
        assert_eq!(st, Some(Working));
        let (_, st) = step(
            &[],
            Block::Ended("a-1".into()),
            Effect::Nothing,
            WaitingInput,
        );
        assert_eq!(st, None);
        // A cut-off payload: it can set "needs you" but names nobody, and
        // can't clear anybody.
        assert_eq!(
            block_change(false, "permission_request", needs, None),
            Block::Keep
        );
        assert_eq!(
            block_change(false, "tool", Effect::Resume, None),
            Block::Keep
        );
        let (b, st) = step(&[""], Block::Keep, Effect::Resume, WaitingInput);
        assert_eq!((b, st), (set(&[""]), None));
        let (b, st) = step(&[], Block::Keep, needs, Working);
        assert_eq!((b, st), (set(&[]), Some(WaitingInput)));
        // A notification says someone is blocked, not who: the status
        // follows, the set is left to the events that name a party.
        assert_eq!(
            block_change(false, "notification", needs, Some("")),
            Block::Keep
        );
        let (b, st) = step(&["a-7"], Block::Keep, needs, WaitingInput);
        assert_eq!((b, st), (set(&["a-7"]), None));
        // What each event means.
        assert_eq!(
            block_change(false, "subagent_stopped", Effect::Nothing, Some("a-7")),
            Block::Ended("a-7".into())
        );
        assert_eq!(
            block_change(true, "agent_stopped", Effect::Set(Idle), Some("t-sub")),
            Block::Ended("t-sub".into())
        );
        assert_eq!(
            block_change(false, "agent_stopped", Effect::Set(Idle), Some("")),
            Block::Clear
        );
        assert_eq!(
            block_change(false, "pre_compact", Effect::Nothing, Some("")),
            Block::Keep
        );
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
