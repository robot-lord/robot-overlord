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

use rlord_core::config::{expand_tilde, UiTarget};
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

/// The view shape shared with the TUI (and the web pages' reports).
pub(crate) fn normalize_view(v: &Value) -> Value {
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

// ---- which screen a UI command lands on ----
//
// A surface is somewhere the user looks at rlord: the terminal (a tmux
// client, with or without the TUI), a web page, or a paired device (the
// phone, through the relay). A command goes to one of them: the one the
// user did something on last (`[web] ui_target` can prefer one). The
// others get a one-line notice.

/// Where a UI command goes, chosen once per tool call (`target`) so the
/// command and anything that brings it to the front agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The user's terminal (this tmux client).
    Terminal(tmux::Client),
    /// The web page with this id.
    Web(String),
    /// The paired device with this id (the phone).
    Device(String),
    /// Nowhere to show anything.
    None,
}

impl Target {
    /// `"terminal"`, `"web"`, `"phone"` or `"none"`: the `on` in tool
    /// answers.
    pub fn name(&self) -> &'static str {
        match self {
            Target::Terminal(_) => "terminal",
            Target::Web(_) => "web",
            Target::Device(_) => "phone",
            Target::None => "none",
        }
    }
}

/// What `pick` chose (`Web` names an entry of its `pages`, `Device` one
/// of its `devices`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Picked<'a> {
    Terminal,
    Web(&'a str),
    Device(&'a str),
    None,
}

/// A surface besides the terminal, for `pick`: its id, its last use in ms
/// (None: never used) and its order of arrival.
pub(crate) type Use = (String, Option<i64>, u64);

/// The surface a command goes to. `terminal`: the user's terminal's last
/// input, epoch seconds (None: no terminal attached). `pages` and
/// `devices`: each open page's and each paired device's use. With
/// `Auto`, the later use wins, compared in whole seconds (tmux's
/// resolution); a tie or an unknown goes to the terminal. Among pages and
/// devices, the latest use, then the newest. A preference (`Terminal`,
/// `Web`: the pages and devices) wins whenever its surface is there.
pub(crate) fn pick<'a>(
    terminal: Option<i64>,
    pages: &'a [Use],
    devices: &'a [Use],
    pref: UiTarget,
) -> Picked<'a> {
    let other = pages
        .iter()
        .map(|p| (p, false))
        .chain(devices.iter().map(|p| (p, true)))
        .max_by_key(|((_, at, seq), _)| (*at, *seq));
    let away = |(p, device): (&'a Use, bool)| {
        if device {
            Picked::Device(&p.0)
        } else {
            Picked::Web(&p.0)
        }
    };
    match (terminal, other) {
        (None, None) => Picked::None,
        (Some(_), None) => Picked::Terminal,
        (None, Some(o)) => away(o),
        (Some(t), Some(o)) => match pref {
            UiTarget::Terminal => Picked::Terminal,
            UiTarget::Web => away(o),
            UiTarget::Auto => match o.0 .1 {
                Some(ms) if ms.div_euclid(1000) > t => away(o),
                _ => Picked::Terminal,
            },
        },
    }
}

/// The screen the overlord's next UI command lands on (see `pick`).
pub fn target(d: &Daemon) -> Target {
    target_among(d, true)
}

/// `target`, for a tool whose screen needs this computer (a terminal or a
/// local page: the code-folder form, the key screen): a phone is never
/// chosen.
pub fn target_local(d: &Daemon) -> Target {
    target_among(d, false)
}

fn target_among(d: &Daemon, phones: bool) -> Target {
    let clients = tmux::list_clients();
    let home = tmux::home();
    let user = tmux::user_client(&clients, home.as_deref()).cloned();
    let pages = page_uses(d);
    let devices = if phones { device_uses(d) } else { vec![] };
    let pref = d.config().web.ui_target;
    let picked = match pick(user.as_ref().map(|c| c.activity), &pages, &devices, pref) {
        Picked::Terminal => None,
        Picked::Web(id) => Some(Target::Web(id.to_string())),
        Picked::Device(id) => Some(Target::Device(id.to_string())),
        Picked::None => Some(Target::None),
    };
    match (picked, user) {
        (Some(t), _) => t,
        (None, Some(c)) => Target::Terminal(c),
        (None, None) => Target::None,
    }
}

