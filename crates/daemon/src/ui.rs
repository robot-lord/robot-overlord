//! UI control: the daemon's handle on what the user sees. Two halves:
//!
//! - **Terminal navigation** is done here directly with tmux: switching the
//!   user's terminal between the TUI ("home") and agent sessions.
//! - **TUI state** (list view, dialogs, the overlord overlay) is owned by the
//!   TUI; the daemon pushes it typed `ui` commands over the subscription
//!   stream. The TUI reports its view back so the overlord sees what the user
//!   sees.
//!
//! "Home" is the tmux session running the TUI. If there isn't one, it is
//! created on demand, so the overlord can always land the user on the list.

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use rlord_core::config::expand_tilde;
use rlord_core::db::{ArchivedMode, SessionQuery, SortMode};
use rlord_core::tmux;
use rlord_core::types::Status;

use crate::{methods, overlord, Daemon};

const DEFAULT_VIEW_KEY: &str = "default_view";
/// tmux session created to host the TUI when none is running.
const HOME_SESSION: &str = "rlord";

/// The view shape shared with the TUI.
fn normalize_view(v: &Value) -> Value {
    json!({
        "query": v.get("query").and_then(Value::as_str).unwrap_or(""),
        "sort": v.get("sort").and_then(Value::as_str).unwrap_or("updated"),
        "reverse": v.get("reverse").and_then(Value::as_bool).unwrap_or(false),
        "archived": v.get("archived").and_then(Value::as_str).unwrap_or("live"),
        // "sessions" (the flat list) or "groups" (sessions by tag).
        "tab": match v.get("tab").and_then(Value::as_str) {
            Some("groups") => "groups",
            _ => "sessions",
        },
        // The selected group on the groups tab: a tag, "" = untagged.
        "group": v.get("group").cloned().filter(Value::is_string).unwrap_or(Value::Null),
    })
}

pub fn default_view(d: &Daemon) -> Result<Value> {
    let db = d.db();
    Ok(match db.kv_get(DEFAULT_VIEW_KEY)? {
        Some(s) => normalize_view(&serde_json::from_str(&s).unwrap_or(json!({}))),
        None => normalize_view(&json!({})),
    })
}

/// What the user is looking at right now.
pub fn state(d: &Daemon) -> Result<Value> {
    let clients = tmux::list_clients();
    let home = tmux::home().filter(|h| tmux::has_session(h));
    let user = tmux::user_client(&clients, home.as_deref()).cloned();
    let terminal = match &user {
        None => json!({"kind": "none", "note": "no terminal attached to tmux"}),
        Some(c) if Some(&c.session) == home.as_ref() => json!({"kind": "session_list"}),
        Some(c) if c.session == overlord::TMUX_NAME => json!({"kind": "overlord"}),
        Some(c) => {
            let db = d.db();
            let all = db.sessions(&SessionQuery {
                archived: ArchivedMode::All,
                ..Default::default()
            })?;
            match all
                .iter()
                .find(|s| s.tmux_session.as_deref() == Some(c.session.as_str()))
            {
                Some(s) => json!({"kind": "session", "id": s.id, "name": s.name}),
                None => json!({"kind": "other_tmux_session", "tmux_session": c.session}),
            }
        }
    };
    Ok(json!({
        "terminal": terminal,
        "tui_running": d.tui_count() > 0,
        "overlord_overlay_open": d.overlay.lock().expect("ui mutex").is_some(),
        "current_view": normalize_view(&d.ui_view.lock().expect("ui mutex")),
        "default_view": default_view(d)?,
    }))
}

/// Switch the user's terminal to `tmux_session`. Returns whether the user
/// was already there.
fn switch_user_to(tmux_session: &str) -> Result<bool> {
    let clients = tmux::list_clients();
    let home = tmux::home();
    let Some(c) = tmux::user_client(&clients, home.as_deref()) else {
        bail!("no terminal is attached to tmux; the user needs to run `rlord` in a terminal");
    };
    if c.session == tmux_session {
        return Ok(true);
    }
    tmux::switch_client_of(&c.name, tmux_session)?;
    Ok(false)
}

