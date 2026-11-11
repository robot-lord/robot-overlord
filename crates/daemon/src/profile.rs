//! Which sessions use the user's imported tmux profile (see
//! `rlord_core::tmux_profile`). Per session: an explicit on/off in the kv
//! table, else `[tmux] profile` from the config. Never the list's home
//! session; the overlord gets only the user's keys (it keeps its own bar).
//! Agent sessions also get rlord's key hints on their status bar.

use anyhow::Result;
use serde_json::{json, Value};

use rlord_core::config::Config;
use rlord_core::db::{Db, SessionQuery};
use rlord_core::types::Session;
use rlord_core::{tmux, tmux_profile};

use crate::{methods, overlord, Daemon};

fn kv_key(id: &str) -> String {
    format!("tmux_profile:{id}")
}

/// The session's explicit choice, if any.
fn choice(db: &Db, s: &Session) -> Result<Option<bool>> {
    Ok(match db.kv_get(&kv_key(&s.id))?.as_deref() {
        Some("on") => Some(true),
        Some("off") => Some(false),
        _ => None,
    })
}

fn wanted(db: &Db, cfg: &Config, s: &Session) -> Result<bool> {
    if s.role == overlord::ROLE || !tmux_profile::exists() {
        return Ok(false);
    }
    Ok(choice(db, s)?.unwrap_or(cfg.tmux.profile))
}

/// Bring one session's tmux in line: its profile, and its window named
/// after the session. `refresh` re-applies a profile that is already on
/// (after a re-import), dropping options the new one lacks.
pub fn sync(db: &Db, cfg: &Config, s: &Session, refresh: bool) -> Result<()> {
    let Some(name) = s.tmux_session.as_deref().filter(|n| tmux::has_session(n)) else {
        return Ok(());
    };
    // The overlord keeps its own bar but gets the user's keys (copy mode,
    // prefix, pane moves), when they have a profile.
    if s.role == overlord::ROLE {
        tmux::set_profile_keys(name, tmux_profile::exists() && cfg.tmux.profile)?;
        return Ok(());
    }
    let want = wanted(db, cfg, s)?;
    let on = tmux::profile_on(name);
    if on && (!want || refresh) {
        tmux::set_profile(name, false)?;
    }
    if want && (!on || refresh) {
        tmux::set_profile(name, true)?;
    }
    // rlord's keys on the session's bar (after the profile: turning it on or
    // off resets the bar).
    tmux::set_hint_bar(name, &hints(cfg), want)?;
    // Last: turning the profile off unsets window options, this included.
    tmux::name_agent_window(name, &s.name)?;
    Ok(())
}

/// `⌥d list · ⌥o overlord`, from the configured keys.
fn hints(cfg: &Config) -> String {
    let mut v = Vec::new();
    if !cfg.tmux.back.trim().is_empty() {
        v.push(format!("{} list", tmux::key_label(&cfg.tmux.back)));
    }
    if !cfg.tmux.overlord.trim().is_empty() {
        v.push(format!("{} overlord", tmux::key_label(&cfg.tmux.overlord)));
    }
    v.join(" · ")
}

pub fn sync_ref(d: &Daemon, r: &str) {
    let db = d.db();
    let res = db
        .resolve_ref(r)
        .and_then(|s| sync(&db, &d.config, &s, false));
    if let Err(e) = res {
        tracing::warn!(session = %r, error = %e, "applying tmux profile");
    }
}

pub fn sync_all(d: &Daemon, refresh: bool) {
    let db = d.db();
    let all = db
        .sessions(&SessionQuery {
            include_overlord: true,
            ..Default::default()
        })
        .unwrap_or_default();
    for s in all {
        if let Err(e) = sync(&db, &d.config, &s, refresh) {
            tracing::warn!(session = %s.id, error = %e, "applying tmux profile");
        }
    }
}

/// `tmux.profile {ref, set?: on|off|default|toggle}` → the session's state.
pub fn profile(d: &Daemon, params: &Value) -> Result<Value> {
    let db = d.db();
    let s = db.resolve_ref(methods::p_str(params, "ref")?)?;
    if let Some(set) = params.get("set").and_then(Value::as_str) {
        if s.role == overlord::ROLE {
            anyhow::bail!("the overlord keeps rlord's tmux");
        }
        if !tmux_profile::exists() && set != "default" && set != "off" {
            anyhow::bail!("no tmux profile imported yet — run `rlord tmux import`");
        }
        let v = match set {
            "on" => "on",
            "off" => "off",
            "default" => "",
            "toggle" => {
                if wanted(&db, &d.config, &s)? {
                    "off"
                } else {
                    "on"
                }
            }
            other => anyhow::bail!("unknown setting '{other}' (on, off, default, toggle)"),
        };
        db.kv_set(&kv_key(&s.id), v)?;
        sync(&db, &d.config, &s, false)?;
    }
    Ok(json!({
        "name": s.name,
        "on": wanted(&db, &d.config, &s)?,
        "explicit": choice(&db, &s)?.is_some(),
        "default": d.config.tmux.profile,
        "imported": tmux_profile::exists(),
    }))
}

/// After an import: reload rlord's tmux config, re-bind rlord's keys (the
/// profile's table starts empty of them), re-apply every session.
pub fn reload(d: &Daemon) -> Result<Value> {
    // (No tmux server yet is fine: it loads the new config when it starts.)
    let _ = tmux::source_conf();
    d.bind_nav_keys();
    sync_all(d, true);
    Ok(json!({"reloaded": true, "imported": tmux_profile::exists()}))
}
