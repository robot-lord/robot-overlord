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
use rlord_core::themes;
use rlord_core::types::Status;
use rlord_core::{models, paths, tmux};

use crate::methods;

/// One of the overlord's two sessions. The **overlord** is the quick front
/// desk (Option-O): mechanical work itself, everything that needs thought
/// handed to the **mastermind** (Option-M), which launches sessions and takes
/// one-off questions. Each has its own folder, instructions, model, effort
/// and fixed tmux name.
pub struct Seat {
    /// Role in the DB, display name and folder name.
    pub key: &'static str,
    /// Fixed tmux session name (the overlay embeds its pane).
    pub tmux: &'static str,
    instructions: &'static str,
    subagents: &'static [(&'static str, &'static str)],
    /// Its model and effort in the config.
    model_of: fn(&Config) -> &str,
    effort_of: fn(&Config) -> &str,
}

pub const FRONT: Seat = Seat {
    key: "overlord",
    tmux: "rlord_overlord",
    instructions: include_str!("overlord.md"),
    subagents: &[
        ("rlord-tidy.md", include_str!("tidy.md")),
        ("rlord-import.md", include_str!("import.md")),
    ],
    model_of: |c| &c.overlord.model,
    effort_of: |c| &c.overlord.effort,
};

pub const MASTERMIND: Seat = Seat {
    key: "mastermind",
    tmux: "rlord_mastermind",
    instructions: include_str!("mastermind.md"),
    subagents: &[],
    model_of: |c| &c.overlord.mastermind_model,
    effort_of: |c| &c.overlord.mastermind_effort,
};

pub const SEATS: [&Seat; 2] = [&FRONT, &MASTERMIND];

/// The front desk's tmux name (the overlord most code means).
pub const TMUX_NAME: &str = FRONT.tmux;

/// A seat by key (`overlord` / `mastermind`); anything else is the front.
pub fn seat(key: &str) -> &'static Seat {
    SEATS.into_iter().find(|s| s.key == key).unwrap_or(&FRONT)
}

/// Whether a session role is one of the overlord's seats (fixed rows: no
/// rename, tags, notes, archive).
pub fn is_overlord_role(role: &str) -> bool {
    SEATS.iter().any(|s| s.key == role)
}

impl Seat {
    pub fn dir(&self) -> PathBuf {
        paths::data_dir().join(self.key)
    }
    fn model<'a>(&self, cfg: &'a Config) -> &'a str {
        (self.model_of)(cfg)
    }
    fn effort<'a>(&self, cfg: &'a Config) -> &'a str {
        (self.effort_of)(cfg)
    }
    pub fn session(&self, db: &Db) -> Result<Option<rlord_core::types::Session>> {
        db.role_session(self.key)
    }
}

/// User-authored standing instructions appended to the generated ones.
pub fn user_instructions_path() -> PathBuf {
    paths::config_path()
        .parent()
        .map(|p| p.join("overlord.md"))
        .unwrap_or_else(|| PathBuf::from("overlord.md"))
}

/// Write a seat's folder (instruction files, mcp.json, subagents); return
/// the adapter flags it must always be launched with.
fn write_files(seat: &Seat, cfg: &Config) -> Result<Vec<String>> {
    let d = seat.dir();
    std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    let mcp_path = d.join("mcp.json");
    let mcp = json!({"mcpServers": {"rlord": {
        "type": "stdio",
        "command": methods::rlord_bin(),
        "args": ["mcp"],
    }}});
    std::fs::write(&mcp_path, serde_json::to_string_pretty(&mcp)?)?;

    let mut md = format!("{GENERATED_NOTE}{}", seat.instructions);
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
    // Subagents the seat delegates to (Claude's project agents dir).
    let agents = d.join(".claude").join("agents");
    std::fs::create_dir_all(&agents)?;
    for (file, text) in seat.subagents {
        std::fs::write(agents.join(file), text)?;
    }

    let mut args = vec![
        "--mcp-config".into(),
        mcp_path.to_string_lossy().into_owned(),
        // The whole point is a cheap, prompt-free control surface. Read-only
        // file tools are pre-allowed for groundwork (reading a repo's README
        // or the files a request names); anything that writes or runs
        // commands still asks.
        "--allowedTools".into(),
        "mcp__rlord,Read,Grep,Glob".into(),
    ];
    let effort = seat.effort(cfg).trim();
    if !effort.is_empty() {
        args.push("--effort".into());
        args.push(effort.into());
    }
    Ok(args)
}