/// Close the overlord overlay wherever it is open.
fn hide_overlord(d: &Daemon) {
    for c in tmux::list_clients() {
        let _ = tmux::close_popup(&c.name);
    }
    *d.overlay.lock().expect("ui mutex") = None;
}

/// Close the overlay if it is open on `client`; returns whether it was.
/// (Option-D while the overlay is up closes it rather than navigating.)
pub fn close_overlay_on(d: &Daemon, params: &Value) -> Result<Value> {
    let client = params.get("client").and_then(Value::as_str);
    let open_here = client.is_some_and(|c| {
        d.overlay
            .lock()
            .expect("ui mutex")
            .as_ref()
            .is_some_and(|(o, _)| o == c)
    });
    if open_here {
        hide_overlord(d);
    }
    Ok(json!({"closed": open_here}))
}

/// Toggle the overlord overlay over the user's terminal (`show: true` /
/// `false` force it). `client` is the terminal (the key handler passes its
/// own; otherwise it is guessed); `from` the session it is showing. In the
/// overlord's own session the overlay is off — you're already there. No
/// overlord yet → the TUI's setup dialog instead.
pub fn overlord(d: &Daemon, params: &Value) -> Result<Value> {
    let show = params.get("show").and_then(Value::as_bool);
    if show == Some(false) {
        hide_overlord(d);
        return Ok(json!({"overlord": "hidden"}));
    }
    if params.get("from").and_then(Value::as_str) == Some(overlord::TMUX_NAME) {
        return Ok(json!({"overlord": "already here"}));
    }
    if d.db().overlord()?.is_none() {
        let home = ensure_home()?;
        // Queue first, so a TUI only now starting picks it up as pending.
        d.ui_cmd(json!({"cmd": "overlord_setup"}));
        switch_user_to(&home)?;
        return Ok(json!({"setup": true}));
    }
    overlord::ensure(&d.db(), &d.config, &json!({}))?;
    d.changed();
    let client = match params.get("client").and_then(Value::as_str) {
        Some(c) => c.to_string(),
        None => {
            let clients = tmux::list_clients();
            let home = tmux::home();
            tmux::user_client(&clients, home.as_deref())
                .map(|c| c.name.clone())
                .context("no terminal is attached to tmux; run `rlord` in a terminal")?
        }
    };
    // tmux may run this key's binding even while the popup is open (rather
    // than handing the key to the popup): the same key on the same terminal
    // closes it.
    let open_here = d
        .overlay
        .lock()
        .expect("ui mutex")
        .as_ref()
        .is_some_and(|(c, _)| *c == client);
    if open_here && show != Some(true) {
        hide_overlord(d);
        return Ok(json!({"overlord": "hidden"}));
    }
    let token = d
        .overlay_seq
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let cmd = format!(
        "exec {} overlay --token {token}",
        tmux::shell_quote(&methods::rlord_bin())
    );
    tmux::open_popup(
        &client,
        &d.config.overlord.width,
        &d.config.overlord.height,
        " ◆ overlord · ⌥O / ⌥D close ",
        &format!("fg={}", crate::overlord::color(&d.config)),
        &cmd,
    )?;
    *d.overlay.lock().expect("ui mutex") = Some((client, token));
    Ok(json!({"overlord": "shown"}))
}

