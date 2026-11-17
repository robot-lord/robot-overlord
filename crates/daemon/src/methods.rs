//! RPC method implementations. All synchronous; called under the Db mutex via
//! `Daemon::call`. tmux operations are subprocess calls in the low-ms range.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::{claude, codex, install, transcript, Agent, Effect, Indexed};
use rlord_core::config::{expand_tilde, Config};
use rlord_core::db::{Db, NewSession, SessionQuery};
use rlord_core::types::{new_id, now_ms, Session, Status, Subagent};
use rlord_core::{git, paths, tmux};

use crate::context;

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
/// followed by repos discovered under `roots` (the finder's, see
/// `Daemon::finder_roots`), deduplicated.
/// `scanning`: the disk scan is still running, so the list may be short.
/// `skipped`: folders the scan stayed out of (`git::scan_repos`).
pub(crate) fn repo_find(
    db: &Db,
    cfg: &Config,
    roots: &[String],
    found: crate::repo_scan::Found,
) -> Result<Value> {
    let mut out = repo_list(db, cfg)?;
    let known: std::collections::HashSet<String> = out
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["path"].as_str().map(str::to_string))
        .collect();
    for p in found.scan.repos {
        if !known.contains(&p) {
            out.as_array_mut()
                .unwrap()
                .push(json!({"path": p, "default": false, "last_used_at": null}));
        }
    }
    let roots: Vec<String> = roots.iter().map(|r| expand_tilde(r)).collect();
    Ok(
        json!({"repos": out, "roots": roots, "depth": cfg.session.repo_scan_depth,
              "scanning": found.scanning,
              "skipped": found.scan.skipped.iter().map(|k| json!({
                  "path": k.path, "why": k.why.id(), "text": k.why.text(),
              })).collect::<Vec<_>>(),
              "skipped_more": found.scan.skipped_more,
              "code_dir": cfg.session.code_dir_path().unwrap_or_default()}),
    )
}

