//! The overlord: a singleton, long-running agent session whose job is to
//! drive rlord itself. It is an ordinary session in every respect (tmux,
//! hooks, resume) plus three things: `role = 'overlord'`, a fixed tmux name
//! (so the TUI can embed it without a lookup), and rlord's MCP
//! server wired in with its tools pre-allowed.
//!
//! One session: a strong model at medium effort. It does light work itself and
//! sends anything heavier to a subagent or a new session (`overlord.md`).
//! It is restarted on a fresh conversation once it has sat idle
//! (`start_fresh_keeper`), and looks its own past up with `recall`
//! (recall.rs) rather than carrying it.
//!
//! It lives in its own folder under the data dir. On every `ensure` that
//! folder's CLAUDE.md / AGENTS.md / GEMINI.md (the same standing instructions,
//! from `overlord.md` plus the user's own), each agent's MCP config and its
//! subagents are regenerated (`write_files`), so upgrades to rlord reach an
//! existing overlord on its next restart. Any agent rlord runs can run it.
//!
//! The overlord used to be two sessions (a fast front desk and a
//! "mastermind" behind it). `migrate` turns such a pair into the one
//! overlord: the mastermind's conversation carries on as the overlord's.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::{codex, opencode, transcript, Agent};
use rlord_core::config::Config;
use rlord_core::db::Db;
use rlord_core::themes;
use rlord_core::types::{Session, Status};
use rlord_core::{models, paths, tmux};

use crate::methods;

/// The overlord's role in the DB, and its display name.
pub const ROLE: &str = "overlord";
/// Its fixed tmux session name (the overlay embeds its pane).
pub const TMUX_NAME: &str = "overlord-rlord";
/// kv: when the overlord last started a fresh conversation (ms).
pub const FRESH_AT: &str = "overlord_fresh_at";
/// kv: don't offer this default model for the overlord again.
pub const DEFAULT_DECLINED: &str = "default_declined:overlord";

const INSTRUCTIONS: &str = include_str!("overlord.md");
/// Subagents it delegates to, in Claude's format (written to its
/// `.claude/agents/`, and converted for the other agents: `write_files`).
const SUBAGENTS: &[(&str, &str)] = &[
    ("rlord-tidy.md", include_str!("tidy.md")),
    ("rlord-import.md", include_str!("import.md")),
];

/// Where the subagents' instructions are also written as plain files, in
/// the overlord's folder (`overlord.md` points at it).
const SUBAGENT_DIR: &str = "subagents";

/// The second session's role, tmux name and folder, from when the overlord
/// was two (see `migrate`).
const OLD_MASTERMIND: &str = "mastermind";

/// Whether a session role is the overlord's (a fixed row: no rename, tags,
/// notes, archive).
pub fn is_overlord_role(role: &str) -> bool {
    role == ROLE
}

/// The overlord's session, if it has been set up.
pub fn session(db: &Db) -> Result<Option<Session>> {
    db.overlord()
}

/// The folder a new overlord gets.
fn default_dir() -> PathBuf {
    paths::data_dir().join(ROLE)
}

/// Every folder an overlord has run in: its conversations there are its
/// own (full of searches), never search results.
pub fn dirs() -> Vec<PathBuf> {
    vec![default_dir(), paths::data_dir().join(OLD_MASTERMIND)]
}

/// User-authored standing instructions appended to the generated ones.
pub fn user_instructions_path() -> PathBuf {
    paths::config_path()
        .parent()
        .map(|p| p.join("overlord.md"))
        .unwrap_or_else(|| PathBuf::from("overlord.md"))
}

/// The flags that set `effort` on `agent` (none: it has no such setting,
/// or the agent's own default is wanted).
pub fn effort_args(agent: Agent, effort: &str) -> Vec<String> {
    let effort = effort.trim();
    if effort.is_empty() {
        return Vec::new();
    }
    match agent {
        Agent::Claude => vec!["--effort".into(), effort.into()],
        Agent::Codex => vec![
            "-c".into(),
            format!("model_reasoning_effort={}", codex::toml_str(effort)),
        ],
        Agent::OpenCode => Vec::new(),
    }
}

/// The overlord's (model, effort) on `agent`: the configured ones when that
/// is the configured agent (they are meant for it), else that agent's
/// defaults.
fn model_effort_for(cfg: &Config, agent: Agent) -> (String, String) {
    if cfg.overlord.agent.trim() == agent.name() {
        (
            cfg.overlord.model.trim().to_string(),
            cfg.overlord.effort.trim().to_string(),
        )
    } else {
        let (m, e) = rlord_core::config::OverlordConfig::defaults_for(agent.name());
        (m.to_string(), e.to_string())
    }
}

/// The overlord's effort on `agent` (see `model_effort_for`).
fn effort_for(cfg: &Config, agent: Agent) -> String {
    model_effort_for(cfg, agent).1
}

