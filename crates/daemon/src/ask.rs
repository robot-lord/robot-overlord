//! Sending the user's words to the overlord.
//!
//! `overlord.ask` types text into the overlord's pane (as one bracketed
//! paste, then Enter), so anything in rlord — the list's new-session form,
//! `rlord overlord ask`, a script — can hand the overlord a request. Claude
//! Code queues a message typed while it's busy, so a working overlord is
//! fine.
//!
//! `overlord.new_session` is the list's `n` form: a request with a few
//! defaults (repo, agent, model, worktree, tags), formatted as a
//! `[rlord new-session request]` message. The overlord's instructions say
//! what that header means: launch exactly one session for this request.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use rlord_core::tmux;
use rlord_core::types::Status;

use crate::{overlord, Daemon};

/// How long to wait for a just-(re)started overlord to accept input.
/// (A first start on a big model can take a while.)
const READY_TIMEOUT: Duration = Duration::from_secs(90);

pub fn ask(d: &Daemon, params: &Value) -> Result<Value> {
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .context("nothing to send")?
        .to_string();
    send(d, &text)
}

/// Type `text` into the overlord's pane (starting it if it has stopped).
/// It must have been set up.
pub fn send(d: &Daemon, text: &str) -> Result<Value> {
    // Not while an idle overlord is being restarted fresh.
    let _send = d.overlord_send.lock().expect("overlord send lock");
    if d.db().overlord()?.is_none() {
        bail!(
            "no overlord yet: press {}-O to set one up (or N for the manual form)",
            rlord_core::keys::alt_name()
        );
    }
    let s = overlord::ensure(&d.db(), &d.config(), &json!({}))?;
    let pane = s["tmux_session"]
        .as_str()
        .unwrap_or(overlord::TMUX_NAME)
        .to_string();
    d.changed();
    // A fresh or resumed overlord isn't listening until its SessionStart hook
    // flips it out of `starting`.
    let start = Instant::now();
    loop {
        let s = overlord::session(&d.db())?.context("overlord vanished")?;
        if s.status != Status::Starting {
            break;
        }
        if start.elapsed() > READY_TIMEOUT {
            bail!("the overlord is still starting; try again in a moment");
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    // Out of copy mode first (left there by the overlay): its keys would
    // take the Enter that submits this.
    let _ = tmux::cancel_mode(&pane);
    tmux::paste(&pane, text)?;
    // Let the paste land before submitting it.
    std::thread::sleep(Duration::from_millis(150));
    tmux::send_keys(&pane, &["Enter"])?;
    Ok(json!({"sent": true}))
}

/// kv key: recent form requests' hints and their provenance, by request id
/// (the `request_id:` line of the message). A reopened form names the request
/// it answers, so its labels come from that request, never another.
pub const PROVENANCE_KEY: &str = "request_provenance";
/// Requests remembered (oldest dropped first).
const PROVENANCE_KEEP: usize = 20;

fn provenance_map(d: &Daemon) -> Result<Map<String, Value>> {
    Ok(d.db()
        .kv_get(PROVENANCE_KEY)?
        .and_then(|v| serde_json::from_str::<Value>(&v).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default())
}

/// The provenance of form request `id` (Null if unknown).
pub fn provenance_of(d: &Daemon, id: &str) -> Result<Value> {
    Ok(provenance_map(d)?.remove(id.trim()).unwrap_or_default())
}

fn remember_provenance(d: &Daemon, id: &str, v: Option<Value>) -> Result<()> {
    let mut m = provenance_map(d)?;
    match v {
        Some(v) => {
            m.insert(id.to_string(), v);
        }
        None => {
            m.remove(id);
        }
    }
    while m.len() > PROVENANCE_KEEP {
        let oldest = m
            .iter()
            .min_by_key(|(_, v)| v["at"].as_i64().unwrap_or(0))
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => m.remove(&k),
            None => break,
        };
    }
    d.db().kv_set(PROVENANCE_KEY, &Value::Object(m).to_string())
}

/// Whether `p[list]` is an array holding `k`.
pub fn in_list(p: &Value, list: &str, k: &str) -> bool {
    p.get(list)
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().any(|c| c == k))
}

/// `{description, repo?, agent?, model?, workspace?, tags?: [..], chosen?: [..],
/// confirmed?}` from the list's new-session form.
pub fn new_session(d: &Daemon, params: &Value) -> Result<Value> {
    // Each request gets an id; a form the overlord reopens names it.
    let id = format!("r{}", &rlord_core::types::new_id()[20..]).to_lowercase();
    let mut params = params.clone();
    params["request_id"] = json!(id);
    let text = format_request(&params)?;
    // The hints as sent and whose they were, so a form reopened for this
    // request gets its labels from rlord, not from the model. Written before
    // delivery (the overlord may answer at once).
    remember_provenance(
        d,
        &id,
        Some(json!({
            "at": rlord_core::types::now_ms(),
            "chosen": params["chosen"], "resolved": params["resolved"],
            "repo": params["repo"], "agent": params["agent"], "model": params["model"],
            "workspace": params["workspace"], "tags": params["tags"],
        })),
    )?;
    let r = match send(d, &text) {
        Ok(r) => r,
        Err(e) => {
            // Never delivered: nothing will answer it.
            let _ = remember_provenance(d, &id, None);
            return Err(e);
        }
    };
    // Delivered: it answers any form the overlord had reopened.
    d.db().kv_set("confirm_pending", "0")?;
    Ok(r)
}

/// The `[rlord new-session request]` message. Hints say whether the user
/// picked them or left the default, so the overlord knows what was a choice;
/// `confirmed: yes` means the user already reviewed the overlord's corrected
/// hints (`confirm_new_session`) and it should launch with them as given.
pub fn format_request(p: &Value) -> Result<String> {
    let description = p
        .get("description")
        .or_else(|| p.get("request"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .context("the description is empty")?;
    let get = |k: &str| {
        p.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let chosen = |k: &str| in_list(p, "chosen", k);
    // Hints the overlord resolved in an earlier round (a reopened form),
    // which the user then sent on unchanged.
    let resolved = |k: &str| in_list(p, "resolved", k);
    let source = |k: &str| {
        if chosen(k) {
            "picked by the user"
        } else if resolved(k) {
            "your earlier pick"
        } else {
            "default"
        }
    };
    let tags: Vec<&str> = p
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let confirmed = p.get("confirmed").and_then(Value::as_bool).unwrap_or(false);

    let mut m = String::from("[rlord new-session request]\n");
    m += &format!("confirmed: {}\n", if confirmed { "yes" } else { "no" });
    if let Some(id) = get("request_id") {
        m += &format!("request_id: {id}\n");
    }
    m += "hints (optional; the description wins if they disagree):\n";
    m += &match get("repo") {
        Some(r) => format!("- repo: {r} ({})\n", source("repo")),
        None => "- repo: (you choose, from the description)\n".to_string(),
    };
    // Which coding agent runs it (older forms don't send one: the default).
    if let Some(agent) = get("agent") {
        m += &format!("- agent: {agent} ({})\n", source("agent"));
    }
    m += &match get("model") {
        Some(model) => format!("- model: {model} ({})\n", source("model")),
        // "" is the agent default; say whose choice it was, if anyone's.
        None if p.get("model").is_some_and(Value::is_string)
            && (chosen("model") || resolved("model")) =>
        {
            format!("- model: agent default ({})\n", source("model"))
        }
        None => "- model: (agent default)\n".to_string(),
    };
    let wt = p.get("workspace").and_then(Value::as_bool).unwrap_or(true);
    m += &format!(
        "- worktree: {} ({})\n",
        if wt { "yes" } else { "no" },
        source("worktree")
    );
    m += &if tags.is_empty() {
        "- tags: (you choose)\n".to_string()
    } else {
        format!("- tags: {} ({})\n", tags.join(", "), source("tags"))
    };
    m += "description:\n";
    m += description;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_message_keeps_the_description_verbatim() {
        let m = format_request(&json!({
            "description": "  fix the login flake\n\nkeep the retry count at 3 ",
            "repo": "/r/web", "model": "opus", "tags": ["web", "wip"],
            "chosen": ["repo", "tags"],
        }))
        .unwrap();
        assert!(m.starts_with("[rlord new-session request]\nconfirmed: no\n"));
        assert!(m.contains("- repo: /r/web (picked by the user)\n"));
        assert!(m.contains("- model: opus (default)\n"));
        assert!(m.contains("- worktree: yes (default)\n"));
        assert!(m.contains("- tags: web, wip (picked by the user)\n"));
        assert!(m.ends_with("description:\nfix the login flake\n\nkeep the retry count at 3"));
        assert!(format_request(&json!({"description": "  "})).is_err());
        let r = format_request(&json!({"description": "x", "request_id": "rab12"})).unwrap();
        assert!(r.contains("confirmed: no\nrequest_id: rab12\n"), "{r}");
        let r = format_request(&json!({"description": "x", "repo": "/r", "model": "opus",
                                        "chosen": ["model"], "resolved": ["repo"]}))
        .unwrap();
        assert!(r.contains("- repo: /r (your earlier pick)\n"), "{r}");
        assert!(r.contains("- model: opus (picked by the user)\n"), "{r}");
        assert!(
            !r.contains("- agent:"),
            "no agent hint sent: none shown\n{r}"
        );
        let r = format_request(&json!({"description": "x", "agent": "codex", "model": "",
                                        "chosen": ["agent"]}))
        .unwrap();
        assert!(
            r.contains("- agent: codex (picked by the user)\n- model: (agent default)\n"),
            "{r}"
        );
        let c = format_request(&json!({"description": "x", "confirmed": true})).unwrap();
        assert!(c.contains("confirmed: yes\n") && c.contains("- repo: (you choose"));
    }
}
