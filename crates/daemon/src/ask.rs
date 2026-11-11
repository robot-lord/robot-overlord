//! Sending the user's words to the overlord.
//!
//! `overlord.ask` types text into the overlord's pane (as one bracketed
//! paste, then Enter), so anything in rlord — the list's new-session form,
//! `rlord overlord ask`, a script — can hand the overlord a request. Claude
//! Code queues a message typed while it's busy, so a working overlord is
//! fine.
//!
//! `overlord.new_session` is the list's `n` form: a request with a few
//! defaults (repo, model, worktree, tags), formatted as a
//! `[rlord new-session request]` message. The overlord's instructions say
//! what that header means: launch exactly one session for this request.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::tmux;
use rlord_core::types::Status;

use crate::{overlord, Daemon};

/// How long to wait for a just-(re)started overlord to accept input.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

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

fn send(d: &Daemon, text: &str) -> Result<Value> {
    if d.db().overlord()?.is_none() {
        bail!("no overlord yet: press Option-O to set one up (or N for the manual form)");
    }
    let s = overlord::ensure(&d.db(), &d.config, &json!({}))?;
    let pane = s["tmux_session"]
        .as_str()
        .unwrap_or(overlord::TMUX_NAME)
        .to_string();
    d.changed();
    // A fresh or resumed overlord isn't listening until its SessionStart hook
    // flips it out of `starting`.
    let start = Instant::now();
    loop {
        let s = d.db().overlord()?.context("overlord vanished")?;
        if s.status != Status::Starting {
            break;
        }
        if start.elapsed() > READY_TIMEOUT {
            bail!("the overlord is still starting; try again in a moment");
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    tmux::paste(&pane, text)?;
    // Let the paste land before submitting it.
    std::thread::sleep(Duration::from_millis(150));
    tmux::send_keys(&pane, &["Enter"])?;
    Ok(json!({"sent": true}))
}

/// `{description, repo?, model?, workspace?, tags?: [..], chosen?: [..],
/// confirmed?}` from the list's new-session form.
pub fn new_session(d: &Daemon, params: &Value) -> Result<Value> {
    let text = format_request(params)?;
    send(d, &text)
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
    let chosen = |k: &str| {
        p.get("chosen")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|c| c == k))
    };
    let source = |k: &str| {
        if chosen(k) {
            "picked by the user"
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
    m += "hints (optional; the description wins if they disagree):\n";
    m += &match get("repo") {
        Some(r) => format!("- repo: {r} ({})\n", source("repo")),
        None => "- repo: (you choose, from the description)\n".to_string(),
    };
    m += &match get("model") {
        Some(model) => format!("- model: {model} ({})\n", source("model")),
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
        format!("- tags: {} (picked by the user)\n", tags.join(", "))
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
            "chosen": ["repo"],
        }))
        .unwrap();
        assert!(m.starts_with("[rlord new-session request]\nconfirmed: no\n"));
        assert!(m.contains("- repo: /r/web (picked by the user)\n"));
        assert!(m.contains("- model: opus (default)\n"));
        assert!(m.contains("- worktree: yes (default)\n"));
        assert!(m.contains("- tags: web, wip (picked by the user)\n"));
        assert!(m.ends_with("description:\nfix the login flake\n\nkeep the retry count at 3"));
        assert!(format_request(&json!({"description": "  "})).is_err());
        let c = format_request(&json!({"description": "x", "confirmed": true})).unwrap();
        assert!(c.contains("confirmed: yes\n") && c.contains("- repo: (you choose"));
    }
}