/// Make sure the overlord exists and is running; returns its session.
/// Creates it on first use (with `agent`/`model` from params, else config),
/// restarts it (resuming the conversation) if stopped. An existing
/// overlord's model is its own — changed only via `set_model`.
pub fn ensure(db: &Db, cfg: &Config, params: &Value) -> Result<Value> {
    let seat = seat(params["seat"].as_str().unwrap_or(FRONT.key));
    let v = ensure_running(db, cfg, seat, params)?;
    style(cfg);
    // The user's keys and the window name, on every path that starts a seat
    // (the mastermind starts lazily from a hand-off, the overlay or goto).
    if let Some(s) = seat.session(db)? {
        if let Err(e) = crate::profile::sync(db, cfg, &s, false) {
            tracing::debug!(error = %e, seat = seat.key, "syncing overlord seat's tmux");
        }
    }
    Ok(v)
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
    let p = themes::resolve(&cfg.theme, themes::system_is_dark());
    let keys = |k: &str| rlord_core::tmux::key_label(k);
    let back = if cfg.tmux.back.trim().is_empty() {
        "M-d"
    } else {
        &cfg.tmux.back
    };
    for (seat, label) in [
        (
            &FRONT,
            format!(
                "#[bold] ◆ OVERLORD #[nobold]· {}{} list  ",
                // Only a key that works here (the global one, if bound).
                if cfg.tmux.mastermind.trim().is_empty() {
                    String::new()
                } else {
                    format!("{} mastermind · ", keys(&cfg.tmux.mastermind))
                },
                keys(back)
            ),
        ),
        (
            &MASTERMIND,
            format!(
                "#[bold] ◆ MASTERMIND #[nobold]· {} overlord · {} list  ",
                keys(&cfg.tmux.overlord),
                keys(back)
            ),
        ),
    ] {
        if !tmux::has_session(seat.tmux) {
            continue;
        }
        if let Err(e) = tmux::style_session(
            seat.tmux,
            &format!("bg={},fg={}", p.overlord.tmux(), p.on_accent.tmux()),
            &label,
        ) {
            tracing::debug!(error = %e, seat = seat.key, "styling overlord session");
        }
    }
}

