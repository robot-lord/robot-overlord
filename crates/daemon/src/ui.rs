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
use rlord_core::usage::UsageMetric;

use crate::{methods, overlord, Daemon};

const DEFAULT_VIEW_KEY: &str = "default_view";
/// The Usage column's measure as the user (or the overlord) last picked it:
/// sticky across TUI and daemon restarts, over config `usage`.
const USAGE_KEY: &str = "ui_usage";
/// tmux session created to host the TUI when none is running.
const HOME_SESSION: &str = "rlord";

/// The view shape shared with the TUI.
fn normalize_view(v: &Value) -> Value {
    json!({
        "query": v.get("query").and_then(Value::as_str).unwrap_or(""),
        "sort": v.get("sort").and_then(Value::as_str).unwrap_or("updated"),
        "reverse": v.get("reverse").and_then(Value::as_bool).unwrap_or(false),
        // What the Usage column shows; null: the configured `usage`.
        "usage": v.get("usage").and_then(Value::as_str)
            .and_then(UsageMetric::parse).map(UsageMetric::label),
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
    let saved = d.db().kv_get(DEFAULT_VIEW_KEY)?;
    let mut view = normalize_view(
        &saved
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(json!({})),
    );
    // The measure lives in `USAGE_KEY` alone (one an older default view
    // saved is ignored); null: config `usage`.
    view["usage"] = json!(usage_pref(d).map(UsageMetric::label));
    Ok(view)
}

/// The Usage column's saved measure (None: never picked; config `usage`).
pub fn usage_pref(d: &Daemon) -> Option<UsageMetric> {
    d.db()
        .kv_get(USAGE_KEY)
        .ok()
        .flatten()
        .as_deref()
        .and_then(UsageMetric::parse)
}

/// A requested measure: `Some(None)` for null or "default" (forget the
/// pick: config `usage` again), None when not asked.
fn usage_param(params: &Value) -> Result<Option<Option<UsageMetric>>> {
    Ok(match params.get("usage") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(u)) if u.trim().eq_ignore_ascii_case("default") => Some(None),
        Some(Value::String(u)) => {
            Some(Some(UsageMetric::parse(u).with_context(|| {
                format!("unknown usage '{u}' (turns|tokens|context|default)")
            })?))
        }
        Some(v) => bail!("usage must be a string or null, not {v}"),
    })
}

fn save_usage(d: &Daemon, m: Option<UsageMetric>) -> Result<()> {
    match m {
        Some(m) => d.db().kv_set(USAGE_KEY, m.label()),
        None => d.db().kv_delete(USAGE_KEY),
    }
}

/// `ui.usage`: save the Usage column's measure with `usage` (null or
/// "default" forgets it: config `usage` again); without it, read it back
/// (null: never picked).
pub fn usage(d: &Daemon, params: &Value) -> Result<Value> {
    if let Some(m) = usage_param(params)? {
        save_usage(d, m)?;
    }
    Ok(json!({"usage": usage_pref(d).map(UsageMetric::label)}))
}