/// `[session]` as configured, plus what each agent offers: `agents` lists
/// every agent rlord can run with its default model and picker choices
/// (`models` empty: no catalog, the model is typed in). `code_dir` is
/// expanded; `repo_roots` is as configured, and `scan_roots` (expanded) is
/// what the finder scans now, which is not a setting to pass back;
/// `setup_needed`: no code folder yet and setup not answered.
pub fn session_defaults(cfg: &Config, setup_needed: bool) -> Result<Value> {
    let mut v = serde_json::to_value(&cfg.session)?;
    v["code_dir"] = json!(cfg.session.code_dir_path().unwrap_or_default());
    v["scan_roots"] = json!(cfg
        .session
        .finder_roots(setup_needed)
        .iter()
        .map(|r| expand_tilde(r))
        .collect::<Vec<_>>());
    if cfg.session.scan_roots().is_empty() && setup_needed {
        v["scan_note"] =
            json!("no code folder yet: the finder scans ~ until setup is done (confirm_setup)");
    }
    v["setup_needed"] = json!(setup_needed);
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
pub(crate) fn refuse_overlord(s: &rlord_core::types::Session, what: &str) -> Result<()> {
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
    // Its tmux session follows the name, and its background file (which
    // names it).
    let s = db.resolve_ref(&s.id)?;
    // ... and the files of the sessions that list it as related.
    let linkers = db.context_linkers(&s.id).unwrap_or_default();
    for other in
        std::iter::once(s.clone()).chain(linkers.iter().filter_map(|id| db.resolve_ref(id).ok()))
    {
        if let Err(e) = context::refresh_file(db, &paths::data_dir(), &other) {
            tracing::warn!(session = %other.id, error = %e, "rewriting the background file");
        }
    }
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

/// Add background for a session: a note (`context`) and links to related
/// sessions (`related`), from the overlord. Its background file is
/// rewritten; the agent gets the new part with its next prompt (hooks) or
/// reads the file.
pub fn session_context_add(db: &Db, params: &Value) -> Result<Value> {
    session_context_add_in(db, params, &paths::data_dir())
}

/// `session_context_add`, with the session files under `base`.
pub(crate) fn session_context_add_in(
    db: &Db,
    params: &Value,
    base: &std::path::Path,
) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    refuse_overlord(&s, "given background")?;
    let (note, links) = context::check(
        db,
        params["context"].as_str(),
        &ref_list(params, "related"),
        Some(&s.id),
    )?;
    let hooks = params.get("hooks_work").and_then(Value::as_bool);
    context_store(db, &s, note, &links, base, hooks)
}

/// Store background already through `context::check` (`update_session`
/// checks before its other edits and stores after them, in-process: never
/// taken from a caller over RPC), rewrite the file (best effort), and say
/// how it reaches the agent.
/// `hooks`: whether rlord's hooks work for its agent (asked outside the
/// lock: `Daemon::first_background_hooks`), for a session getting its
/// first background now, which then gets a mode: Hook, or the way an
/// agent without them gets it.
pub(crate) fn context_store(
    db: &Db,
    s: &Session,
    note: Option<String>,
    links: &[String],
    base: &std::path::Path,
    hooks: Option<bool>,
) -> Result<Value> {
    let s = s.clone();
    refuse_overlord(&s, "given background")?;
    let now = now_ms();
    if context::mode(db, &s.id)?.is_none() {
        let agent = Agent::named(&s.agent)?;
        context::record_mode(db, &s.id, context::delivery(agent, hooks.unwrap_or(false)))?;
    }
    if context::store(db, &s.id, note.as_slice(), links, now)? {
        // (Stored either way; the file is best effort.)
        if let Err(e) = context::refresh_file(db, base, &s) {
            tracing::warn!(session = %s.id, error = %e, "rewriting the background file");
        }
        db.insert_event(
            &s.id,
            now,
            "context_added",
            &json!({"note_chars": note.as_ref().map(|n| n.chars().count()),
                    "related": links.len()})
            .to_string(),
        )?;
    }
    // How it reaches the agent, said honestly (see `context::later_delivery`).
    let mut out = context::listing(db, &s.id)?;
    out["delivery"] = json!(context::later_delivery(context::mode(db, &s.id)?));
    Ok(out)
}

/// A session's background: the overlord's notes and related sessions.
pub fn session_context(db: &Db, params: &Value) -> Result<Value> {
    let s = db.resolve_ref(p_str(params, "ref")?)?;
    context::listing(db, &s.id)
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

/// Whether a new session runs on Codex's shared background server: Codex
/// with `[session] codex_daemon` on, but not the overlord (its wiring is
/// `-c` overrides, which Codex runs only in the pane), nor a session whose
/// own flags need the pane.
pub(crate) fn new_on_codex_daemon(
    agent: Agent,
    codex_daemon: bool,
    role: &str,
    extra_args: &[String],
) -> bool {
    agent == Agent::Codex
        && codex_daemon
        && !crate::overlord::is_overlord_role(role)
        && !codex::needs_embedded(extra_args)
}

/// `new_on_codex_daemon` for `session.new`'s params, read as `session_new`
/// reads them (asked before it runs, to start the server first).
pub(crate) fn new_params_on_codex_daemon(cfg: &Config, params: &Value) -> bool {
    let field = |k: &str| params[k].as_str().map(str::trim).filter(|v| !v.is_empty());
    let Ok(agent) = agent_param(cfg, params) else {
        return false;
    };
    new_on_codex_daemon(
        agent,
        cfg.session.codex_daemon,
        field("role").unwrap_or(""),
        &str_list(params, "extra_args"),
    )
}

/// The agent `session.new`'s params ask for: `agent`, else the
/// configured session agent. The one place that choice is made (the
/// launch, the Codex daemon check, the daemon's hooks check).
pub(crate) fn agent_param(cfg: &Config, params: &Value) -> Result<Agent> {
    let named = params["agent"]
        .as_str()
        .map(str::trim)
        .filter(|a| !a.is_empty());
    Agent::named(named.unwrap_or(&cfg.session.agent))
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

/// A list of session refs (strings only, trimmed, blanks dropped; missing:
/// none). The one parser for `related` / `add_related`, on every path.
pub(crate) fn ref_list(params: &Value, key: &str) -> Vec<String> {
    str_list(params, key)
        .into_iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect()
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

/// A new session's location (`new_location`): the repo's main root, if it
/// is in one; the folder it runs in; and its branch when the params name
/// it (`branch`, else from `name`; None: from a generated name).
pub(crate) type Location = (Option<String>, String, Option<String>);

/// Where a new session lives: an explicit repo, an explicit plain folder,
/// or the configured default repo, and its branch name (the one rule for
/// it). Shared with `repo_latest::set_launch_base`, which reads
/// `session.new`'s params before the DB lock.
pub(crate) fn new_location(cfg: &Config, params: &Value) -> Result<Location> {
    let p_opt = |k: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    // The one rule for a session's branch name.
    let branch = p_opt("branch").or_else(|| p_opt("name").map(|n| git::sanitize_branch(&n)));
    let (repo, project) = if let Some(r) = p_opt("repo") {
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
    Ok((repo, project, branch))
}

/// `session.new`'s cheap checks (the agent is installed, Remote Control
/// is possible, the background is within its limits), run before its base
/// is worked out (which may fetch), so a launch that will be refused
/// waits on nothing. `Daemon::call` runs them once and hands the result to
/// `session_new_with`; a direct `session_new` runs them itself.
pub(crate) fn precheck_new(db: &Db, cfg: &Config, params: &Value) -> Result<Prechecked> {
    let context = params["context"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    // (Too long: refused for the overlord to shorten.)
    let (note, links) = context::check(db, context, &ref_list(params, "related"), None)?;
    let agent = agent_param(cfg, params)?;
    if let Some(why) = install::missing(agent) {
        bail!("{why}");
    }
    if params["remote_control"].as_bool() == Some(true) && !agent.has_remote_control() {
        bail!(
            "Remote Control is a Claude Code feature; {} has none",
            agent.label()
        );
    }
    Ok(Prechecked { agent, note, links })
}

/// What `precheck_new` checked and `session_new` goes on with.
#[derive(Debug)]
pub(crate) struct Prechecked {
    pub agent: Agent,
    /// The overlord's note, checked.
    pub note: Option<String>,
    /// Related sessions, checked (their ids).
    pub links: Vec<String>,
}

/// A new session's worktree at `dir`: an existing `branch` checked out as
/// it is; a new one started at `base_resolved`'s commit (no upstream), or
/// at the main checkout's HEAD when it has none. Returns the base used
/// (null: none to speak of).
pub(crate) fn new_worktree(
    repo: &str,
    branch: &str,
    dir: &std::path::Path,
    base_resolved: &Value,
) -> Result<Value> {
    // (`git::worktree_add` asks once whether the branch exists. A base
    // is only worked out for a new branch, `launch_base`; one that
    // appeared since is refused with a start, which a retry gets past.)
    let start = base_resolved["commit"].as_str();
    git::worktree_add(repo, branch, dir, start)?;
    if start.is_some() {
        return Ok(base_resolved.clone());
    }
    if base_resolved["head"] != true {
        return Ok(Value::Null);
    }
    // Started at the main checkout's HEAD: which commit, for its note.
    let mut v = base_resolved.clone();
    let env = rlord_core::latest::GitEnv::default();
    v["commit"] = json!(rlord_core::latest::commit_of(&env, dir, "HEAD"));
    Ok(v)
}

pub fn session_new(db: &Db, cfg: &Config, params: &Value) -> Result<Value> {
    session_new_with(db, cfg, params, None)
}

/// `session_new` with `precheck_new` already run (`Daemon::call` runs it
/// before the base); None: run here.
pub(crate) fn session_new_with(
    db: &Db,
    cfg: &Config,
    params: &Value,
    prechecked: Option<Prechecked>,
) -> Result<Value> {
    let p_opt = |k: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    // The checks a launch makes before anything else (also run before
    // its base, `Daemon::call`): the agent is installed (else how to get
    // it, rather than fail in the pane), Remote Control is possible, the
    // overlord's notes and related sessions are within their limits.
    let Prechecked { agent, note, links } = match prechecked {
        Some(p) => p,
        None => precheck_new(db, cfg, params)?,
    };
    // Internal-only knobs (the overlord uses them; the MCP surface never
    // forwards them): adapter flags that must survive restarts, a role, and a
    // fixed tmux name.
    let extra_args = str_list(params, "extra_args");
    let role = p_opt("role").unwrap_or_default();
    let remote_control = params["remote_control"].as_bool().unwrap_or(false);
    // Each agent has its own models: the default is that agent's.
    let model = p_opt("model").or_else(|| {
        let m = cfg.session.model_for(agent).trim();
        (!m.is_empty()).then(|| m.to_string())
    });
    // The user's words; the overlord's notes and related sessions apart
    // from them (checked above).
    let prompt = p_opt("prompt");
    let notes: Vec<String> = note.into_iter().collect();
    // Launched by the overlord (`create_session`, or its proposal the user
    // launched): its background says so even with no notes.
    let background = context::needs_background(params);

    // (Worked out before the DB lock, `repo_latest::set_launch_base`; a
    // direct call works it out here.)
    let (repo, project, loc_branch) = match params["location_resolved"]["project"].as_str() {
        Some(project) => {
            let field = |k: &str| params["location_resolved"][k].as_str().map(str::to_string);
            (field("repo"), project.to_string(), field("branch"))
        }
        None => new_location(cfg, params)?,
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
    // A new branch starts where `Daemon::call` worked out before the DB
    // lock (`base_resolved`, from `repo_latest::launch_base`): the latest
    // default branch, a named `base`, or the main checkout's HEAD.
    let mut start_base = Value::Null;
    let (project_path, workspace_path, branch) = match (&repo, want_ws) {
        (Some(repo), true) => {
            let branch = loc_branch.unwrap_or_else(|| git::sanitize_branch(&name));
            let dir = git::workspace_path(repo, &cfg.session.workspace_dir, &branch);
            git::ensure_excluded(repo, &cfg.session.workspace_dir)?;
            start_base = new_worktree(repo, &branch, &dir, &params["base_resolved"])?;
            let dir = dir.to_string_lossy().into_owned();
            (dir.clone(), Some(dir), Some(branch))
        }
        _ => (project, None, None),
    };
    // Its background's rows: where its branch started (`source =
    // 'base'`, plain code) first, then the overlord's notes and links.
    let rows: Vec<context::NewRow> = crate::repo_latest::base_note(&start_base, now_ms())
        .map(|body| context::NewRow {
            kind: "note",
            body,
            source: "base",
        })
        .into_iter()
        .chain(context::rows_from(&notes, &links, "overlord"))
        .collect();

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
    if new_on_codex_daemon(agent, cfg.session.codex_daemon, &role, &extra_args) {
        spec["codex_host"] = json!(codex::HOST_DAEMON);
    }
    let daemon = codex::on_daemon(&spec);
    // How its background reaches the agent: its hook, Claude's system
    // prompt file, or a fixed line after the user's words naming the file.
    let base = paths::data_dir();
    let bg_file = context::file_path(&base, &id);
    // (`hooks_work`: asked by the daemon before taking the DB lock.)
    let delivery = background.then(|| context::delivery(agent, params["hooks_work"] == true));
    // Its file first (the agent may read it at once). Best effort: a
    // file that can't be written never stops the launch; the launch then
    // carries no flag or pointer naming it.
    let file_ok = delivery.is_some() && {
        let text = context::launch_file_text_rows(db, &name, &[], &rows, now_ms());
        match context::write_file(&base, &id, &text) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "writing the background file");
                false
            }
        }
    };
    let carried = delivery.filter(|d| file_ok || *d == context::Delivery::Hook);
    let mut args = spec_args(agent, &spec);
    args.extend(context::launch_args(carried, &bg_file));
    // With no words of the user's, the pointer line alone is the prompt.
    let first = context::launch_prompt(prompt.as_deref(), carried, &bg_file);
    let launch = agent.launch(&project_path, first.as_deref(), model.as_deref(), &args);
    let tmux_name = match p_opt("tmux_name") {
        Some(n) => n,
        None => tmux_name_for(db, &name, None),
    };
    let launch_spec = spec.to_string();
    start(
        db,
        Start {
            id: &id,
            name: &name,
            agent,
            launch_spec: &launch_spec,
            launch,
            first: first.as_deref(),
            project_path: &project_path,
            repo: repo.as_deref(),
            workspace_path: workspace_path.as_deref(),
            branch: branch.as_deref(),
            role: &role,
            tmux_name: &tmux_name,
            daemon,
            forked_from: None,
            kv: Vec::new(),
            background: delivery.map(|mode| Background {
                mode,
                carried,
                copy_from: None,
                rows: rows.clone(),
            }),
            event: (
                "launched",
                json!({"workspace": workspace_path, "branch": branch, "base": start_base}),
            ),
        },
    )?;
    tracing::info!(session = %id, name = %name, agent = agent.name(), workspace = ?workspace_path, "launched session");

    let s = db.resolve_ref(&id)?;
    let mut v = serde_json::to_value(s)?;
    if !start_base.is_null() {
        v["base"] = start_base;
    }
    Ok(v)
}

/// A new session's background, as `start` stores it: how it reaches the
/// agent (`mode`), how the launch carries it (`carried`: None when it
/// doesn't, its file couldn't be written), the session whose rows are
/// copied first (unseen) with the row ids left out (a fork's parent),
/// and the rows to add after them.
pub(crate) struct Background<'a> {
    pub mode: context::Delivery,
    pub carried: Option<context::Delivery>,
    pub copy_from: Option<(&'a str, &'a [i64])>,
    pub rows: Vec<context::NewRow>,
}

/// What a new session and a fork have decided before the pane starts.
pub(crate) struct Start<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub agent: Agent,
    /// The launch spec as stored.
    pub launch_spec: &'a str,
    /// The agent's command, and what is known of its conversation.
    pub launch: rlord_core::agents::Launch,
    /// The first prompt as typed (an agent that picks its own id is bound
    /// by it).
    pub first: Option<&'a str>,
    pub project_path: &'a str,
    pub repo: Option<&'a str>,
    pub workspace_path: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub role: &'a str,
    pub tmux_name: &'a str,
    /// On Codex's shared background server.
    pub daemon: bool,
    /// A fork's parent (its rlord id).
    pub forked_from: Option<&'a str>,
    /// kv keys written with its row, before its pane starts (a fork's
    /// `bind_fork`, `fork_pending`): a running session never lacks them.
    pub kv: Vec<(String, String)>,
    pub background: Option<Background<'a>>,
    /// The event logged once the pane is up (`launched`, `forked`): its
    /// detail, to which the argv is added.
    pub event: (&'static str, Value),
}

/// What a new session and a fork share once everything is decided: the
/// row, its background, its pane, the log.
pub(crate) fn start(db: &Db, s: Start) -> Result<()> {
    let Start { id, agent, .. } = s;
    let mut launch = s.launch;
    let program = pane_program(agent, &mut launch.argv);
    let argv = &launch.argv;
    let now = now_ms();
    db.insert_session(&NewSession {
        id,
        name: s.name,
        agent: agent.name(),
        agent_session: launch.session_id.as_deref(),
        project_path: s.project_path,
        tmux_session: Some(s.tmux_name),
        launch_spec: s.launch_spec,
        created_at: now,
        repo_path: s.repo,
        workspace_path: s.workspace_path,
        branch: s.branch,
        role: s.role,
    })?;
    if let Some(parent) = s.forked_from {
        db.set_forked_from(id, parent)?;
    }
    for (k, v) in &s.kv {
        db.kv_set(k, v)?;
    }
    if let Some(r) = s.repo {
        db.touch_repo(r, now)?;
    }
    if let Some(t) = &launch.transcript {
        db.upsert_transcript_source(id, &t.to_string_lossy())?;
    }
    hold_deferred(db, id, &launch)?;
    note_first_prompt(db, id, agent, s.first, now)?;
    if let Some(bg) = &s.background {
        context::turn_on(db, id)?;
        if let Some((from, except)) = bg.copy_from {
            db.context_copy(from, id, except)?;
        }
        context::store_rows(db, id, &bg.rows, now)?;
        if bg.carried == Some(bg.mode) {
            context::set_mode(db, id, bg.mode, now)?;
        } else {
            // (It didn't go out with the launch: its rows stay unseen.)
            context::record_mode(db, id, bg.mode)?;
        }
    }

    let cmd = pane_cmd(id, program.as_deref(), s.daemon, argv);
    note_launch(db, id, argv)?;
    // (Its folder is in the gate before its first hook can run.)
    crate::codex_daemon::sync_gate(db);
    tmux::new_session(s.tmux_name, s.project_path, &cmd)?;
    // The pane is up: from here nothing fails the start (a caller undoes a
    // start that failed, and must never undo a running one).
    let (kind, mut detail) = s.event;
    detail["argv"] = json!(argv);
    if let Err(e) = db.insert_event(id, now, kind, &detail.to_string()) {
        tracing::warn!(session = %id, error = %e, "logging a start");
    }
    Ok(())
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
pub(crate) fn tmux_name_for(db: &Db, name: &str, own: Option<&str>) -> String {
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
    let mut s = [db.resolve_ref(p_str(params, "ref")?)?];
    // (Its forks: shown here, so read here, not on every lookup.)
    db.attach_forks(&mut s)?;
    let [s] = s;
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
    // A fork whose conversation was never made (its agent never got that
    // far) forks again rather than resume, from its parent's conversation
    // as it is now: its note says so in its file before the pane starts;
    // the database takes the note (and the fork's other changes) only
    // once the pane is up, so a restart that fails changes nothing.
    let refork = crate::fork::pending(db, &s)?;
    let renewed_at = now_ms();
    let renewed = match &refork {
        Some(_) => crate::fork::renewed_note(db, &s, renewed_at)?,
        None => None,
    };
    let renewed_note = renewed.as_deref().map(|body| context::Renewed {
        source: "fork",
        body,
        at: renewed_at,
    });
    // Its background as it was launched with, its file rewritten first
    // (related sessions' status, a rename: as they are now).
    // Best effort: a file that can't be written never stops the restart;
    // without it the start carries no flag or pointer naming it.
    let base = paths::data_dir();
    let bg_file = context::file_path(&base, &s.id);
    let rewritten = match context::refresh_file_with(db, &base, &s, renewed_note.as_ref()) {
        Ok(f) => f.is_some(),
        Err(e) => {
            tracing::warn!(session = %s.id, error = %e, "rewriting the background file");
            false
        }
    };
    // The command with the session's own flags; the background's flag or
    // pointer is added to it below (both go last, where a launch puts
    // them).
    let (mut argv, resumed) = match &refork {
        Some(parent) => (
            crate::fork::refork_argv(
                agent,
                &s,
                parent,
                model.as_deref(),
                &spec_args(agent, &spec),
            )?,
            false,
        ),
        None => agent.resume(
            s.agent_session.as_deref(),
            &s.agent_session_trust,
            &s.project_path,
            model.as_deref(),
            &spec_args(agent, &spec),
        )?,
    };
    let mut mode = context::mode(db, &s.id)?;
    // A new conversation by hook, but the hooks no longer work
    // (`hooks_work`: see `Daemon::restart_hooks`, asked before the lock):
    // the way an agent without them gets it, from now on.
    let hooks_gone = params.get("hooks_work").and_then(Value::as_bool) == Some(false);
    // (Stored only once the pane is up, below: a failed restart leaves the
    // mode as it was.)
    let mut fell_back = None;
    if mode == Some(context::Delivery::Hook) && hooks_gone && !resumed {
        let fallback = context::delivery(agent, false);
        fell_back = Some(fallback);
        mode = Some(fallback);
    }
    let carried = mode.filter(|d| *d == context::Delivery::Hook || bg_file.is_file());
    argv.extend(context::launch_args(carried, &bg_file));
    // A new conversation's first prompt: a Pointer-mode session starting
    // one gets the pointer again; a re-forked fork whose conversation was
    // never written, its own first words too, as at the fork
    // (`fork::restart_prompt`). Its conversation is then bound by that
    // prompt, from now.
    let first = crate::fork::restart_prompt(&spec, refork.is_some(), carried, resumed, &bg_file);
    // (What it hands over: the pointer.)
    let pointer_sent = first.is_some() && carried == Some(context::Delivery::Pointer);
    if let Some(p) = &first {
        agent.add_first_prompt(&mut argv, p);
    }
    let program = pane_program(agent, &mut argv);
    // What an earlier start left to confirm is over (its clock too). A
    // session that starts anew under its id (the agent never had it) is
    // created with the launch's model: that is confirmed as at launch.
    let deferred = agent.deferred(model.as_deref()).filter(|_| !resumed);
    hold(db, &s.id, deferred.as_ref())?;
    let tmux_name = tmux_name_of(db, &s)?;
    let cmd = pane_cmd(&s.id, program.as_deref(), s.codex_daemon, &argv);
    note_launch(db, &s.id, &argv)?;
    // A re-fork binds its new thread as at the fork: the key that says so
    // is written before the pane starts, so it can't be lost after.
    if let Some(parent) = &refork {
        crate::fork::refork_bind_key(db, &s, parent)?;
    }
    if tmux::has_session(&tmux_name) {
        tmux::respawn_pane(&tmux_name, &s.project_path, &cmd)?;
    } else {
        tmux::new_session(&tmux_name, &s.project_path, &cmd)?;
    }
    // The pane is up: from here nothing fails the restart (as at `start`).
    // Each step is logged if it fails, and the status, the seen marks and
    // the event are always written.
    let session = s.id.clone();
    let logged = |what: &str, r: Result<()>| {
        if let Err(e) = r {
            tracing::warn!(session = %session, error = %e, "restart: {what}");
        }
    };
    let now = now_ms();
    if let Some(fallback) = fell_back {
        logged(
            "recording the mode",
            context::record_mode(db, &s.id, fallback),
        );
        tracing::info!(session = %s.id, mode = fallback.as_str(), "hooks gone: background another way");
    }
    if let Some(p) = &first {
        logged(
            "noting the first prompt",
            note_first_prompt(db, &s.id, agent, Some(p), now),
        );
    } else if refork.is_some() {
        // (Its thread, forked from the parent's, is one started from now.)
        logged(
            "noting the first prompt",
            note_first_prompt(db, &s.id, agent, None, now),
        );
    }
    // The re-fork's note (as its file already showed it) and its new
    // thread are stored now, before what it handed over is marked seen.
    if let Some(body) = &renewed {
        logged(
            "renewing the fork's note",
            crate::fork::renote(db, &s, body, renewed_at),
        );
    }
    if refork.is_some() {
        logged("unbinding the old thread", crate::fork::reforked(db, &s));
    }
    // Only what this start carried fresh counts as handed over: the
    // pointer sent again, or the system prompt file rewritten just now.
    // (A resume without a pointer, or a stale file, carries nothing new.)
    let fresh = pointer_sent || (carried == Some(context::Delivery::SystemPromptFile) && rewritten);
    if fresh {
        logged(
            "marking its background seen",
            db.context_mark_all_seen(&s.id, now),
        );
    }
    logged(
        "setting its status",
        db.set_status(&s.id, Status::Starting, now).map(|_| ()),
    );
    // Listed in the gate again (no longer stopped). (A resumed Codex runs
    // no hook before its next prompt.)
    crate::codex_daemon::sync_gate(db);
    logged(
        "logging the event",
        db.insert_event(
            &s.id,
            now,
            match (resumed, &refork) {
                (true, _) => "resumed",
                // (Forked again: the `forked` event's branch point, now.)
                (false, Some(_)) => "reforked",
                (false, None) => "restarted",
            },
            &match (resumed, agent.deferred(model.as_deref())) {
                // (Resumed: whatever model the conversation has by now.)
                (true, Some(d)) => json!({"argv": argv,
                    "model": format!("the session keeps its own (launched with {})", d.model)}),
                (_, _) if refork.is_some() => {
                    json!({"argv": argv, "from": s.forked_from, "from_conversation": refork})
                }
                _ => json!({"argv": argv}),
            }
            .to_string(),
        )
        .map(|_| ()),
    );
    tracing::info!(session = %s.id, resumed, "restarted session");
    Ok(serde_json::to_value(db.resolve_ref(&s.id)?)?)
}

/// A start with first prompt `prompt` at `at`: for an agent that picks its
/// own conversation id, the conversation is bound once the agent reports
/// it (or it turns up in the agent's own index): one started from then
/// on, with that first prompt as typed (the pointer line included).
fn note_first_prompt(db: &Db, id: &str, agent: Agent, prompt: Option<&str>, at: i64) -> Result<()> {
    if !agent.mints_id() {
        db.kv_set(&bind_since_key(id), &at.to_string())?;
        db.kv_set(&bind_prompt_key(id), prompt.unwrap_or(""))?;
    }
    Ok(())
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
    // Only overlords start fresh, and they never run on Codex's daemon. A
    // daemon-mode session would need its turn interrupted, the server
    // started and the gate moved to the new thread first: refuse it.
    if s.codex_daemon {
        bail!(
            "{} runs on Codex's background server: a fresh conversation isn't supported there",
            s.name
        );
    }
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
            // (A fresh conversation is no fork.)
            db.kv_delete(&bind_fork_key(&s.id))?;
        }
    }
    // Nor is there a fork to make again at its next restart.
    db.kv_delete(&crate::fork::pending_key(&s.id))?;
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

/// Ingest one live agent hook event (from `rlord hook emit`).
pub fn hook_emit(db: &Db, params: &Value) -> Result<Value> {
    hook_emit_with(db, params, &|agent, id| agent.indexed(id))
}

/// `hook.ack`: the hook client printed the answer `ack` names.
pub fn hook_ack(db: &Db, params: &Value) -> Result<Value> {
    let acked = context::ack(db, p_str(params, "ack")?, now_ms())?;
    Ok(json!({"acked": acked}))
}

/// Ingest a hook event spooled while the daemon was down: as a live one,
/// but its client is long gone, so it gets no answer (nothing is handed
/// over, nothing marked).
pub fn hook_replay(db: &Db, params: &Value) -> Result<Value> {
    hook_emit_in(db, params, &|agent, id| agent.indexed(id), false)
}

/// `hook_emit`, with how to ask an agent's own index whether a
/// conversation is one the user drives (tests answer it themselves).
pub(crate) fn hook_emit_with(
    db: &Db,
    params: &Value,
    indexed: &dyn Fn(Agent, &str) -> Indexed,
) -> Result<Value> {
    hook_emit_in(db, params, indexed, true)
}

/// `live`: its client waits for the answer (false: replayed).
fn hook_emit_in(
    db: &Db,
    params: &Value,
    indexed: &dyn Fn(Agent, &str) -> Indexed,
    live: bool,
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
            clear_bind_keys(db, &s.id)?;
        }
        // The session's background, for its own conversation only (not a
        // sub-agent's thread; a forked or background copy was refused
        // above): the hook prints it for the agent.
        // The session's background for its agent, built at the very end
        // (`answer`, at each return below), after everything that can
        // fail: its own bookkeeping never runs for an event that didn't go
        // through. Best effort: a failure is logged, no answer. Marked
        // handed over only when the client acknowledges it (`hook.ack`).
        let answer = |v: Value| -> Value {
            if whose == Whose::Other
                || !live
                || !matches!(event, "SessionStart" | "UserPromptSubmit")
            {
                return v;
            }
            let now_s = db.resolve_ref(&s.id).unwrap_or_else(|_| s.clone());
            let file = context::file_path(&paths::data_dir(), &s.id);
            let source = payload["source"].as_str();
            match context::hook_reply_from(db, &now_s, agent, event, source, sid, &file) {
                Ok(Some(r)) => {
                    let mut v = v;
                    v["reply"] = r.output;
                    v["reply_ack"] = json!(r.ack);
                    v
                }
                Ok(None) => v,
                Err(e) => {
                    tracing::warn!(session = %s.id, event, error = %e, "session background for a hook");
                    v
                }
            }
        };
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
                // A fork whose own conversation is written: no longer one
                // to make again at a restart.
                crate::fork::conversation_seen(db, &s.id, tp)?;
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
            return Ok(answer(json!({"ok": true, "unchanged": true})));
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
            let mut detail = crate::own::with_source(json!({"hook": event}), source);
            // How the agent started (`startup`, `resume`, `clear`,
            // `compact`): a new process (startup, resume) ended the old
            // one's background work (`background.rs`).
            if event == "SessionStart" {
                if let Some(how) = payload["source"].as_str() {
                    detail["start"] = json!(how);
                }
            }
            db.insert_event(&s.id, now, kind, &detail.to_string())?;
        }
        return Ok(answer(json!({"ok": true})));
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

/// kv key: for a user fork whose agent assigns its own id (Codex), the
/// thread it was forked from: its conversation is the thread whose rollout
/// says so (`discover::pick_thread`), not one picked by first message (a
/// fork's is its parent's).
pub(crate) fn bind_fork_key(id: &str) -> String {
    format!("bind_fork:{id}")
}

/// What an agent that picks its own id needed to find a conversation, once
/// one is bound.
pub(crate) fn clear_bind_keys(db: &Db, id: &str) -> Result<()> {
    db.kv_delete(&bind_since_key(id))?;
    db.kv_delete(&bind_prompt_key(id))?;
    db.kv_delete(&bind_fork_key(id))
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
    fn only_plain_codex_sessions_go_on_the_daemon() {
        let none: Vec<String> = Vec::new();
        assert!(new_on_codex_daemon(Agent::Codex, true, "", &none));
        assert!(!new_on_codex_daemon(Agent::Codex, false, "", &none));
        assert!(!new_on_codex_daemon(Agent::Claude, true, "", &none));
        assert!(!new_on_codex_daemon(
            Agent::Codex,
            true,
            crate::overlord::ROLE,
            &none
        ));
        let pane_only = vec!["--search".to_string()];
        assert!(!new_on_codex_daemon(Agent::Codex, true, "", &pane_only));
        // From `session.new`'s params, as `session_new` reads them.
        let mut cfg = Config::default();
        cfg.session.codex_daemon = true;
        cfg.session.agent = "codex".into();
        assert!(new_params_on_codex_daemon(&cfg, &json!({})));
        assert!(!new_params_on_codex_daemon(
            &cfg,
            &json!({"agent": "claude"})
        ));
        assert!(!new_params_on_codex_daemon(
            &cfg,
            &json!({"role": crate::overlord::ROLE})
        ));
        assert!(!new_params_on_codex_daemon(
            &cfg,
            &json!({"extra_args": ["--search"]})
        ));
    }

    #[test]
    fn fresh_refuses_a_daemon_mode_session() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id: "d1",
            name: "d1",
            agent: "codex",
            agent_session: Some("t-1"),
            project_path: "/src/app",
            tmux_session: Some("rlord-test-none-d1"),
            launch_spec: &json!({"agent": "codex", "codex_host": "daemon"}).to_string(),
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        let e = session_fresh(&db, &json!({"ref": "d1"})).unwrap_err();
        assert!(e.to_string().contains("background server"), "{e}");
    }

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
    fn background_added_later_is_stored_and_written() {
        let db = Db::open_in_memory().unwrap();
        for (id, name) in [("01a", "web"), ("01b", "auth")] {
            db.insert_session(&NewSession {
                id,
                name,
                agent: "claude",
                agent_session: None,
                project_path: "/tmp/w",
                tmux_session: None,
                launch_spec: "{}",
                created_at: 1,
                repo_path: None,
                workspace_path: None,
                branch: None,
                role: "",
            })
            .unwrap();
        }
        let t = tempfile::tempdir().unwrap();
        let r = session_context_add_in(
            &db,
            &json!({"ref": "web", "context": "The user wants dark mode too.", "related": ["auth"],
                    "hooks_work": false}),
            t.path(),
        )
        .unwrap();
        // Its first background: a mode is recorded from whether hooks
        // work (not asked here: no): Claude's system prompt file.
        assert_eq!(r["delivery"], "next_start");
        assert_eq!(
            context::mode(&db, "01a").unwrap(),
            Some(context::Delivery::SystemPromptFile)
        );
        assert_eq!(r["notes"][0]["note"], "The user wants dark mode too.");
        assert_eq!(r["related"][0]["name"], "auth");
        let text = std::fs::read_to_string(context::file_path(t.path(), "01a")).unwrap();
        assert!(text.contains("dark mode") && text.contains("- auth (/tmp/w"));
        // Too long: refused, nothing stored.
        let long = "x".repeat(context::NOTE_MAX + 1);
        assert!(
            session_context_add_in(&db, &json!({"ref": "web", "context": long}), t.path()).is_err()
        );
        assert_eq!(db.context_rows("01a").unwrap().len(), 2);
        let mut listed = r.clone();
        listed.as_object_mut().unwrap().remove("delivery");
        assert_eq!(
            session_context(&db, &json!({"ref": "web"})).unwrap(),
            listed
        );
        // By the stored mode: what the running agent loaded at its start.
        let add = |text: &str| {
            session_context_add_in(&db, &json!({"ref": "auth", "context": text}), t.path()).unwrap()
                ["delivery"]
                .clone()
        };
        // First background with hooks working: Hook, the next prompt.
        let first = session_context_add_in(
            &db,
            &json!({"ref": "auth", "context": "a", "hooks_work": true}),
            t.path(),
        )
        .unwrap();
        assert_eq!(first["delivery"], "next_prompt");
        context::set_mode(&db, "01b", context::Delivery::Hook, 1).unwrap();
        assert_eq!(add("b"), "next_prompt");
        context::set_mode(&db, "01b", context::Delivery::SystemPromptFile, 1).unwrap();
        assert_eq!(add("c"), "next_start");
        context::set_mode(&db, "01b", context::Delivery::Pointer, 1).unwrap();
        assert_eq!(add("d"), "file_only");
        // The file can't be written (its folder is a file): the note is
        // stored all the same.
        let blocked = t.path().join("not-a-folder");
        std::fs::write(&blocked, b"").unwrap();
        let r = session_context_add_in(
            &db,
            &json!({"ref": "auth", "context": "stored anyway"}),
            &blocked,
        )
        .unwrap();
        assert!(r["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["note"] == "stored anyway"));
    }

    #[test]
    fn a_first_prompt_resets_what_binding_goes_by() {
        let db = Db::open_in_memory().unwrap();
        // Codex picks its own ids: bound by the first prompt, from then.
        note_first_prompt(&db, "s", Agent::Codex, Some("[rlord] Background …"), 42).unwrap();
        assert_eq!(
            db.kv_get(&bind_since_key("s")).unwrap().as_deref(),
            Some("42")
        );
        assert_eq!(
            db.kv_get(&bind_prompt_key("s")).unwrap().as_deref(),
            Some("[rlord] Background …")
        );
        // Claude and OpenCode: rlord chose the id, nothing to note.
        note_first_prompt(&db, "o", Agent::OpenCode, Some("x"), 1).unwrap();
        assert_eq!(db.kv_get(&bind_since_key("o")).unwrap(), None);
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
            (
                "context",
                session_context_add_in(
                    &db,
                    &with("context", json!("x")),
                    std::path::Path::new("/nonexistent"),
                ),
            ),
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