/// The error when there is no screen to show anything on.
fn no_screen() -> anyhow::Error {
    anyhow::anyhow!(
        "the user has no rlord screen open: they need to run `rlord` in a terminal, or open the web UI (`rlord web`)"
    )
}

/// The error when `what` has no screen on this computer to open on (it
/// never opens on a phone).
pub(crate) fn no_local_screen(what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{what} opens only on this computer, never on a phone, and the user has no rlord screen open here: they need to run `rlord` in a terminal, or open the web UI (`rlord web`)"
    )
}

/// What a surface is called in notices.
fn where_(t: &Target) -> &'static str {
    match t {
        Target::Web(_) => "in the web UI",
        Target::Device(_) => "on the phone",
        _ => "in the terminal",
    }
}

/// The id of the most recently used open page, if any.
pub(crate) fn latest_page(d: &Daemon) -> Option<String> {
    page_uses(d)
        .into_iter()
        .max_by_key(|(_, at, seq)| (*at, *seq))
        .map(|p| p.0)
}

/// Each open page's last use: its own (an op, a report of input) or the
/// last key typed into the pane it shows (a browser's tmux client).
fn page_uses(d: &Daemon) -> Vec<(String, Option<i64>, u64)> {
    let pages = d.web_ui().pages().to_vec();
    let mut panes: std::collections::HashMap<String, Option<i64>> = Default::default();
    pages
        .into_iter()
        .map(|p| {
            let pane = pane_of(d, &p.route).and_then(|t| {
                *panes.entry(t.clone()).or_insert_with(|| {
                    tmux::view_clients(&t)
                        .iter()
                        .map(|c| c.activity * 1000)
                        .max()
                })
            });
            let at = match (p.last_input, pane) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
            (p.id, at, p.seq)
        })
        .collect()
}

/// Each paired device's last use (an op from it, a report of input). A
/// device the plugin no longer lists as paired isn't one; one never used
/// counts only while it has a live session.
fn device_uses(d: &Daemon) -> Vec<Use> {
    let paired = crate::relay::paired(d);
    d.web_ui()
        .devices()
        .iter()
        .filter(|p| {
            paired
                .iter()
                .any(|q| q.id == p.id && (q.linked || p.last_input.is_some()))
        })
        .map(|p| (p.id.clone(), p.last_input, p.seq))
        .collect()
}

/// The id of the most recently used paired device, if any.
fn latest_device(d: &Daemon) -> Option<String> {
    device_uses(d)
        .into_iter()
        .max_by_key(|(_, at, seq)| (*at, *seq))
        .map(|p| p.0)
}

/// The tmux session a page's route shows, if it shows a pane.
fn pane_of(d: &Daemon, route: &Value) -> Option<String> {
    let db = d.db();
    let s = match route["kind"].as_str()? {
        "overlord" => overlord::session(&db).ok().flatten()?,
        "session" => db.resolve_ref(route["id"].as_str()?).ok()?,
        _ => return None,
    };
    s.tmux_session
}

/// The web part of `ui.state`: the most recently used page (null: none).
fn web_state(d: &Daemon, target: &Target) -> Value {
    let id = match target {
        Target::Web(id) => Some(id.clone()),
        _ => latest_page(d),
    };
    let w = d.web_ui();
    let Some(p) = id.and_then(|id| w.page(&id)).cloned() else {
        return Value::Null;
    };
    let open = w.pages().len();
    drop(w);
    json!({
        "open": open > 0,
        "pages": open,
        "showing": showing(d, &p.route),
        "visible": p.visible,
        "form": p.form,
    })
}

/// The phone part of `ui.state`, shaped like `web`: the target device, or
/// the most recently used one (null: no device has used rlord). `open`:
/// it has a live session now; `devices`: how many paired devices have.
fn phone_state(d: &Daemon, target: &Target) -> Value {
    let id = match target {
        Target::Device(id) => Some(id.clone()),
        _ => latest_device(d),
    };
    let Some(p) = id.and_then(|id| d.web_ui().device(&id).cloned()) else {
        return Value::Null;
    };
    let paired = crate::relay::paired(d);
    let me = paired.iter().find(|q| q.id == p.id);
    json!({
        "open": me.is_some_and(|q| q.linked),
        "devices": paired.iter().filter(|q| q.linked).count(),
        "name": me.map(|q| q.name.clone()),
        "showing": showing(d, &p.route),
        "visible": p.visible,
        "form": p.form,
    })
}