/// What the user is looking at right now.
pub fn state(d: &Daemon) -> Result<Value> {
    let clients = tmux::list_clients();
    let home = tmux::home().filter(|h| tmux::has_session(h));
    let user = tmux::user_client(&clients, home.as_deref()).cloned();
    let terminal = match &user {
        None => json!({"kind": "none", "note": "no terminal attached to tmux"}),
        Some(c) if Some(&c.session) == home.as_ref() => json!({"kind": "session_list"}),
        Some(c) if overlord::is_tmux_name(&c.session) => json!({"kind": "overlord"}),
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
    if params
        .get("from")
        .and_then(Value::as_str)
        .is_some_and(overlord::is_tmux_name)
    {
        return Ok(json!({"overlord": "already here"}));
    }
    if d.db().overlord()?.is_none() {
        let home = ensure_home()?;
        // Queue first, so a TUI only now starting picks it up as pending.
        d.ui_cmd(json!({"cmd": "overlord_setup"}));
        switch_user_to(&home)?;
        return Ok(json!({"setup": true}));
    }
    overlord::ensure(&d.db(), &d.config(), &json!({}))?;
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
        tmux::shell_quote(&methods::rlord_bin()),
    );
    // When this conversation started and how many came before: the
    // overlord restarts on a fresh one after sitting idle. (With several
    // agents to run an overlord on, the overlay's own switcher row says
    // it, for whichever is active: this title can't change once open.)
    let several = rlord_core::agents::Agent::ALL
        .into_iter()
        .filter(|a| rlord_core::agents::install::missing(*a).is_none())
        .count()
        > 1;
    let fresh = {
        let db = d.db();
        let found = overlord::session(&db)?.filter(|_| !several);
        found
            .map(|o| crate::recall::summary(&db, &o))
            .and_then(|v| crate::recall::fresh_label(&v))
            .map(|l| format!(" · {l}"))
            .unwrap_or_default()
    };
    let title = format!(
        " ◆ overlord{fresh} · {} close ",
        tmux::key_label(&d.config().tmux.overlord)
    );
    tmux::open_popup(
        &client,
        &d.config().overlord.width,
        &d.config().overlord.height,
        &title,
        &format!("fg={}", crate::overlord::color(&d.config())),
        &crate::overlord::overlay_style(&d.config()),
        &cmd,
    )?;
    *d.overlay.lock().expect("ui mutex") = Some((client, token));
    Ok(json!({"overlord": "shown"}))
}