/// The effort the overlord was launched with, from the adapter flags in
/// its launch spec ("": the agent's default).
pub fn launched_effort(db: &Db, s: &Session) -> String {
    // What its pane was started with; the spec only before that's known.
    if let Some(argv) = db
        .kv_get(&methods::launched_args_key(&s.id))
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
    {
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        return effort_in_args(&args);
    }
    let spec: Value = db
        .launch_spec(&s.id)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let args: Vec<&str> = spec["extra_args"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    effort_in_args(&args)
}

/// The effort `effort_args` put in `args`, if any.
fn effort_in_args(args: &[&str]) -> String {
    for w in args.windows(2) {
        match w {
            ["--effort", e] => return e.to_string(),
            ["-c", kv] => {
                if let Some(e) = kv.strip_prefix("model_reasoning_effort=") {
                    return e.trim_matches('"').to_string();
                }
            }
            _ => {}
        }
    }
    String::new()
}

/// A subagent definition (`tidy.md`, `import.md`: Claude's format) taken
/// apart: (name, description, instructions).
fn subagent_parts(text: &str) -> (String, String, String) {
    let (head, body) = text
        .strip_prefix("---\n")
        .and_then(|t| t.split_once("\n---\n"))
        .unwrap_or(("", text));
    let field = |k: &str| {
        head.lines()
            .find_map(|l| l.strip_prefix(k))
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    (
        field("name:"),
        field("description:"),
        body.trim().to_string(),
    )
}

/// Write the overlord's folder (instruction files, each agent's MCP config
/// and subagents); return the adapter flags `agent` must always be
/// launched with. Every agent's files are written, whichever runs it: they
/// don't read each other's.
fn write_files(d: &Path, agent: Agent, effort: &str) -> Result<Vec<String>> {
    std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    let bin = methods::rlord_bin();
    // This daemon's socket, as it resolved it: `rlord mcp` is told, so a
    // server started with another environment still reaches this daemon.
    let socket = paths::socket_path().to_string_lossy().into_owned();
    let mcp_path = d.join("mcp.json");
    let mcp = json!({"mcpServers": {"rlord": {
        "type": "stdio",
        "command": bin,
        "args": rlord_core::agents::mcp_args(&socket),
    }}});
    std::fs::write(&mcp_path, serde_json::to_string_pretty(&mcp)?)?;
    // OpenCode: no flags, the folder's config (see `overlord_config`).
    std::fs::write(
        d.join("opencode.json"),
        serde_json::to_string_pretty(&opencode::overlord_config(&bin, &socket))?,
    )?;

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
    // Subagents it delegates to, in each agent's own place and format, and
    // as plain instructions (`SUBAGENT_DIR`) for an overlord that can't
    // start one and does the pass itself.
    let dirs = [
        d.join(".claude").join("agents"),
        d.join(".codex").join("agents"),
        d.join(".opencode").join("agents"),
        d.join(SUBAGENT_DIR),
    ];
    for dir in &dirs {
        std::fs::create_dir_all(dir)?;
    }
    for (file, text) in SUBAGENTS {
        let (name, description, body) = subagent_parts(text);
        std::fs::write(dirs[0].join(file), text)?;
        std::fs::write(
            dirs[1].join(format!("{name}.toml")),
            codex::subagent_toml(&name, &description, &body),
        )?;
        std::fs::write(
            dirs[2].join(file),
            opencode::subagent_md(&description, &body),
        )?;
        std::fs::write(dirs[3].join(file), format!("{body}\n"))?;
    }

    // (No wildcard arm: an agent added to `Agent` doesn't build until it
    // says here how it runs the overlord.)
    let mut args = match agent {
        Agent::Claude => vec![
            "--mcp-config".into(),
            mcp_path.to_string_lossy().into_owned(),
            // The whole point is a cheap, prompt-free control surface. Read-only
            // file tools are pre-allowed for groundwork (reading a repo's README
            // or the files a request names), and Claude's peer messaging for
            // passing work to a session; anything that writes or runs commands
            // still asks.
            "--allowedTools".into(),
            "mcp__rlord,Read,Grep,Glob,ListAgents,SendMessage".into(),
        ],
        // The same surface in Codex's terms: rlord's tools without a
        // prompt, reading free, writes ask. (Its folder as Codex sees it:
        // the resolved path.)
        Agent::Codex => {
            let dir = std::fs::canonicalize(d).unwrap_or_else(|_| d.to_path_buf());
            codex::overlord_args(&bin, &socket, &dir.to_string_lossy())
        }
        Agent::OpenCode => Vec::new(),
    };
    args.extend(effort_args(agent, effort));
    Ok(args)
}

/// Make sure the overlord exists and is running; returns its session.
/// Creates it on first use (with `agent` / `model` / `effort` /
/// `remote_control` from params, else config), restarts it (resuming the
/// conversation) if stopped. An existing overlord's model is its own —
/// changed only via `set_model`.
pub fn ensure(db: &Db, cfg: &Config, params: &Value) -> Result<Value> {
    let v = ensure_running(db, cfg, params)?;
    style(cfg);
    // The user's keys and the window name, on every path that starts it.
    if let Some(s) = session(db)? {
        if let Err(e) = crate::profile::sync(db, cfg, &s, false) {
            tracing::debug!(error = %e, "syncing the overlord's tmux");
        }
    }
    Ok(v)
}

/// The overlay popup's style: a wash of the overlord's colour.
pub fn overlay_style(cfg: &Config) -> String {
    let p = themes::resolve(&cfg.theme, themes::system_is_dark());
    format!("bg={}", p.overlay_bg().tmux())
}

/// The overlord's color in the current theme (tmux spelling).
pub fn color(cfg: &Config) -> String {
    themes::resolve(&cfg.theme, themes::system_is_dark())
        .overlord
        .tmux()
}

/// Its tmux bar: viewed full screen it should be unmistakable, otherwise it
/// looks like any session. Re-run when the theme changes (`overlord.style`).
pub fn style(cfg: &Config) {
    if !tmux::has_session(TMUX_NAME) {
        return;
    }
    let p = themes::resolve(&cfg.theme, themes::system_is_dark());
    let back = if cfg.tmux.back.trim().is_empty() {
        "M-d"
    } else {
        &cfg.tmux.back
    };
    let label = format!(
        "#[bold] ◆ OVERLORD #[nobold]· {} list  ",
        rlord_core::tmux::key_label(back)
    );
    if let Err(e) = tmux::style_session(
        TMUX_NAME,
        &format!("bg={},fg={}", p.overlord.tmux(), p.on_accent.tmux()),
        &label,
    ) {
        tracing::debug!(error = %e, "styling the overlord's session");
    }
}

fn ensure_running(db: &Db, cfg: &Config, params: &Value) -> Result<Value> {
    let pick = |k: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    match session(db)? {
        None => {
            let agent_name = pick("agent").unwrap_or_else(|| cfg.overlord.agent.clone());
            let agent = Agent::named(&agent_name)?;
            let (dflt_model, dflt_effort) = model_effort_for(cfg, agent);
            // ("" given on purpose: the agent's own default.)
            let model = match params.get("model").and_then(Value::as_str) {
                Some(m) => m.trim().to_string(),
                None => dflt_model,
            };
            let effort = match params.get("effort").and_then(Value::as_str) {
                Some(e) => e.trim().to_string(),
                None => dflt_effort,
            };
            let d = default_dir();
            let extra_args = write_files(&d, agent, &effort)?;
            // Created now, with a model the user chose (or the default): never
            // "offer the default" back to it.
            db.kv_set(DEFAULT_DECLINED, cfg.overlord.model.trim())?;
            let remote =
                params["remote_control"].as_bool().unwrap_or(false) && agent.has_remote_control();
            let v = methods::session_new(
                db,
                cfg,
                &json!({
                    "agent": agent.name(),
                    "project": d.to_string_lossy(),
                    "workspace": false,
                    "name": ROLE,
                    "model": model,
                    "role": ROLE,
                    "tmux_name": TMUX_NAME,
                    "extra_args": extra_args,
                    "remote_control": remote,
                }),
            )?;
            // Its first conversation starts now.
            db.kv_set(FRESH_AT, &rlord_core::types::now_ms().to_string())?;
            Ok(v)
        }
        Some(s) => {
            // Fresh instructions and adapter flags (the rlord binary path
            // moved, the effort changed).
            refresh_launch(db, cfg, &s)?;
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
            // An agent that assigns the conversation id itself (Codex)
            // stopped before rlord learned it: look in its index once more
            // (the thread started since the last start, as `discover`
            // would have), and resume what it finds. Only when there is
            // nothing to resume, start anew.
            let unbound = s.agent_session.is_none()
                && !Agent::named(&s.agent)?.mints_id()
                && !crate::discover::bind_now(db, &s)?;
            if unbound {
                let v = methods::session_fresh(db, &json!({"ref": s.id, "reason": "unbound"}))?;
                db.kv_set(FRESH_AT, &rlord_core::types::now_ms().to_string())?;
                return Ok(v);
            }
            methods::session_restart(db, &json!({"ref": s.id}))
        }
    }
}

/// Rewrite the overlord's folder and put the current adapter flags in its
/// launch spec: its next start (a restart, a fresh conversation) has them.
fn refresh_launch(db: &Db, cfg: &Config, s: &Session) -> Result<()> {
    let agent = Agent::named(&s.agent)?;
    let extra_args = write_files(Path::new(&s.project_path), agent, &effort_for(cfg, agent))?;
    let mut spec: Value = serde_json::from_str(&db.launch_spec(&s.id)?).unwrap_or(json!({}));
    spec["extra_args"] = json!(extra_args);
    db.set_launch_spec(&s.id, &spec.to_string())
}

/// Keep the overlord's context from growing without end: once a minute,
/// if it has sat idle past `[overlord] fresh_after_mins` since its last
/// exchange, and nobody is looking at it, restart it on a fresh
/// conversation. Its instructions tell it to keep nothing durable only in
/// its conversation, and to `recall` what it needs from earlier ones.
pub fn start_fresh_keeper(d: &std::sync::Arc<crate::Daemon>) {
    let d = std::sync::Arc::clone(d);
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        // (Read each time: a config reload changes it.)
        let mins = d.config().overlord.fresh_after_mins;
        if mins == 0 {
            continue;
        }
        if let Err(e) = fresh_if_idle(&d, mins) {
            tracing::debug!(error = %e, "overlord fresh start");
        }
    });
}

