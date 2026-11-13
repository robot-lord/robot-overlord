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

/// `RLORD_TMUX_PROFILE` as the `rlord` that opened the list had it
/// (`tmux.profile_mode`): `default` or `imported`, for that run. None:
/// not set. A testing override: it wins over each session's own choice.
#[cfg(not(test))]
static OVERRIDE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

// Per test thread under test: a test that sets it can't leak it into
// another running alongside.
#[cfg(test)]
thread_local! {
    static OVERRIDE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn override_mode() -> Option<String> {
    #[cfg(not(test))]
    return OVERRIDE.lock().expect("profile override").clone();
    #[cfg(test)]
    return OVERRIDE.with(|o| o.borrow().clone());
}

/// Set the override; whether it changed.
fn set_override(env: Option<String>) -> bool {
    #[cfg(not(test))]
    let mut o = OVERRIDE.lock().expect("profile override");
    #[cfg(test)]
    return OVERRIDE.with(|o| o.replace(env.clone()) != env);
    #[cfg(not(test))]
    {
        let changed = *o != env;
        *o = env;
        changed
    }
}

/// Which tmux setup agent sessions get, and why: (`imported` uses the
/// user's profile; `default` is rlord's own everywhere, as a new user sees
/// it). Without `RLORD_TMUX_PROFILE`, each session's own choice still
/// decides for it (`wanted`); with it, the override wins over those.
pub fn mode(cfg: &Config) -> (&'static str, &'static str) {
    match override_mode().as_deref() {
        Some("default") => ("default", "RLORD_TMUX_PROFILE"),
        Some(_) if tmux_profile::exists() => ("imported", "RLORD_TMUX_PROFILE"),
        _ if !tmux_profile::exists() => ("default", "no profile imported"),
        _ if cfg.tmux.profile => ("imported", "[tmux] profile = true"),
        _ => ("default", "[tmux] profile = false"),
    }
}

fn wanted(db: &Db, cfg: &Config, s: &Session) -> Result<bool> {
    if overlord::is_overlord_role(&s.role) || !tmux_profile::exists() {
        return Ok(false);
    }
    match override_mode().as_deref() {
        Some("default") => Ok(false),
        Some(_) => Ok(true),
        None => Ok(choice(db, s)?.unwrap_or(cfg.tmux.profile)),
    }
}

/// `tmux.profile_mode {env?}`: the `rlord` opening the list reports its
/// `RLORD_TMUX_PROFILE` (absent: none). A change re-applies every session.
/// Without `set`, only reports.
pub fn profile_mode(d: &Daemon, params: &Value) -> Result<Value> {
    if params.get("set") == Some(&Value::Bool(true)) {
        let env = params["env"]
            .as_str()
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty());
        if let Some(v) = &env {
            if v != "default" && v != "imported" {
                anyhow::bail!("RLORD_TMUX_PROFILE={v}: use default or imported");
            }
        }
        if set_override(env) {
            sync_all(d, false);
        }
    }
    let (mode, why) = mode(&d.config());
    Ok(json!({"mode": mode, "why": why, "imported": tmux_profile::exists()}))
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
    if overlord::is_overlord_role(&s.role) {
        tmux::set_profile_keys(name, mode(cfg).0 == "imported")?;
        tmux::name_agent_window(name, &s.name)?;
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

/// `⌥d list · ⌥o overlord`: the configured keys rlord actually bound.
fn hints(cfg: &Config) -> String {
    [
        (&cfg.tmux.back, "home", "list"),
        (&cfg.tmux.overlord, "overlord", "overlord"),
    ]
    .into_iter()
    .filter(|(k, _, _)| !k.trim().is_empty() && tmux::nav_key_bound(k))
    // Each a status-line range: a click does what the key does
    // (`tmux::bind_nav_keys`).
    .map(|(k, verb, what)| {
        let hint = format!("{} {what}", tmux::key_label(k));
        match tmux::hint_range(verb) {
            Some(r) => format!("#[range=user|{r}]{hint}#[norange]"),
            None => hint,
        }
    })
    .collect::<Vec<_>>()
    .join(" · ")
}

pub fn sync_ref(d: &Daemon, r: &str) {
    let db = d.db();
    let res = db
        .resolve_ref(r)
        .and_then(|s| sync(&db, &d.config(), &s, false));
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
        if let Err(e) = sync(&db, &d.config(), &s, refresh) {
            tracing::warn!(session = %s.id, error = %e, "applying tmux profile");
        }
    }
}

/// `tmux.profile {ref, set?: on|off|default|toggle}` → the session's state.
pub fn profile(d: &Daemon, params: &Value) -> Result<Value> {
    let db = d.db();
    let s = db.resolve_ref(methods::p_str(params, "ref")?)?;
    if let Some(set) = params.get("set").and_then(Value::as_str) {
        if overlord::is_overlord_role(&s.role) {
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
                if wanted(&db, &d.config(), &s)? {
                    "off"
                } else {
                    "on"
                }
            }
            other => anyhow::bail!("unknown setting '{other}' (on, off, default, toggle)"),
        };
        db.kv_set(&kv_key(&s.id), v)?;
        sync(&db, &d.config(), &s, false)?;
    }
    Ok(json!({
        "name": s.name,
        "on": wanted(&db, &d.config(), &s)?,
        "explicit": choice(&db, &s)?.is_some(),
        "default": d.config().tmux.profile,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `RLORD_TMUX_PROFILE=default` wins for that run, whatever the
    /// config; unset, the config decides again; anything else is refused.
    #[test]
    fn the_env_override_picks_the_default_setup() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let set = |env: Value| profile_mode(&d, &json!({"set": true, "env": env}));
        let v = set(json!("default")).unwrap();
        assert_eq!(
            (v["mode"].as_str(), v["why"].as_str()),
            (Some("default"), Some("RLORD_TMUX_PROFILE"))
        );
        assert!(set(json!("mine")).is_err());
        let v = set(Value::Null).unwrap();
        assert_ne!(v["why"], "RLORD_TMUX_PROFILE");
        // Without `set`: only reports.
        set(json!("default")).unwrap();
        let v = profile_mode(&d, &json!({})).unwrap();
        assert_eq!(v["mode"], "default");
        set(Value::Null).unwrap();
    }
}