/// Post-restart cleanup for `rlord reset`: close overlays, drop parked UI
/// commands, resync session state with tmux, re-apply rlord's tmux config and
/// key bindings, and restart the list (if it's open) so it starts fresh.
pub fn reset(d: &Daemon, params: &Value) -> Result<Value> {
    hide_overlord(d);
    // `--rescan`: forget every transcript read position and recount
    // sub-agent totals from the transcripts (after parsing changes, or if
    // numbers look wrong).
    if params["rescan"].as_bool() == Some(true) {
        d.db().clear_transcript_cursors()?;
        rlord_core::agents::transcript::clear_cursors();
    }
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
        let _ = overlord::ensure(&d.db(), &d.config(), &json!({})); // restyle
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
    let id = d.db().resolve_ref(r)?.id;
    let s = {
        // (Its lock before the DB: a start or restart may follow.)
        let _session = d.sessions.lock(&id);
        let db = d.db();
        let s = db.resolve_ref(&id)?;
        if overlord::is_overlord_role(&s.role) {
            overlord::ensure(&db, &d.config(), &json!({}))?;
        } else if s.status == Status::Stopped
            || !s.tmux_session.as_deref().is_some_and(tmux::has_session)
        {
            methods::session_restart(&db, &json!({"ref": s.id}))?;
        }
        db.resolve_ref(&s.id)?
    };
    d.changed();
    let t = s
        .tmux_session
        .as_deref()
        .context("session has no tmux session")?;
    if overlord::is_overlord_role(&s.role) {
        // Full screen: hand the window's size back from any overlay.
        let _ = tmux::auto_size_window(t);
    }
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
        let m = SortMode::parse(sort).with_context(|| {
            format!("unknown sort '{sort}' (updated|user|state|created|name|usage)")
        })?;
        obj.insert("sort".into(), json!(m.label()));
    }
    // (Saved below, once the whole view is known to be good.)
    let usage = usage_param(params)?;
    if let Some(m) = usage {
        // Null: the TUI goes back to config `usage`.
        obj.insert("usage".into(), json!(m.map(UsageMetric::label)));
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
    let mut cmd = Map::new();
    cmd.insert("cmd".into(), json!("set_view"));
    cmd.insert("view".into(), view.clone());
    if let Some(sel) = params.get("select").and_then(Value::as_str) {
        let id = d.db().resolve_ref(sel)?.id;
        cmd.insert("select".into(), json!(id));
    }
    // Valid throughout: now save. The measure is sticky (as when the user
    // picks it) and kept only under its own key, never in the default view.
    if let Some(m) = usage {
        save_usage(d, m)?;
    }
    if params.get("save_as_default").and_then(Value::as_bool) == Some(true) {
        let mut saved = view.clone();
        if let Some(o) = saved.as_object_mut() {
            o.remove("usage");
        }
        d.db().kv_set(DEFAULT_VIEW_KEY, &saved.to_string())?;
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
    for k in [
        "repo", "project", "name", "branch", "agent", "model", "prompt",
    ] {
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
    if let Some(a) = prefill.get("agent").and_then(Value::as_str) {
        rlord_core::agents::Agent::named(a)?;
    }
    if let Some(w) = params.get("workspace").and_then(Value::as_bool) {
        prefill.insert("workspace".into(), json!(w));
    }
    d.ui_cmd(json!({"cmd": "new_session", "prefill": prefill}));
    show_list(d)?;
    Ok(json!({"proposed": prefill, "note": "the form is open on the user's screen for review"}))
}

/// Which hints on a reopened form are the user's own picks and which the
/// overlord's, worked out by rlord from the request the form answers (its
/// hint values and labels), never relayed by the model. A hint the overlord
/// leaves out is a default again. The TUI shows the result as is: `chosen` (the user's), `resolved` (the
/// overlord's), anything else is a default.
fn apply_provenance(prefill: &mut Map<String, Value>, prev: &Value) {
    use crate::ask::in_list;
    const FOLDER: &str = " (plain folder, not a repo)";
    let folder = prefill.contains_key("project") && !prefill.contains_key("repo");
    let value = |k: &str| -> Option<Value> {
        match k {
            "repo" => prefill.get("repo").cloned().or_else(|| {
                prefill
                    .get("project")
                    .map(|f| json!(format!("{}{FOLDER}", f.as_str().unwrap_or(""))))
            }),
            "agent" => prefill.get("agent").cloned(),
            "model" => prefill.get("model").cloned(),
            // A plain folder has no worktree to pick.
            "worktree" => prefill.get("workspace").cloned().filter(|_| !folder),
            _ => prefill.get("tags").cloned(),
        }
    };
    // Compared loosely: a trailing slash, tag order or a '#' is no change.
    let norm = |k: &str, v: Option<&Value>| -> Option<Value> {
        let v = v?.clone();
        Some(match (k, &v) {
            // The same place however it's spelled: a repo by its main root
            // (a worktree or symlink of it is the same repo), a folder by
            // its real path.
            ("repo", Value::String(r)) => match r.strip_suffix(FOLDER) {
                Some(f) => {
                    let f = f.trim_end_matches('/');
                    let real = std::fs::canonicalize(f)
                        .ok()
                        .and_then(|p| p.to_str().map(str::to_string))
                        .unwrap_or_else(|| f.to_string());
                    json!(format!("{real}{FOLDER}"))
                }
                None => {
                    let r = r.trim_end_matches('/');
                    json!(rlord_core::git::main_repo_root(r).unwrap_or_else(|| r.to_string()))
                }
            },
            ("tags", Value::Array(a)) => {
                let mut t: Vec<String> = a
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|t| t.trim_start_matches('#').to_string())
                    .collect();
                t.sort();
                json!(t)
            }
            _ => v,
        })
    };
    // A plain folder forced the worktree off: that value was nobody's
    // choice, so it can't make a later one look like a default.
    let prev_folder = prev["repo"].as_str().is_some_and(|r| r.ends_with(FOLDER));
    let prev_value = |k: &str| match k {
        "worktree" if prev_folder => None,
        "worktree" => norm(k, prev.get("workspace")),
        _ => norm(k, prev.get(k)),
    };
    let mut chosen = Vec::new();
    let mut resolved = Vec::new();
    for k in ["repo", "agent", "model", "worktree", "tags"] {
        let Some(v) = value(k) else { continue };
        let changed = prefill
            .get("changed")
            .is_some_and(|c| c.as_array().is_some_and(|a| a.iter().any(|x| x == k)));
        // The user's pick only while it still holds the value they sent.
        let same = prev_value(k) == norm(k, Some(&v));
        if in_list(prev, "chosen", k) && !changed && same {
            chosen.push(k);
        } else if changed || in_list(prev, "resolved", k) || !same {
            resolved.push(k);
        }
    }
    prefill.insert("chosen".into(), json!(chosen));
    prefill.insert("resolved".into(), json!(resolved));
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
    for k in ["repo", "project", "agent", "model", "note"] {
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
    if let Some(a) = prefill.get("agent").and_then(Value::as_str) {
        rlord_core::agents::Agent::named(a)?;
    }
    // "" is the agent default: a model the overlord reset, not one it left out.
    if params["model"]
        .as_str()
        .is_some_and(|m| m.trim().is_empty())
    {
        prefill.insert("model".into(), json!(""));
    }
    for k in ["workspace", "tags", "changed"] {
        if !params[k].is_null() {
            prefill.insert(k.into(), params[k].clone());
        }
    }
    // The request this form answers (none for one the overlord starts
    // itself): its hints' labels come from it.
    let provenance = match params["request_id"]
        .as_str()
        .filter(|r| !r.trim().is_empty())
    {
        Some(r) => {
            let v = crate::ask::provenance_of(d, r)?;
            if v.is_null() {
                bail!("unknown request id '{r}': pass the `request_id:` from the new-session request you're answering, or omit it");
            }
            v
        }
        None => Value::Null,
    };
    apply_provenance(&mut prefill, &provenance);
    // The overlord is now waiting on the user (its fresh start waits too).
    d.db()
        .kv_set("confirm_pending", &rlord_core::types::now_ms().to_string())?;
    d.ui_cmd(json!({"cmd": "confirm_new_session", "prefill": prefill}));
    show_list(d)?;
    Ok(json!({"shown": prefill,
              "note": "the form is open on the user's screen; nothing is launched until they send it back (confirmed: yes)"}))
}

/// Every key rlord binds, as set now, with what stands in each one's way
/// (plain code: the config, rlord's tmux server, Claude Code's key file).
pub fn keys_list(d: &Daemon) -> Result<Value> {
    use rlord_core::keys::{self, Env, Key, SLOTS};
    let cfg = d.config();
    let env = Env::read();
    let rows: Vec<Value> = SLOTS
        .iter()
        .map(|s| {
            let spec = s.current(&cfg);
            let key = Key::parse(&spec);
            let findings: Vec<Value> = key
                .as_ref()
                .map(|k| keys::check(&cfg, &env, s, k))
                .unwrap_or_default()
                .into_iter()
                .map(|f| json!({"level": f.level.name(), "text": f.text}))
                .collect();
            json!({
                "id": s.id(),
                "does": s.does,
                "where": s.scope.name(),
                "key": key.as_ref().map(Key::label).unwrap_or_else(|| "off".into()),
                "default": s.default_spec(),
                "findings": findings,
            })
        })
        .collect();
    Ok(json!({
        "keys": rows,
        "note": "taken = doesn't work as set; shadows = rlord takes it from something else. Keys change only on the user's key screen (open_keys): only they can test a key reaches rlord.",
    }))
}

/// Open the key screen in the list (at one key, if given) and show it.
pub fn keys_open(d: &Daemon, params: &Value) -> Result<Value> {
    let row = params.get("key").and_then(Value::as_str).unwrap_or("");
    if !row.is_empty() && rlord_core::keys::slot(row).is_none() {
        bail!("unknown key id '{row}' (see keys_list)");
    }
    d.ui_cmd(json!({"cmd": "keys", "row": row}));
    show_list(d)?;
    Ok(
        json!({"shown": "key screen", "note": "the user presses the new key there; it's tested and saved to config.toml when they confirm"}),
    )
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    fn run(prefill: Value, prev: Value) -> Value {
        let mut m = prefill.as_object().unwrap().clone();
        apply_provenance(&mut m, &prev);
        Value::Object(m)
    }

    #[test]
    fn labels_come_from_the_request_answered() {
        let prev = json!({"chosen": ["model"], "resolved": ["repo"],
                          "repo": "/r/web", "model": "opus", "workspace": true, "tags": []});
        // Round two: it changes only the worktree and omits the rest: those
        // are defaults again (it was told to send every hint).
        let p = run(
            json!({"workspace": false, "changed": ["worktree"]}),
            prev.clone(),
        );
        assert!(p["model"].is_null());
        assert_eq!(p["chosen"], json!([]));
        assert_eq!(p["resolved"], json!(["worktree"]));
        // A user's pick it sends back with another value, unlisted, is its.
        let p = run(json!({"model": "sonnet", "changed": []}), prev.clone());
        assert_eq!(p["chosen"], json!([]));
        assert_eq!(p["resolved"], json!(["model"]));
        // A value it fills in for a default, without saying so, is its pick.
        let p = run(
            json!({"model": "opus", "tags": ["web"], "changed": []}),
            prev,
        );
        assert_eq!(p["chosen"], json!(["model"]));
        assert_eq!(p["resolved"], json!(["tags"]));
        // No request in play (asked in conversation): whatever it sends is its pick.
        let p = run(json!({"model": "sonnet", "changed": []}), Value::Null);
        assert_eq!(p["chosen"], json!([]));
        assert_eq!(p["resolved"], json!(["model"]));
        // A plain folder carries no worktree pick.
        let p = run(
            json!({"project": "/tmp", "workspace": true, "changed": ["repo"]}),
            json!({"chosen": ["worktree"], "workspace": true}),
        );
        assert_eq!(p["chosen"], json!([]));
        assert_eq!(p["resolved"], json!(["repo"]));
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;
    use rlord_core::db::Db;

    /// The measure is sticky: whoever picks it (the TUI, or the overlord's
    /// `set_list_view usage`), the next start's default view has it, and
    /// list requests without one use it over config `usage`.
    #[test]
    fn the_usage_measure_is_sticky() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Default::default());
        assert_eq!(usage(&d, &json!({})).unwrap()["usage"], Value::Null);
        assert_eq!(default_view(&d).unwrap()["usage"], Value::Null);
        assert_eq!(crate::usage::metric(&d, &json!({})), UsageMetric::Turns);
        usage(&d, &json!({"usage": "tokens"})).unwrap();
        assert_eq!(default_view(&d).unwrap()["usage"], "tokens");
        assert_eq!(crate::usage::metric(&d, &json!({})), UsageMetric::Tokens);
        assert!(usage(&d, &json!({"usage": "bogus"})).is_err());
        // The overlord's set_list_view (not shown: no tmux here).
        set_view(&d, &json!({"usage": "context", "show": false})).unwrap();
        assert_eq!(usage(&d, &json!({})).unwrap()["usage"], "context");
        assert_eq!(default_view(&d).unwrap()["usage"], "context");
        // A set_list_view that fails saves nothing, the measure included.
        assert!(set_view(&d, &json!({"usage": "turns", "tab": "nope", "show": false})).is_err());
        assert_eq!(usage(&d, &json!({})).unwrap()["usage"], "context");
        // Saving the default view keeps the measure out of it.
        set_view(
            &d,
            &json!({"sort": "name", "save_as_default": true, "show": false}),
        )
        .unwrap();
        let saved = d.db().kv_get(DEFAULT_VIEW_KEY).unwrap().unwrap();
        assert!(!saved.contains("usage"), "{saved}");
        // A measure an older default view saved is ignored.
        d.db()
            .kv_set(DEFAULT_VIEW_KEY, r#"{"usage": "turns", "sort": "name"}"#)
            .unwrap();
        let v = default_view(&d).unwrap();
        assert_eq!(
            (v["usage"].as_str(), v["sort"].as_str()),
            (Some("context"), Some("name"))
        );
        // Forgetting it: config `usage` again (null, "default", or the
        // overlord's "default").
        usage(&d, &json!({"usage": null})).unwrap();
        assert_eq!(default_view(&d).unwrap()["usage"], Value::Null);
        assert_eq!(crate::usage::metric(&d, &json!({})), UsageMetric::Turns);
        usage(&d, &json!({"usage": "tokens"})).unwrap();
        usage(&d, &json!({"usage": "default"})).unwrap();
        assert_eq!(usage(&d, &json!({})).unwrap()["usage"], Value::Null);
        usage(&d, &json!({"usage": "tokens"})).unwrap();
        let r = set_view(&d, &json!({"usage": "default", "show": false})).unwrap();
        assert_eq!(r["view"]["usage"], Value::Null);
        assert_eq!(usage(&d, &json!({})).unwrap()["usage"], Value::Null);
    }
}