/// What a page's or a device's route shows, for `ui.state`.
fn showing(d: &Daemon, route: &Value) -> Value {
    let kind = route["kind"].as_str().unwrap_or("list");
    match kind {
        "session" | "overlord" => {
            let db = d.db();
            let s = if kind == "overlord" {
                overlord::session(&db).ok().flatten()
            } else {
                route["id"].as_str().and_then(|i| db.resolve_ref(i).ok())
            };
            match s {
                Some(s) => json!({"kind": kind, "id": s.id, "name": s.name}),
                None => json!({"kind": kind}),
            }
        }
        _ => json!({"kind": "session_list"}),
    }
}

/// What the user is looking at right now.
pub fn state(d: &Daemon) -> Result<Value> {
    let target = target(d);
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
    let web = web_state(d, &target);
    let phone = phone_state(d, &target);
    Ok(json!({
        "target": target.name(),
        "terminal": terminal,
        "web": web,
        "phone": phone,
        "tui_running": d.tui_count() > 0,
        "overlord_overlay_open": d.overlay.lock().expect("ui mutex").is_some(),
        "current_view": current_view(d, &target)?,
        "default_view": default_view(d)?,
    }))
}

/// The list view on `target`: a page's or a device's as it reported it,
/// else the TUI's (any: the default view until one is reported).
fn current_view(d: &Daemon, target: &Target) -> Result<Value> {
    let v = match target {
        Target::Web(id) => d.web_ui().page(id).map(|p| p.view.clone()),
        Target::Device(id) => d.web_ui().device(id).map(|p| p.view.clone()),
        _ => Some(d.ui_view.lock().expect("ui mutex").clone()),
    };
    match v.filter(|v| !v.is_null()) {
        Some(v) => Ok(normalize_view(&v)),
        None => default_view(d),
    }
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

/// Switch terminal `c` to `tmux_session` (the client `target` chose).
fn switch_to(c: &tmux::Client, tmux_session: &str) -> Result<()> {
    if c.session != tmux_session {
        tmux::switch_client_of(&c.name, tmux_session)?;
    }
    Ok(())
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
    *d.pending_web.lock().expect("ui mutex") = None;
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

/// `show_list` (the overlord's): the list on the user's screen.
pub fn show_list(d: &Daemon) -> Result<Value> {
    let t = target(d);
    show_list_on(d, &t)?;
    Ok(json!({"shown": "session_list", "on": t.name()}))
}

/// Show the list on `t`: the terminal goes to the TUI (hiding the
/// overlay); a page or a device is sent `show_list`.
fn show_list_on(d: &Daemon, t: &Target) -> Result<()> {
    match t {
        Target::Terminal(c) => {
            let home = ensure_home()?;
            hide_overlord(d);
            switch_to(c, &home)
        }
        Target::Web(_) | Target::Device(_) => {
            d.ui_cmd_to(t, json!({"cmd": "show_list"}));
            Ok(())
        }
        Target::None => Err(no_screen()),
    }
}

/// `goto_session`: the session on the user's screen, resumed first if it
/// is stopped.
pub fn goto(d: &Daemon, params: &Value) -> Result<Value> {
    let r = methods::p_str(params, "ref")?;
    let to = target(d);
    if to == Target::None {
        return Err(no_screen());
    }
    let s = ensure_running(d, r)?;
    let t = s
        .tmux_session
        .as_deref()
        .context("session has no tmux session")?;
    let is_overlord = overlord::is_overlord_role(&s.role);
    match &to {
        Target::Terminal(c) => {
            if is_overlord {
                // Full screen: hand the window's size back from any overlay.
                let _ = tmux::auto_size_window(t);
            }
            hide_overlord(d);
            switch_to(c, t)?;
        }
        _ => d.ui_cmd_to(
            &to,
            json!({"cmd": "goto", "id": s.id, "overlord": is_overlord}),
        ),
    }
    d.notice_others(
        &to,
        &format!("the overlord showed {} {}", s.name, where_(&to)),
    );
    Ok(json!({"switched_to": {"id": s.id, "name": s.name}, "on": to.name()}))
}

/// Make sure session `r` runs: an overlord is ensured, a stopped session
/// (or one whose tmux session is gone) is restarted on its conversation.
/// Returns its row as it is now. (Enter in the list, and the web UI's
/// Resume.)
pub fn ensure_running(d: &Daemon, r: &str) -> Result<rlord_core::types::Session> {
    let found = d.db().resolve_ref(r)?;
    let id = found.id.clone();
    let needs_start = |s: &rlord_core::types::Session| {
        s.status == Status::Stopped || !s.tmux_session.as_deref().is_some_and(tmux::has_session)
    };
    // (Asked before the locks, and only when it will be restarted: see
    // `Daemon::restart_hooks`.)
    let hooks = (!overlord::is_overlord_role(&found.role) && needs_start(&found))
        .then(|| d.restart_hooks(&id))
        .flatten();
    let s = {
        // (Its lock before the DB: a start or restart may follow.)
        let _session = d.sessions.lock(&id);
        let db = d.db();
        let s = db.resolve_ref(&id)?;
        if overlord::is_overlord_role(&s.role) {
            overlord::ensure(&db, &d.config(), &json!({}))?;
        } else if needs_start(&s) {
            methods::session_restart(&db, &json!({"ref": s.id, "hooks_work": hooks}))?;
        }
        db.resolve_ref(&s.id)?
    };
    d.changed();
    Ok(s)
}

/// Set (and optionally save as default) the list view on the user's
/// screen (the TUI's, or a page's); show it unless `show: false`.
pub fn set_view(d: &Daemon, params: &Value) -> Result<Value> {
    let to = target(d);
    let mut view = current_view(d, &to)?;
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
    match &to {
        Target::Web(id) => {
            if let Some(p) = d.web_ui().page_mut(id) {
                p.view = view.clone();
            }
            d.ui_cmd_to(&to, Value::Object(cmd));
        }
        Target::Device(id) => {
            d.web_ui().device_mut(id).view = view.clone();
            d.ui_cmd_to(&to, Value::Object(cmd));
        }
        // (No screen: kept for the next TUI, as before.)
        _ => {
            *d.ui_view.lock().expect("ui mutex") = view.clone();
            d.ui_cmd(Value::Object(cmd));
        }
    }
    let shown = if params.get("show").and_then(Value::as_bool).unwrap_or(true) {
        show_list_on(d, &to)?;
        true
    } else {
        false
    };
    Ok(json!({"view": view, "shown": shown, "on": to.name()}))
}

/// The proposals `propose_session` made, newest last (at most this many):
/// the overlord's background for each, which a page's launch refers to by
/// id and never sends itself.
const PROPOSALS: usize = 8;

/// Keep a proposal's background; its id.
fn keep_proposal(d: &Daemon, background: Value) -> String {
    let mut p = d.proposals.lock().expect("ui mutex");
    // `p<n>-<boot id>`: a form left open across a daemon restart can't
    // name a newer proposal (the counter starts again after a restart).
    let n = p.back().map_or(0, |(id, _)| {
        id.trim_start_matches('p')
            .split('-')
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0)
    });
    let id = format!("p{}-{}", n + 1, d.boot_id);
    p.push_back((id.clone(), background));
    while p.len() > PROPOSALS {
        p.pop_front();
    }
    id
}

/// The background (`context`, `related`) kept for proposal `id`.
pub(crate) fn proposal(d: &Daemon, id: &str) -> Result<Value> {
    d.proposals
        .lock()
        .expect("ui mutex")
        .iter()
        .find(|(i, _)| i == id)
        .map(|(_, v)| v.clone())
        .with_context(|| {
            format!("unknown proposal '{id}': it is too old (ask the overlord to propose it again)")
        })
}

/// Send form command `cmd` to the user's screen: on the terminal the list
/// is shown too (the TUI opens it there); on a page the command opens it.
/// The other screen gets a notice naming the form.
fn open_form(d: &Daemon, to: &Target, cmd: Value, form: &str) -> Result<()> {
    if *to == Target::None {
        return Err(no_screen());
    }
    d.ui_cmd_to(to, cmd);
    show_form(d, to, form)
}

/// After a form's command went to `to`: on the terminal, show the list
/// (where the TUI opens it); tell the other screen.
pub(crate) fn show_form(d: &Daemon, to: &Target, form: &str) -> Result<()> {
    match to {
        Target::None => return Err(no_screen()),
        Target::Terminal(_) => show_list_on(d, to)?,
        Target::Web(_) | Target::Device(_) => {}
    }
    d.notice_others(to, &format!("the overlord opened {form} {}", where_(to)));
    Ok(())
}

/// Open the new-session form pre-filled, on the list, for the user to confirm.
pub fn propose_session(d: &Daemon, params: &Value) -> Result<Value> {
    let mut prefill = Map::new();
    for k in [
        "repo", "project", "name", "branch", "agent", "model", "prompt", "context", "base",
        "prefer",
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
    // The overlord's background and related sessions, checked now (not
    // when the user launches): the form shows them, read-only.
    let related = crate::methods::ref_list(params, "related");
    {
        let db = d.db();
        let (_, ids) = crate::context::check(
            &db,
            prefill.get("context").and_then(Value::as_str),
            &related,
            None,
        )?;
        // The sessions as resolved now (ids), and their names to show.
        if !ids.is_empty() {
            let names: Vec<String> = ids
                .iter()
                .filter_map(|i| db.resolve_ref(i).ok())
                .map(|s| crate::context::clean(&s.name))
                .collect();
            prefill.insert("related".into(), json!(ids));
            prefill.insert("related_names".into(), json!(names));
        }
    }
    // What the TUI shows of the overlord's text: no control characters.
    if let Some(c) = prefill.get("context").and_then(Value::as_str) {
        let c = crate::context::clean(c);
        prefill.insert("context".into(), json!(c));
    }
    let to = target(d);
    if to == Target::None {
        return Err(no_screen());
    }
    // A page sends back only this id: the background stays here.
    let id = keep_proposal(
        d,
        json!({"context": prefill.get("context"), "related": prefill.get("related")}),
    );
    prefill.insert("proposal".into(), json!(id));
    open_form(
        d,
        &to,
        json!({"cmd": "new_session", "prefill": prefill}),
        "a new-session form",
    )?;
    Ok(json!({"proposed": prefill, "on": to.name(),
              "note": "the form is open on the user's screen for review"}))
}

/// A `base` the overlord leaves out is unchanged: the one the request
/// sent (with its side), if any. `""` is the configured default.
fn keep_base(prefill: &mut Map<String, Value>, params: &Value, prev: &Value) {
    if !params["base"].is_null() {
        return;
    }
    let sent = |k: &str| {
        prev[k]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let Some(b) = sent("base") else {
        return;
    };
    prefill.insert("base".into(), json!(b));
    if !prefill.contains_key("prefer") {
        if let Some(p) = sent("prefer") {
            prefill.insert("prefer".into(), json!(p));
        }
    }
}

/// Which hints on a reopened form are the user's own picks and which the
/// overlord's, worked out by rlord from the request the form answers (its
/// hint values and labels), never relayed by the model. A hint the overlord
/// leaves out is a default again. The TUI shows the result as is: `chosen` (the user's), `resolved` (the
/// overlord's), anything else is a default.
/// `default_base`: the configured `[session] base` ("latest" or "head"),
/// what a missing base means.
fn apply_provenance(prefill: &mut Map<String, Value>, prev: &Value, default_base: &str) {
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
            "base" => prefill.get("base").cloned(),
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
            // (Missing or "" is the configured default; "latest" and
            // "head" in any case.)
            ("base", Value::String(b)) if b.trim().is_empty() => json!(default_base),
            ("base", Value::String(b))
                if b.trim().eq_ignore_ascii_case("latest")
                    || b.trim().eq_ignore_ascii_case("head") =>
            {
                json!(b.trim().to_ascii_lowercase())
            }
            ("base", Value::Null) => json!(default_base),
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
        "base" => norm(k, Some(prev.get("base").unwrap_or(&Value::Null))),
        _ => norm(k, prev.get(k)),
    };
    let mut chosen = Vec::new();
    let mut resolved = Vec::new();
    for k in ["repo", "agent", "model", "worktree", "base", "tags"] {
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
    for k in [
        "repo", "project", "agent", "model", "base", "prefer", "note",
    ] {
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
    keep_base(&mut prefill, params, &provenance);
    let default_base = if d.config().session.base_is_head() {
        "head"
    } else {
        "latest"
    };
    apply_provenance(&mut prefill, &provenance, default_base);
    let to = target(d);
    if to == Target::None {
        return Err(no_screen());
    }
    // The overlord is now waiting on the user (its fresh start waits too).
    d.db()
        .kv_set("confirm_pending", &rlord_core::types::now_ms().to_string())?;
    open_form(
        d,
        &to,
        json!({"cmd": "confirm_new_session", "prefill": prefill}),
        "your new-session form",
    )?;
    Ok(json!({"shown": prefill, "on": to.name(),
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

/// Open the key screen (at one key, if given) and show it. Keys are
/// changed only in a terminal (the screen tests that a key reaches rlord
/// there), so it goes to the terminal whenever one is attached; with only
/// a web page, the page shows the keys read-only. Never on a phone.
pub fn keys_open(d: &Daemon, params: &Value) -> Result<Value> {
    let row = params.get("key").and_then(Value::as_str).unwrap_or("");
    if !row.is_empty() && rlord_core::keys::slot(row).is_none() {
        bail!("unknown key id '{row}' (see keys_list)");
    }
    let clients = tmux::list_clients();
    let home = tmux::home();
    let to = match tmux::user_client(&clients, home.as_deref()) {
        Some(c) => Target::Terminal(c.clone()),
        None => target_local(d),
    };
    if to == Target::None {
        return Err(no_local_screen("the key screen"));
    }
    open_form(d, &to, json!({"cmd": "keys", "row": row}), "the key screen")?;
    let note = match to {
        Target::Web(_) => "the web UI shows the keys read-only: changing one needs a terminal (the user runs `rlord` and opens the key screen there)",
        _ => "the user presses the new key there; it's tested and saved to config.toml when they confirm",
    };
    Ok(json!({"shown": "key screen", "on": to.name(), "note": note}))
}

#[cfg(test)]
mod target_tests {
    use super::*;

    /// Proposals are kept, the newest 8, and an unknown id is refused.
    #[test]
    fn the_proposal_store() {
        let d = Daemon::new(
            rlord_core::db::Db::open_in_memory().unwrap(),
            Default::default(),
        );
        let ids: Vec<String> = (0..10)
            .map(|i| keep_proposal(&d, json!({"context": format!("c{i}")})))
            .collect();
        assert_eq!(ids[0], format!("p1-{}", d.boot_id));
        assert_eq!(ids[9], format!("p10-{}", d.boot_id));
        // After a restart (another boot id) an old id names nothing.
        assert!(proposal(&d, &ids[9].replace(&d.boot_id, "other")).is_err());
        assert_eq!(proposal(&d, &ids[9]).unwrap()["context"], "c9");
        assert_eq!(proposal(&d, &ids[2]).unwrap()["context"], "c2");
        assert!(proposal(&d, &ids[1]).is_err());
        assert!(proposal(&d, "nope")
            .unwrap_err()
            .to_string()
            .contains("too old"));
        assert_eq!(d.proposals.lock().unwrap().len(), PROPOSALS);
    }

    fn pages(p: &[(&str, Option<i64>, u64)]) -> Vec<(String, Option<i64>, u64)> {
        p.iter().map(|(i, a, s)| (i.to_string(), *a, *s)).collect()
    }

    #[test]
    fn the_surface_used_last_wins() {
        use Picked::{Terminal, Web};
        let auto = UiTarget::Auto;
        let no: &[Use] = &[];
        // Nothing, one surface, the other.
        assert_eq!(pick(None, no, no, auto), Picked::None);
        assert_eq!(pick(Some(100), no, no, auto), Terminal);
        let one = pages(&[("p1", None, 1)]);
        assert_eq!(pick(None, &one, no, auto), Web("p1"));
        // Both: the later, in whole seconds; a tie or a page never used
        // goes to the terminal.
        assert_eq!(pick(Some(100), &one, no, auto), Terminal);
        let used = pages(&[("p1", Some(101_000), 1)]);
        assert_eq!(pick(Some(100), &used, no, auto), Web("p1"));
        let tie = pages(&[("p1", Some(100_900), 1)]);
        assert_eq!(pick(Some(100), &tie, no, auto), Terminal);
        assert_eq!(pick(Some(102), &used, no, auto), Terminal);
        // Several pages: the latest use, then the newest.
        let several = pages(&[
            ("p1", Some(105_000), 1),
            ("p2", Some(103_000), 2),
            ("p3", None, 3),
        ]);
        assert_eq!(pick(Some(100), &several, no, auto), Web("p1"));
        let unused = pages(&[("p1", None, 1), ("p2", None, 2)]);
        assert_eq!(pick(None, &unused, no, auto), Web("p2"));
        // A preference wins while its surface is there.
        assert_eq!(pick(Some(100), &used, no, UiTarget::Terminal), Terminal);
        assert_eq!(pick(Some(200), &one, no, UiTarget::Web), Web("p1"));
        assert_eq!(pick(None, &one, no, UiTarget::Terminal), Web("p1"));
        assert_eq!(pick(Some(1), no, no, UiTarget::Web), Terminal);
    }

    /// Devices stand beside pages by the same rule: the latest use wins,
    /// then the newest; against the terminal, the later in whole seconds.
    #[test]
    fn a_device_is_picked_as_a_page_is() {
        use Picked::{Device, Terminal, Web};
        let auto = UiTarget::Auto;
        let no: &[Use] = &[];
        let phone = pages(&[("ab12", Some(110_000), 4)]);
        assert_eq!(pick(None, no, &phone, auto), Device("ab12"));
        assert_eq!(pick(Some(100), no, &phone, auto), Device("ab12"));
        assert_eq!(pick(Some(110), no, &phone, auto), Terminal);
        // A page used later wins; one used earlier doesn't.
        let later = pages(&[("p1", Some(120_000), 1)]);
        assert_eq!(pick(Some(100), &later, &phone, auto), Web("p1"));
        let earlier = pages(&[("p1", Some(105_000), 1)]);
        assert_eq!(pick(Some(100), &earlier, &phone, auto), Device("ab12"));
        // Never used: by arrival, a device among pages too.
        let unused = pages(&[("p1", None, 1)]);
        let new_phone = pages(&[("cd34", None, 2)]);
        assert_eq!(pick(None, &unused, &new_phone, auto), Device("cd34"));
        // The `web` preference covers devices; `terminal` beats them.
        assert_eq!(
            pick(Some(200), &earlier, &phone, UiTarget::Web),
            Device("ab12")
        );
        assert_eq!(pick(Some(1), no, &phone, UiTarget::Terminal), Terminal);
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    fn run(prefill: Value, prev: Value) -> Value {
        let mut m = prefill.as_object().unwrap().clone();
        apply_provenance(&mut m, &prev, "latest");
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

    /// The base hint: the user's pick stays theirs while it holds; "" and
    /// "latest" are the same default.
    #[test]
    fn base_labels() {
        let prev = json!({"chosen": ["base"], "base": "feat"});
        let p = run(json!({"base": "feat", "changed": []}), prev.clone());
        assert_eq!(p["chosen"], json!(["base"]));
        let p = run(json!({"base": "main", "changed": ["base"]}), prev.clone());
        assert_eq!(p["resolved"], json!(["base"]));
        // Sent as "" (none here): back to the default, no one's pick.
        let p = run(json!({"changed": []}), prev);
        assert!(p.get("base").is_none());
        assert_eq!((&p["chosen"], &p["resolved"]), (&json!([]), &json!([])));
        let p = run(
            json!({"base": "latest", "changed": []}),
            json!({"base": ""}),
        );
        assert_eq!((&p["chosen"], &p["resolved"]), (&json!([]), &json!([])));
    }

    /// A missing base is the configured default: with `[session] base =
    /// "head"`, "latest" is a change and "head" is not.
    #[test]
    fn a_missing_base_is_the_configured_default() {
        let mut m = json!({"base": "latest", "changed": []})
            .as_object()
            .unwrap()
            .clone();
        apply_provenance(&mut m, &json!({}), "head");
        assert_eq!(m["resolved"], json!(["base"]));
        let mut m = json!({"base": "HEAD", "changed": []})
            .as_object()
            .unwrap()
            .clone();
        apply_provenance(&mut m, &json!({}), "head");
        assert_eq!((&m["chosen"], &m["resolved"]), (&json!([]), &json!([])));
    }

    /// A base the overlord leaves out stays as the user sent it (with its
    /// side), and stays theirs; "" is the default.
    #[test]
    fn a_base_left_out_is_kept() {
        let prev = json!({"chosen": ["base"], "base": "feat", "prefer": "local"});
        let mut m = Map::new();
        keep_base(&mut m, &json!({"description": "d"}), &prev);
        assert_eq!(
            (&m["base"], &m["prefer"]),
            (&json!("feat"), &json!("local"))
        );
        apply_provenance(&mut m, &prev, "latest");
        assert_eq!(m["chosen"], json!(["base"]));
        let mut m = Map::new();
        keep_base(&mut m, &json!({"base": ""}), &prev);
        assert!(m.get("base").is_none());
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