fn fresh_if_idle(d: &crate::Daemon, mins: u64) -> Result<()> {
    let Some(s) = session(&d.db())? else {
        return Ok(());
    };
    // Only between turns: idle, or "waiting for input" (Claude flips an idle
    // session to that about a minute after its turn). Whether it actually
    // asked the user something is checked below.
    let between_turns = matches!(s.status, Status::Idle | Status::WaitingInput);
    if !between_turns || !s.tmux_session.as_deref().is_some_and(tmux::has_session) {
        return Ok(());
    }
    let now = rlord_core::types::now_ms();
    let last = [
        s.last_user_msg_at,
        s.last_agent_msg_at,
        s.last_state_change_at,
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(now);
    if now - last < (mins as i64) * 60_000 {
        return Ok(());
    }
    // Nothing new since the last fresh start: nothing to clear.
    let fresh_at: i64 = d
        .db()
        .kv_get(FRESH_AT)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if s.last_user_msg_at.is_none_or(|u| u <= fresh_at) {
        return Ok(());
    }
    // Its turn must really be over: the transcript ends in a final reply
    // (not a pending tool call, e.g. a permission prompt mid-turn)...
    let ended = d
        .db()
        .transcript_source(&s.id)?
        .and_then(|p| rlord_core::agents::transcript::transcript_stats(std::path::Path::new(&p)))
        .is_some_and(|st| st.ended);
    if !ended {
        return Ok(());
    }
    // ...and that reply didn't ask the user something (a question, or a form
    // to confirm): then it's waiting on them, whatever the status says.
    if waiting_on_user(d, &s)? {
        return Ok(());
    }
    // Someone is looking at it: its overlay is open, or a terminal shows it.
    let overlay_open = d.overlay.lock().expect("ui mutex").is_some();
    let attached = tmux::list_clients().iter().any(|c| c.session == TMUX_NAME);
    if overlay_open || attached {
        return Ok(());
    }
    // (Where the agent's command is: asked before any lock is taken.)
    rlord_core::agents::install::warm();
    {
        // Nothing may be pasted into the pane while it's being replaced.
        let _send = d.overlord_send.lock().expect("overlord send lock");
        let db = d.db();
        // Re-check under the lock: a request may have just landed.
        let cur = session(&db)?.context("overlord vanished")?;
        if !matches!(cur.status, Status::Idle | Status::WaitingInput)
            || cur.last_user_msg_at != s.last_user_msg_at
        {
            return Ok(());
        }
        // Current instructions and flags (effort may have changed).
        refresh_launch(&db, &d.config(), &s)?;
        crate::methods::session_fresh(
            &db,
            &json!({"ref": s.id, "reason": "idle", "idle_mins": mins}),
        )?;
        db.kv_set(FRESH_AT, &now.to_string())?;
    }
    tracing::info!("overlord idle: restarted on a fresh conversation");
    d.changed();
    Ok(())
}

/// Whether the overlord's last turn waits on the user: its reply ends in a
/// question, or a new-session form it reopened is still out for confirming.
fn waiting_on_user(d: &crate::Daemon, s: &rlord_core::types::Session) -> Result<bool> {
    // A reopened form counts for an hour (it may have been dismissed).
    let pending: i64 = d
        .db()
        .kv_get("confirm_pending")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if rlord_core::types::now_ms() - pending < 60 * 60_000 {
        return Ok(true);
    }
    let Some(src) = d.db().transcript_source(&s.id)? else {
        return Ok(false);
    };
    let ex = rlord_core::agents::transcript::transcript_excerpts(std::path::Path::new(&src));
    Ok(ex
        .last_reply
        .is_some_and(|r| r.trim_end_matches(['…', ' ', '*', '`']).ends_with('?')))
}

/// Everything the startup check and the setup dialog need: does the
/// overlord exist, what model is it set to, what did it actually run, and
/// is that the latest; before it exists, which agents can run it and what
/// each would default to.
pub fn status(db: &Db, cfg: &Config) -> Result<Value> {
    let catalog = |agent: &str| -> Value {
        models::latest_table(agent)
            .iter()
            .map(|(fam, id)| json!({"family": fam, "latest": id}))
            .collect()
    };
    let mut out = json!({
        "default_agent": cfg.overlord.agent,
        "default_model": cfg.overlord.model,
        "default_effort": cfg.overlord.effort,
    });
    let Some(s) = session(db)? else {
        out["exists"] = json!(false);
        // The agents it can run on (installed ones), each with what the
        // dialog offers: its default model and effort, and the choices.
        let agents: Vec<Value> = Agent::ALL
            .into_iter()
            .filter(|a| rlord_core::agents::install::missing(*a).is_none())
            .map(|a| {
                let (model, effort) = model_effort_for(cfg, a);
                json!({
                    "name": a.name(), "label": a.label(),
                    "model": model, "effort": effort,
                    "models": cfg.session.models_for(a),
                    "efforts": rlord_core::config::OverlordConfig::efforts_for(a.name()),
                    "remote_control": a.has_remote_control(),
                    "catalog": catalog(a.name()),
                })
            })
            .collect();
        out["agents"] = json!(agents);
        out["catalog"] = catalog(&cfg.overlord.agent);
        return Ok(out);
    };
    let observed = db
        .transcript_source(&s.id)?
        .and_then(|p| transcript::observed_model(std::path::Path::new(&p)));
    let fresh = models::check(&s.agent, s.model.as_deref(), observed.as_deref());
    let running =
        s.status != Status::Stopped && s.tmux_session.as_deref().is_some_and(tmux::has_session);
    out["exists"] = json!(true);
    out["id"] = json!(s.id);
    out["status"] = json!(s.status);
    out["running"] = json!(running);
    out["agent"] = json!(s.agent);
    out["model"] = json!(s.model);
    out["observed_model"] = json!(observed);
    out["freshness"] = json!(fresh);
    out["catalog"] = catalog(&s.agent);
    // An overlord on another model than the configured default (set up
    // under older defaults, or by hand): offer the default once.
    let (cur, dflt) = (
        s.model.clone().unwrap_or_default(),
        cfg.overlord.model.trim().to_string(),
    );
    let fam = |m: &str| models::parse(m).map(|(f, _)| f);
    let declined = db.kv_get(DEFAULT_DECLINED)?;
    if !dflt.is_empty()
        && s.agent == cfg.overlord.agent.trim()
        && fam(&cur) != fam(&dflt)
        && declined.as_deref() != Some(dflt.as_str())
    {
        out["suggest_default"] = json!(dflt);
    }
    Ok(out)
}

/// Change the overlord's model and restart it onto that model (the
/// conversation resumes).
pub fn set_model(db: &Db, cfg: &Config, params: &Value) -> Result<Value> {
    let model = methods::p_str(params, "model")?.trim().to_string();
    let Some(s) = session(db)? else {
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
    // Current flags too (an effort change applies with the switch).
    refresh_launch(db, cfg, &s)?;
    methods::session_restart(db, &json!({"ref": s.id}))?;
    ensure(db, cfg, &json!({}))
}

/// What `migrate_rows` retired, for the caller to clean up in tmux.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Migrated {
    /// The old front desk's tmux session: to be killed, so its fixed name
    /// is free for the overlord.
    pub front_tmux: Option<String>,
}

/// The name the retired front desk is archived under.
const RETIRED_FRONT: &str = "overlord-front-desk";

/// Turn a two-session overlord (front desk + mastermind) into the one
/// overlord, in the DB. The mastermind's session becomes the overlord: same
/// row, so its conversation, folder, model, Remote Control and history stay;
/// only its role and name change (its tmux session is renamed by
/// `sync_tmux_names`). The old front desk is stopped and archived as an
/// ordinary session. None: there was no mastermind, nothing to do (an
/// overlord that never had one simply stays the overlord). Idempotent.
/// Its writes belong in one transaction (`migrate`).
pub fn migrate_rows(db: &Db) -> Result<Option<Migrated>> {
    // (Renamed whether or not a mastermind ever started.)
    for (old, new) in [
        ("mastermind_fresh_at", FRESH_AT),
        ("default_declined:mastermind", DEFAULT_DECLINED),
    ] {
        if let Some(v) = db.kv_get(old)? {
            // Only the mastermind's row carries these over (below).
            if db.role_session(OLD_MASTERMIND)?.is_some() {
                db.kv_set(new, &v)?;
            }
            db.kv_delete(old)?;
        }
    }
    // The setup dialog's pick for a mastermind not started yet.
    db.kv_delete("mastermind_model")?;
    // Remote Control lived on the mastermind, remembered here for one not
    // started yet: now it is the overlord's own wish, like any session's.
    // (The kv goes once the wish is folded in; the caller commits both
    // together.)
    let remote = db.kv_get("mastermind_remote")?.as_deref() == Some("1");
    let Some(mind) = db.role_session(OLD_MASTERMIND)? else {
        if let Some(front) = db.role_session(ROLE)?.filter(|_| remote) {
            crate::remote::want(db, &front)?;
        }
        db.kv_delete("mastermind_remote")?;
        return Ok(None);
    };
    let now = rlord_core::types::now_ms();
    let mut out = Migrated::default();
    if let Some(front) = db.role_session(ROLE)? {
        out.front_tmux = front.tmux_session.clone();
        db.retire_role(&front.id, RETIRED_FRONT, now)?;
        db.insert_event(
            &front.id,
            now,
            "archived",
            &json!({"why": "the overlord is one session now: the mastermind's conversation is the overlord's"})
                .to_string(),
        )?;
    }
    db.set_role(&mind.id, ROLE, ROLE)?;
    // Shown in the Claude app under the overlord's name from its next start.
    let mut spec: Value = serde_json::from_str(&db.launch_spec(&mind.id)?).unwrap_or(json!({}));
    if spec["remote_control"]
        .as_str()
        .is_some_and(|n| !n.is_empty())
    {
        spec["remote_control"] = json!(crate::remote::remote_name(ROLE, ROLE));
        db.set_launch_spec(&mind.id, &spec.to_string())?;
    } else if remote {
        crate::remote::want(db, &db.resolve_ref(&mind.id)?)?;
    }
    db.kv_delete("mastermind_remote")?;
    db.insert_event(
        &mind.id,
        now,
        "became_overlord",
        &json!({"from": OLD_MASTERMIND}).to_string(),
    )?;
    tracing::info!(session = %mind.id, "the mastermind is now the overlord (one session)");
    Ok(Some(out))
}

/// Make the overlord one session at daemon start: `migrate_rows`, the
/// config's old `mastermind_*` keys folded in, and the overlord's tmux name
/// freed for it (the mastermind's session is renamed onto it by
/// `sync_tmux_names`, which runs next).
pub fn migrate(db: &Db) -> Result<()> {
    migrate_with(db, &tmux::has_session, &tmux::kill_session)?;
    match rlord_core::config::fold_mastermind_keys_in(&paths::config_path()) {
        Ok(moved) if !moved.is_empty() => {
            tracing::info!(keys = ?moved, "config: the mastermind's settings are the overlord's now")
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "moving mastermind_* keys in config.toml"),
    }
    Ok(())
}

/// `migrate`'s DB and tmux part, with tmux passed in (tests).
fn migrate_with(
    db: &Db,
    live: &dyn Fn(&str) -> bool,
    kill: &dyn Fn(&str) -> Result<()>,
) -> Result<()> {
    // The row changes commit only once the old front desk's tmux session is
    // gone: a failed kill (or a crash before it) must not leave a retired,
    // archived row holding a live session under the overlord's name.
    db.atomically(|| {
        if let Some(t) = migrate_rows(db)?.and_then(|m| m.front_tmux) {
            if live(&t) {
                kill(&t).with_context(|| format!("stopping the old front desk ({t})"))?;
            }
        }
        Ok(())
    })?;
    // A session under the overlord's name that no row claims (the front desk
    // of an earlier, interrupted migration): stop it, so the overlord's
    // session can be renamed onto the name.
    if let Some(o) = session(db)? {
        if o.tmux_session.as_deref() != Some(TMUX_NAME)
            && !db.tmux_name_taken(TMUX_NAME)?
            && live(TMUX_NAME)
        {
            tracing::info!("stopping an unclaimed {TMUX_NAME} tmux session");
            kill(TMUX_NAME)?;
        }
    }
    Ok(())
}

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
                !INSTRUCTIONS.contains(&q) && !SUBAGENTS.iter().any(|(_, t)| t.contains(&q))
            })
            .collect();
        assert!(
            missing.is_empty(),
            "add to crates/daemon/src/overlord.md: {missing:?}"
        );
    }

    /// The instructions name no tool that doesn't exist (a removed tool
    /// left behind makes the overlord call it), and nothing of the old
    /// two-session overlord.
    #[test]
    fn instructions_name_only_tools_that_exist() {
        let names: Vec<String> = crate::mcp::tools()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        for gone in ["hand_off", "mastermind", "front desk", "Alt-M", "Option-M"] {
            assert!(
                !INSTRUCTIONS.contains(gone),
                "overlord.md still mentions {gone}"
            );
        }
        // Every `snake_case` word in backticks under "What the tools do" is
        // a tool.
        let section = INSTRUCTIONS
            .split("## What the tools do")
            .nth(1)
            .and_then(|s| s.split("\n## ").next())
            .expect("a tools section");
        for line in section.lines().filter(|l| l.starts_with("- `")) {
            let head = line.split('—').next().unwrap_or(line);
            for name in head.split('`').skip(1).step_by(2) {
                assert!(
                    names.iter().any(|n| n == name),
                    "overlord.md lists a tool that doesn't exist: {name}"
                );
            }
        }
    }

    /// Subagents may only use rlord tools that exist.
    #[test]
    fn subagent_tools_exist() {
        let names: Vec<String> = crate::mcp::tools()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str().map(|n| format!("mcp__rlord__{n}")))
            .collect();
        for (file, text) in SUBAGENTS {
            let line = text
                .lines()
                .find_map(|l| l.strip_prefix("tools:"))
                .unwrap_or_else(|| panic!("no tools line in {file}"));
            for t in line.split(',').map(str::trim) {
                assert!(names.iter().any(|n| n == t), "unknown tool in {file}: {t}");
            }
        }
    }

    /// Each agent gets rlord's MCP server with its tools pre-allowed, in
    /// its own terms, and its subagents in its own place.
    #[test]
    fn the_folder_is_written_for_every_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("overlord");
        let claude = write_files(&d, Agent::Claude, "low").unwrap();
        assert_eq!(claude[0], "--mcp-config");
        assert!(claude.contains(&"--allowedTools".to_string()));
        assert_eq!(
            effort_in_args(&claude.iter().map(String::as_str).collect::<Vec<_>>()),
            "low"
        );

        let codex = write_files(&d, Agent::Codex, "low").unwrap();
        let has = |needle: &str| codex.iter().any(|a| a.contains(needle));
        assert!(has("mcp_servers.rlord.command="));
        assert!(has(
            "mcp_servers.rlord.default_tools_approval_mode=\"approve\""
        ));
        assert!(has("sandbox_mode=\"read-only\""));
        assert!(has("approval_policy=\"untrusted\""));
        assert!(has(&format!(
            "mcp_servers.rlord.args=[\"mcp\",\"--socket\",\"{}\"]",
            paths::socket_path().display()
        )));
        assert!(has("model_reasoning_effort=\"low\""));
        // Never the flags that skip approvals or hook trust.
        assert!(!has("dangerously") && !has("never") && !has("--mcp-config"));
        // Its folder, as Codex sees it (resolved), is trusted for the run.
        let real = std::fs::canonicalize(&d).unwrap();
        assert!(has(&format!("projects={{\"{}\"=", real.display())));

        // OpenCode: no flags at all; the folder's config.
        assert!(write_files(&d, Agent::OpenCode, "").unwrap().is_empty());
        let cfg: Value =
            serde_json::from_str(&std::fs::read_to_string(d.join("opencode.json")).unwrap())
                .unwrap();
        assert_eq!(cfg["mcp"]["servers"]["rlord"]["command"][1], "mcp");
        assert_eq!(cfg["mcp"]["servers"]["rlord"]["command"][2], "--socket");
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(d.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"]["rlord"]["args"][1], "--socket");

        for f in [
            "CLAUDE.md",
            "AGENTS.md",
            "GEMINI.md",
            "mcp.json",
            ".claude/agents/rlord-tidy.md",
            ".codex/agents/rlord-tidy.toml",
            ".codex/agents/rlord-import.toml",
            ".opencode/agents/rlord-tidy.md",
            "subagents/rlord-tidy.md",
            "subagents/rlord-import.md",
        ] {
            assert!(d.join(f).is_file(), "{f}");
        }
        // The plain copy is instructions only; the instructions point at it.
        let plain = std::fs::read_to_string(d.join("subagents/rlord-tidy.md")).unwrap();
        assert!(!plain.starts_with("---") && plain.contains("triage"));
        assert!(INSTRUCTIONS.contains("`subagents/rlord-tidy.md`"));
        assert!(INSTRUCTIONS.contains("`subagents/rlord-import.md`"));
        let toml = std::fs::read_to_string(d.join(".codex/agents/rlord-tidy.toml")).unwrap();
        assert!(toml.starts_with("name = \"rlord-tidy\"\n"));
    }

    #[test]
    fn a_subagent_file_comes_apart() {
        let (name, description, body) = subagent_parts(SUBAGENTS[0].1);
        assert_eq!(name, "rlord-tidy");
        assert!(description.starts_with("Tidy rlord's session list"));
        assert!(body.starts_with("You keep the user's rlord session list tidy"));
        assert!(!body.contains("omitClaudeMd"));
    }

    #[test]
    fn effort_flags_per_agent() {
        assert_eq!(effort_args(Agent::Claude, "low"), ["--effort", "low"]);
        // And read back from a launch spec's flags.
        for (agent, e) in [(Agent::Claude, "high"), (Agent::Codex, "low")] {
            let args = effort_args(agent, e);
            let mut all = vec!["--mcp-config", "/x/mcp.json"];
            all.extend(args.iter().map(String::as_str));
            assert_eq!(effort_in_args(&all), e);
        }
        assert_eq!(effort_in_args(&["--mcp-config", "/x"]), "");
        assert_eq!(
            effort_args(Agent::Codex, " low "),
            ["-c", "model_reasoning_effort=\"low\""]
        );
        assert!(effort_args(Agent::OpenCode, "low").is_empty());
        assert!(effort_args(Agent::Claude, "").is_empty());
    }

    fn row(db: &Db, id: &str, role: &str, tmux: &str, dir: &str) {
        db.insert_session(&rlord_core::db::NewSession {
            id,
            name: role,
            agent: "claude",
            agent_session: Some(&format!("conv-{id}")),
            project_path: dir,
            tmux_session: Some(tmux),
            launch_spec: r#"{"model":"opus","remote_control":"rlord mastermind"}"#,
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role,
        })
        .unwrap();
    }

    #[test]
    fn the_mastermind_becomes_the_overlord() {
        let db = Db::open_in_memory().unwrap();
        row(&db, "01FRONT", "overlord", "overlord-rlord", "/d/overlord");
        row(
            &db,
            "01MIND",
            "mastermind",
            "mastermind-rlord",
            "/d/mastermind",
        );
        db.kv_set("mastermind_fresh_at", "42").unwrap();
        db.kv_set("default_declined:mastermind", "opus").unwrap();
        db.kv_set("mastermind_model", "fable").unwrap();
        db.insert_event(
            "01MIND",
            5,
            "fresh_conversation",
            r#"{"from":"old","to":"conv-01MIND"}"#,
        )
        .unwrap();

        let m = migrate_rows(&db).unwrap().expect("migrated");
        assert_eq!(m.front_tmux.as_deref(), Some("overlord-rlord"));
        // The mastermind's row is the overlord: its conversation, folder,
        // model and Remote Control wish are untouched.
        let o = db.overlord().unwrap().expect("an overlord");
        assert_eq!(o.id, "01MIND");
        assert_eq!(o.name, "overlord");
        assert_eq!(o.agent_session.as_deref(), Some("conv-01MIND"));
        assert_eq!(o.project_path, "/d/mastermind");
        assert!(db
            .launch_spec("01MIND")
            .unwrap()
            .contains(r#""remote_control":"rlord overlord""#));
        assert_eq!(db.past_agent_sessions("01MIND").unwrap(), ["old"]);
        assert!(db.role_session("mastermind").unwrap().is_none());
        // The front desk: an ordinary archived, stopped session.
        let f = db.resolve_ref("01FRONT").unwrap();
        assert_eq!(f.role, "");
        assert_eq!(f.name, RETIRED_FRONT);
        assert_eq!(f.status, Status::Stopped);
        assert!(f.archived_at.is_some() && f.tmux_session.is_none());
        // kv renamed.
        assert_eq!(db.kv_get(FRESH_AT).unwrap().as_deref(), Some("42"));
        assert_eq!(
            db.kv_get(DEFAULT_DECLINED).unwrap().as_deref(),
            Some("opus")
        );
        for gone in [
            "mastermind_fresh_at",
            "default_declined:mastermind",
            "mastermind_model",
        ] {
            assert_eq!(db.kv_get(gone).unwrap(), None, "{gone}");
        }
        // Again: nothing to do.
        assert_eq!(migrate_rows(&db).unwrap(), None);
        assert_eq!(db.overlord().unwrap().unwrap().id, "01MIND");
    }

    /// The front desk's tmux session is stopped before the rows commit: a
    /// failed kill leaves everything as it was, to retry on the next start.
    #[test]
    fn a_failed_kill_rolls_the_migration_back() {
        use std::cell::RefCell;
        let db = Db::open_in_memory().unwrap();
        row(&db, "01FRONT", "overlord", "overlord-rlord", "/d/overlord");
        row(&db, "01MIND", "mastermind", "mastermind-rlord", "/d/mm");
        db.kv_set("mastermind_remote", "1").unwrap();
        let fail = |_: &str| -> Result<()> { anyhow::bail!("tmux said no") };
        assert!(migrate_with(&db, &|_| true, &fail).is_err());
        assert_eq!(db.overlord().unwrap().unwrap().id, "01FRONT");
        assert!(db.role_session("mastermind").unwrap().is_some());
        let f = db.resolve_ref("01FRONT").unwrap();
        assert_eq!(f.tmux_session.as_deref(), Some("overlord-rlord"));
        assert_eq!(
            db.kv_get("mastermind_remote").unwrap().as_deref(),
            Some("1")
        );
        // It works next time: killed, then committed.
        let killed = RefCell::new(Vec::new());
        let kill = |t: &str| -> Result<()> {
            killed.borrow_mut().push(t.to_string());
            Ok(())
        };
        let live = |t: &str| t == "overlord-rlord" && !killed.borrow().iter().any(|k| k == t);
        migrate_with(&db, &live, &kill).unwrap();
        assert_eq!(*killed.borrow(), ["overlord-rlord"]);
        assert_eq!(db.overlord().unwrap().unwrap().id, "01MIND");
        assert_eq!(db.kv_get("mastermind_remote").unwrap(), None);
    }

    /// An interrupted migration left a live session under the overlord's
    /// name that no row claims: it is stopped so the overlord's can take
    /// the name. One a row claims is left alone.
    #[test]
    fn an_unclaimed_overlord_tmux_session_is_stopped() {
        use std::cell::RefCell;
        let db = Db::open_in_memory().unwrap();
        row(&db, "01MIND", "overlord", "mastermind-rlord", "/d/mm");
        let killed = RefCell::new(Vec::new());
        let kill = |t: &str| -> Result<()> {
            killed.borrow_mut().push(t.to_string());
            Ok(())
        };
        migrate_with(&db, &|_| true, &kill).unwrap();
        assert_eq!(*killed.borrow(), [TMUX_NAME]);
        // Claimed by a row: kept.
        killed.borrow_mut().clear();
        row(&db, "01OTHER", "", TMUX_NAME, "/d/x");
        migrate_with(&db, &|_| true, &kill).unwrap();
        assert!(killed.borrow().is_empty());
        // The overlord already has the name: nothing to do.
        let db = Db::open_in_memory().unwrap();
        row(&db, "01O", "overlord", TMUX_NAME, "/d/o");
        migrate_with(&db, &|_| true, &kill).unwrap();
        assert!(killed.borrow().is_empty());
    }

    #[test]
    fn an_overlord_without_a_mastermind_stays() {
        let db = Db::open_in_memory().unwrap();
        row(&db, "01FRONT", "overlord", "overlord-rlord", "/d/overlord");
        db.set_launch_spec("01FRONT", r#"{"model":"sonnet"}"#)
            .unwrap();
        db.kv_set(DEFAULT_DECLINED, "sonnet").unwrap();
        db.kv_set("mastermind_fresh_at", "42").unwrap();
        db.kv_set("mastermind_remote", "1").unwrap();
        assert_eq!(migrate_rows(&db).unwrap(), None);
        // Remote Control chosen for a mastermind that never started: the
        // overlord's own now.
        assert!(db
            .launch_spec("01FRONT")
            .unwrap()
            .contains(r#""remote_control":"rlord overlord""#));
        assert_eq!(db.kv_get("mastermind_remote").unwrap(), None);
        let o = db.overlord().unwrap().unwrap();
        assert_eq!((o.id.as_str(), o.archived_at), ("01FRONT", None));
        // Its own "declined" stays; the unused mastermind's keys go.
        assert_eq!(
            db.kv_get(DEFAULT_DECLINED).unwrap().as_deref(),
            Some("sonnet")
        );
        assert_eq!(db.kv_get("mastermind_fresh_at").unwrap(), None);
        assert_eq!(db.kv_get(FRESH_AT).unwrap(), None);
        // No overlord at all: nothing happens either.
        assert_eq!(migrate_rows(&Db::open_in_memory().unwrap()).unwrap(), None);
    }
}
