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
/// (A first-time mastermind on a big model can take a while.)
const READY_TIMEOUT: Duration = Duration::from_secs(90);

pub fn ask(d: &Daemon, params: &Value) -> Result<Value> {
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .context("nothing to send")?
        .to_string();
    send_to(d, &overlord::FRONT, &text)
}

/// Type `text` into a seat's pane (starting the seat if needed). The
/// mastermind starts on first use; the overlord must have been set up.
pub fn send_to(d: &Daemon, seat: &overlord::Seat, text: &str) -> Result<Value> {
    // Not while an idle mastermind is being restarted fresh.
    let _send = (seat.key == overlord::MASTERMIND.key)
        .then(|| d.mastermind_send.lock().expect("mastermind send lock"));
    if d.db().overlord()?.is_none() {
        bail!("no overlord yet: press Option-O to set one up (or N for the manual form)");
    }
    let s = overlord::ensure(&d.db(), &d.config, &json!({"seat": seat.key}))?;
    let pane = s["tmux_session"].as_str().unwrap_or(seat.tmux).to_string();
    d.changed();
    // A fresh or resumed overlord isn't listening until its SessionStart hook
    // flips it out of `starting`.
    let start = Instant::now();
    loop {
        let s = seat.session(&d.db())?.context("overlord vanished")?;
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
/// Launching is the mastermind's job: the form goes straight to it.
pub fn new_session(d: &Daemon, params: &Value) -> Result<Value> {
    let text = format_request(params)?;
    let r = send_to(d, &overlord::MASTERMIND, &text)?;
    // Delivered: it answers any form the mastermind had reopened.
    d.db().kv_set("confirm_pending", "0")?;
    Ok(r)
}

/// `{request, context?}`: the overlord hands a request to the mastermind
/// (verbatim, with what it knew), and the user's overlay switches to the
/// mastermind so they can watch and carry on there.
pub fn hand_off(d: &Daemon, params: &Value) -> Result<Value> {
    let request = params["request"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .context("missing 'request' (the user's words, verbatim)")?;
    let mut text = format!("[rlord hand-off from the overlord]\nrequest:\n{request}");
    if let Some(c) = params["context"]
        .as_str()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        text.push_str(&format!("\ncontext:\n{c}"));
    }
    // Switch the user first, so they watch the mastermind start and pick
    // the request up (a first start can take a while), then deliver it.
    let note = show_mastermind(d);
    send_to(d, &overlord::MASTERMIND, &text)?;
    Ok(json!({"handed_off": true, "note": note}))
}

/// Show the mastermind where the user is: switch their open overlay, or a
/// terminal showing the overlord full screen. Returns a note for the
/// overlord about what the user now sees.
fn show_mastermind(d: &Daemon) -> String {
    let client = d
        .overlay
        .lock()
        .expect("ui mutex")
        .as_ref()
        .map(|(c, _)| c.clone());
    if let Some(client) = client {
        // The overlord's overlay is about to be replaced (killed before its
        // own cleanup): give its window its size back first.
        let _ = rlord_core::tmux::auto_size_window(overlord::FRONT.tmux);
        let p = json!({"seat": overlord::MASTERMIND.key, "show": true, "client": client});
        return match crate::ui::overlord(d, &p) {
            Ok(_) => "the user now sees the mastermind".into(),
            Err(e) => format!(
                "delivered, but the overlay didn't switch ({e:#}): tell the user to press Option-M in the overlay"
            ),
        };
    }
    // Full screen on the overlord: move that terminal to the mastermind.
    let on_front = rlord_core::tmux::list_clients()
        .iter()
        .any(|c| c.session == overlord::FRONT.tmux);
    if on_front {
        if let Ok(Some(m)) = overlord::MASTERMIND.session(&d.db()).and_then(|m| match m {
            Some(m) => Ok(Some(m)),
            None => overlord::ensure(&d.db(), &d.config, &json!({"seat": "mastermind"}))
                .map(|v| v["id"].as_str().map(str::to_string))
                .and_then(|id| match id {
                    Some(id) => d.db().resolve_ref(&id).map(Some),
                    None => Ok(None),
                }),
        }) {
            if crate::ui::goto(d, &json!({"ref": m.id})).is_ok() {
                return "the user now sees the mastermind (full screen)".into();
            }
        }
    }
    "delivered; the user isn't looking at the overlord: tell them the mastermind has it (the list's ↳ mastermind row)".into()
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
    // Hints the overlord resolved in an earlier round (a reopened form),
    // which the user then sent on unchanged.
    let resolved = |k: &str| {
        p.get("resolved")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|c| c == k))
    };
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
        let r = format_request(&json!({"description": "x", "repo": "/r", "model": "opus",
                                        "chosen": ["model"], "resolved": ["repo"]}))
        .unwrap();
        assert!(r.contains("- repo: /r (your earlier pick)\n"), "{r}");
        assert!(r.contains("- model: opus (picked by the user)\n"), "{r}");
        let c = format_request(&json!({"description": "x", "confirmed": true})).unwrap();
        assert!(c.contains("confirmed: yes\n") && c.contains("- repo: (you choose"));
    }
}
