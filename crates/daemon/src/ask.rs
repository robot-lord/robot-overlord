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

/// `{request, repo?, model?, workspace?, tags?: [..]}`
pub fn new_session(d: &Daemon, params: &Value) -> Result<Value> {
    let text = format_request(params)?;
    send(d, &text)
}

pub fn format_request(p: &Value) -> Result<String> {
    let request = p
        .get("request")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .context("the request is empty")?;
    let get = |k: &str| {
        p.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    let tags: Vec<&str> = p
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut m = String::from("[rlord new-session request]\n");
    m += &format!(
        "repo: {}\n",
        get("repo").unwrap_or("(you choose, from the request)")
    );
    m += &format!("model: {}\n", get("model").unwrap_or("(default)"));
    m += &format!(
        "worktree: {}\n",
        yes_no(p.get("workspace").and_then(Value::as_bool).unwrap_or(true))
    );
    m += &format!(
        "tags: {}\n",
        if tags.is_empty() {
            "(you choose)".to_string()
        } else {
            tags.join(", ")
        }
    );
    m += "request:\n";
    m += request;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_message_keeps_the_request_verbatim() {
        let m = format_request(&json!({
            "request": "  fix the login flake\n\nkeep the retry count at 3 ",
            "repo": "/r/web", "tags": ["web", "wip"],
        }))
        .unwrap();
        assert!(m.starts_with("[rlord new-session request]\n"));
        assert!(m.contains("repo: /r/web\n"));
        assert!(m.contains("tags: web, wip\n"));
        assert!(m.ends_with("request:\nfix the login flake\n\nkeep the retry count at 3"));
        assert!(format_request(&json!({"request": "  "})).is_err());
    }
}