/// Post-restart cleanup for `rlord reset`: close overlays, drop parked UI
/// commands, resync session state with tmux, re-apply rlord's tmux config and
/// key bindings, and restart the list (if it's open) so it starts fresh.
pub fn reset(d: &Daemon) -> Result<Value> {
    hide_overlord(d);
    *d.pending_ui.lock().expect("ui mutex") = None;
    methods::reconcile(&d.db())?;
    let _ = tmux::source_conf();
    d.bind_nav_keys();
    crate::profile::sync_all(d, true);
    if let Some(home) = tmux::home().filter(|h| tmux::has_session(h)) {
        let cmd = format!("exec {} tui", tmux::shell_quote(&methods::rlord_bin()));
        let _ = tmux::respawn_plain(&home, &cmd);
    }
    // (Bind first: a guard held in an `if let` scrutinee would still be
    // locked inside the block, and std's Mutex isn't reentrant.)
    let overlord_running = d
        .db()
        .overlord()?
        .is_some_and(|o| o.tmux_session.as_deref().is_some_and(tmux::has_session));
    if overlord_running {
        let _ = overlord::ensure(&d.db(), &d.config, &json!({})); // restyle
    }
    d.changed();
    let all = d.db().sessions(&SessionQuery {
        include_overlord: true,
        ..Default::default()
    })?;
    let running = all.iter().filter(|s| s.status != Status::Stopped).count();
    Ok(json!({"running": running, "stopped": all.len() - running}))
}

/// The TUI's tmux session, creating one running `rlord tui` if needed.
fn ensure_home() -> Result<String> {
    if let Some(h) = tmux::home().filter(|h| tmux::has_session(h)) {
        return Ok(h);
    }
    if !tmux::has_session(HOME_SESSION) {
        let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let cmd = format!("exec {} tui", tmux::shell_quote(&methods::rlord_bin()));
        tmux::new_plain_session(HOME_SESSION, &home_dir, &cmd)?;
    }
    tmux::set_home(HOME_SESSION)?;
    Ok(HOME_SESSION.to_string())
}

pub fn show_list(d: &Daemon) -> Result<Value> {
    let home = ensure_home()?;
    hide_overlord(d);
    switch_user_to(&home)?;
    Ok(json!({"shown": "session_list"}))
}

pub fn goto(d: &Daemon, params: &Value) -> Result<Value> {
    let r = methods::p_str(params, "ref")?;
    let s = {
        let db = d.db();
        let s = db.resolve_ref(r)?;
        if s.role == overlord::ROLE {
            overlord::ensure(&db, &d.config, &json!({}))?;
        } else if s.status == Status::Stopped
            || !s.tmux_session.as_deref().is_some_and(tmux::has_session)
        {
            methods::session_restart(&db, &json!({"ref": s.id}))?;
        }
        db.resolve_ref(&s.id)?
    };
    d.changed();
    if s.role == overlord::ROLE {
        // Full screen: hand the window's size back from any overlay.
        let _ = tmux::auto_size_window(overlord::TMUX_NAME);
    }
    let t = s
        .tmux_session
        .as_deref()
        .context("session has no tmux session")?;
    hide_overlord(d);
    switch_user_to(t)?;
    Ok(json!({"switched_to": {"id": s.id, "name": s.name}}))
}