fn ensure_running(db: &Db, cfg: &Config, seat: &Seat, params: &Value) -> Result<Value> {
    let extra_args = write_files(seat, cfg)?;
    let pick = |k: &str, dflt: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(dflt)
            .to_string()
    };
    // The setup dialog picks the mastermind's model too, before it exists.
    // (Kept only when it differs from the config's default, so a later
    // config change still applies to a mastermind not yet started.)
    if let Some(m) = params["mastermind_model"].as_str().map(str::trim) {
        if !m.is_empty() && m != MASTERMIND.model(cfg).trim() {
            db.kv_set("mastermind_model", m)?;
        } else {
            db.kv_set("mastermind_model", "")?;
        }
    }
    match seat.session(db)? {
        None => {
            let chosen = if seat.key == MASTERMIND.key {
                db.kv_get("mastermind_model")?.filter(|m| !m.is_empty())
            } else {
                None
            };
            let model = pick("model", chosen.as_deref().unwrap_or(seat.model(cfg)).trim());
            // Created now, with a model the user chose (or the default): never
            // "offer the default" back to it.
            db.kv_set(
                &format!("default_declined:{}", seat.key),
                seat.model(cfg).trim(),
            )?;
            let agent = pick("agent", &cfg.overlord.agent);
            let d = seat.dir();
            methods::session_new(
                db,
                cfg,
                &json!({
                    "agent": agent,
                    "project": d.to_string_lossy(),
                    "workspace": false,
                    "name": seat.key,
                    "model": model,
                    "role": seat.key,
                    "tmux_name": seat.tmux,
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

/// Keep the mastermind's context from growing without end: once a minute,
/// if it has sat idle past `[overlord] mastermind_fresh_after_mins` since its
/// last exchange, and nobody is looking at it, restart it on a fresh
/// conversation. Its instructions tell it to keep nothing durable only in its
/// conversation.
pub fn start_fresh_keeper(d: &std::sync::Arc<crate::Daemon>) {
    let mins = d.config.overlord.mastermind_fresh_after_mins;
    if mins == 0 {
        return;
    }
    let d = std::sync::Arc::clone(d);
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        if let Err(e) = fresh_if_idle(&d, mins) {
            tracing::debug!(error = %e, "mastermind fresh start");
        }
    });
}

fn fresh_if_idle(d: &crate::Daemon, mins: u64) -> Result<()> {
    let Some(s) = MASTERMIND.session(&d.db())? else {
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
        .kv_get("mastermind_fresh_at")?
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
        .and_then(|p| rlord_core::agents::claude::transcript_stats(std::path::Path::new(&p)))
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
    let overlay_on_it = d.overlay.lock().expect("ui mutex").is_some()
        && *d.overlay_seat.lock().expect("ui mutex") == MASTERMIND.key;
    let attached = tmux::list_clients()
        .iter()
        .any(|c| c.session == MASTERMIND.tmux);
    if overlay_on_it || attached {
        return Ok(());
    }
    {
        // No hand-off may paste into the pane while it's being replaced.
        let _send = d.mastermind_send.lock().expect("mastermind send lock");
        let db = d.db();
        // Re-check under the lock: a hand-off may have just landed.
        let cur = MASTERMIND.session(&db)?.context("mastermind vanished")?;
        if !matches!(cur.status, Status::Idle | Status::WaitingInput)
            || cur.last_user_msg_at != s.last_user_msg_at
        {
            return Ok(());
        }
        // Current instructions and flags (effort may have changed).
        let extra_args = write_files(&MASTERMIND, &d.config)?;
        let mut spec: Value = serde_json::from_str(&db.launch_spec(&s.id)?).unwrap_or(json!({}));
        spec["extra_args"] = json!(extra_args);
        db.set_launch_spec(&s.id, &spec.to_string())?;
        crate::methods::session_fresh(&db, &json!({"ref": s.id}))?;
        db.kv_set("mastermind_fresh_at", &now.to_string())?;
    }
    tracing::info!("mastermind idle: restarted on a fresh conversation");
    d.changed();
    Ok(())
}

/// Whether the mastermind's last turn waits on the user: its reply ends in a
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
    let ex = rlord_core::agents::claude::transcript_excerpts(std::path::Path::new(&src));
    Ok(ex
        .last_reply
        .is_some_and(|r| r.trim_end_matches(['…', ' ', '*', '`']).ends_with('?')))
}

/// Everything the startup check needs: does the overlord exist, what models
/// are its seats set to, what did they actually run, and is that the latest.
/// The front desk's fields are top-level (as before); the mastermind's are
/// under `mastermind`.
pub fn status(db: &Db, cfg: &Config) -> Result<Value> {
    let catalog = |agent: &str| -> Value {
        models::latest_table(agent)
            .iter()
            .map(|(fam, id)| json!({"family": fam, "latest": id}))
            .collect()
    };
    let mut out = seat_status(db, cfg, &FRONT)?;
    if out["exists"] != true {
        out["agents"] = json!(["claude"]);
        out["catalog"] = catalog(&cfg.overlord.agent);
    } else {
        let agent = out["agent"].as_str().unwrap_or("claude").to_string();
        out["catalog"] = catalog(&agent);
    }
    out["mastermind"] = seat_status(db, cfg, &MASTERMIND)?;
    // An overlord created before the two-seat split (or by hand) on another
    // model than the configured default: offer the default once.
    for (k, seat) in [("", &FRONT), ("mastermind", &MASTERMIND)] {
        let st = if k.is_empty() { &mut out } else { &mut out[k] };
        let (cur, dflt) = (
            st["model"].as_str().unwrap_or("").to_string(),
            st["default_model"].as_str().unwrap_or("").to_string(),
        );
        let fam = |m: &str| models::parse(m).map(|(f, _)| f);
        let declined = db.kv_get(&format!("default_declined:{}", seat.key))?;
        if st["exists"] == true
            && !dflt.is_empty()
            && fam(&cur) != fam(&dflt)
            && declined.as_deref() != Some(dflt.as_str())
        {
            st["suggest_default"] = json!(dflt);
        }
    }
    Ok(out)
}

fn seat_status(db: &Db, cfg: &Config, seat: &Seat) -> Result<Value> {
    let defaults = json!({
        "default_agent": cfg.overlord.agent,
        "default_model": seat.model(cfg),
        "default_effort": seat.effort(cfg),
    });
    let Some(s) = seat.session(db)? else {
        let mut v = defaults;
        v["exists"] = json!(false);
        return Ok(v);
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
    let mut v = json!({
        "exists": true,
        "id": s.id,
        "status": s.status,
        "running": running,
        "agent": s.agent,
        "model": s.model,
        "observed_model": observed,
        "freshness": fresh,
    });
    for (k, val) in defaults.as_object().into_iter().flatten() {
        v[k] = val.clone();
    }
    Ok(v)
}

/// Change a seat's model (`seat`: overlord by default) and restart it onto
/// that model (the conversation resumes).
pub fn set_model(db: &Db, cfg: &Config, params: &Value) -> Result<Value> {
    let model = methods::p_str(params, "model")?.trim().to_string();
    let seat = seat(params["seat"].as_str().unwrap_or(FRONT.key));
    let Some(s) = seat.session(db)? else {
        return ensure(db, cfg, &json!({"model": model, "seat": seat.key}));
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
    spec["extra_args"] = json!(write_files(seat, cfg)?);
    db.set_launch_spec(&s.id, &spec.to_string())?;
    methods::session_restart(db, &json!({"ref": s.id}))?;
    ensure(db, cfg, &json!({"seat": seat.key}))
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
                !SEATS.iter().any(|seat| {
                    seat.instructions.contains(&q)
                        || seat.subagents.iter().any(|(_, t)| t.contains(&q))
                })
            })
            .collect();
        assert!(
            missing.is_empty(),
            "add to crates/daemon/src/overlord.md: {missing:?}"
        );
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
        for (file, text) in SEATS.iter().flat_map(|s| s.subagents.iter()) {
            let line = text
                .lines()
                .find_map(|l| l.strip_prefix("tools:"))
                .unwrap_or_else(|| panic!("no tools line in {file}"));
            for t in line.split(',').map(str::trim) {
                assert!(names.iter().any(|n| n == t), "unknown tool in {file}: {t}");
            }
        }
    }
}