/// Set (and optionally save as default) the TUI list view; show it unless
/// `show: false`.
pub fn set_view(d: &Daemon, params: &Value) -> Result<Value> {
    let mut view = d.ui_view.lock().expect("ui mutex").clone();
    if view.is_null() {
        view = default_view(d)?;
    }
    let obj = view.as_object_mut().context("bad view state")?;
    if let Some(q) = params.get("query").and_then(Value::as_str) {
        obj.insert("query".into(), json!(q.trim()));
    }
    if let Some(sort) = params.get("sort").and_then(Value::as_str) {
        let m = SortMode::parse(sort)
            .with_context(|| format!("unknown sort '{sort}' (updated|user|state|created|name)"))?;
        obj.insert("sort".into(), json!(m.label()));
    }
    if let Some(r) = params.get("reverse").and_then(Value::as_bool) {
        obj.insert("reverse".into(), json!(r));
    }
    if let Some(a) = params.get("archived").and_then(Value::as_str) {
        if !matches!(a, "live" | "only") {
            bail!("archived must be 'live' or 'only'");
        }
        obj.insert("archived".into(), json!(a));
    }
    if let Some(t) = params.get("tab").and_then(Value::as_str) {
        if !matches!(t, "sessions" | "groups") {
            bail!("tab must be 'sessions' or 'groups'");
        }
        obj.insert("tab".into(), json!(t));
    }
    if let Some(g) = params.get("group").and_then(Value::as_str) {
        obj.insert("group".into(), json!(g.trim()));
        obj.insert("tab".into(), json!("groups"));
    }
    let view = normalize_view(&view);
    if params.get("save_as_default").and_then(Value::as_bool) == Some(true) {
        d.db().kv_set(DEFAULT_VIEW_KEY, &view.to_string())?;
    }
    let mut cmd = Map::new();
    cmd.insert("cmd".into(), json!("set_view"));
    cmd.insert("view".into(), view.clone());
    if let Some(sel) = params.get("select").and_then(Value::as_str) {
        let id = d.db().resolve_ref(sel)?.id;
        cmd.insert("select".into(), json!(id));
    }
    *d.ui_view.lock().expect("ui mutex") = view.clone();
    d.ui_cmd(Value::Object(cmd));
    let shown = if params.get("show").and_then(Value::as_bool).unwrap_or(true) {
        show_list(d)?;
        true
    } else {
        false
    };
    Ok(json!({"view": view, "shown": shown}))
}

/// Open the new-session form pre-filled, on the list, for the user to confirm.
pub fn propose_session(d: &Daemon, params: &Value) -> Result<Value> {
    let mut prefill = Map::new();
    for k in ["repo", "project", "name", "branch", "model", "prompt"] {
        if let Some(v) = params
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            let v = if matches!(k, "repo" | "project") {
                expand_tilde(v)
            } else {
                v.to_string()
            };
            prefill.insert(k.into(), json!(v));
        }
    }
    if let Some(r) = prefill.get("repo").and_then(Value::as_str) {
        if rlord_core::git::main_repo_root(r).is_none() {
            bail!("{r} is not a git repository (use find_repos)");
        }
    }
    if let Some(w) = params.get("workspace").and_then(Value::as_bool) {
        prefill.insert("workspace".into(), json!(w));
    }
    d.ui_cmd(json!({"cmd": "new_session", "prefill": prefill}));
    show_list(d)?;
    Ok(json!({"proposed": prefill, "note": "the form is open on the user's screen for review"}))
}

/// Reopen the list's describe-a-session form (`n`) with the overlord's
/// corrections, for the user to confirm: their description, the hints as the
/// overlord would launch them, which hints it changed and a one-line note.
/// Submitting sends a `confirmed: yes` request back to the overlord.
pub fn confirm_new_session(d: &Daemon, params: &Value) -> Result<Value> {
    let description = params["description"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .context("missing 'description' (the user's, verbatim)")?;
    let mut prefill = Map::new();
    prefill.insert("description".into(), json!(description));
    for k in ["repo", "project", "model", "note"] {
        if let Some(v) = params[k].as_str().map(str::trim).filter(|v| !v.is_empty()) {
            let v = if matches!(k, "repo" | "project") {
                expand_tilde(v)
            } else {
                v.to_string()
            };
            prefill.insert(k.into(), json!(v));
        }
    }
    if let Some(r) = prefill.get("repo").and_then(Value::as_str) {
        if rlord_core::git::main_repo_root(r).is_none() {
            bail!("{r} is not a git repository (use find_repos)");
        }
    }
    for k in ["workspace", "tags", "changed"] {
        if !params[k].is_null() {
            prefill.insert(k.into(), params[k].clone());
        }
    }
    d.ui_cmd(json!({"cmd": "confirm_new_session", "prefill": prefill}));
    show_list(d)?;
    Ok(json!({"shown": prefill,
              "note": "the form is open on the user's screen; nothing is launched until they send it back (confirmed: yes)"}))
}
