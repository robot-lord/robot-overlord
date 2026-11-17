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
//!
//! `overlord.setup_request` is the code-folder form's description, sent as
//! a `[rlord setup request]`: the overlord answers with `confirm_setup`,
//! and the user's confirm on the form saves it (no second round).

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::Agent;
use rlord_core::tmux;
use rlord_core::types::Status;

use crate::send::{type_into, Checks, Typed};
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
    // A fresh or resumed overlord isn't listening until its SessionStart hook
    // flips it out of `starting`. That wait is spent without the send lock
    // (an idle fresh restart or a "continue it" may need it meanwhile); each
    // look is under it, and the paste happens under the same hold as the
    // look that found it ready.
    let start = Instant::now();
    let mut started = false;
    // Starts asked for: the first, and one more if it stopped meanwhile.
    let mut starts = 0;
    loop {
        {
            // Not while an idle overlord is being restarted fresh. (Lock
            // order: this, then the DB.)
            let _send = d.overlord_send.lock().expect("overlord send lock");
            let Some(id) = d.db().overlord()?.map(|s| s.id) else {
                if started {
                    bail!("overlord vanished");
                }
                bail!(
                    "no overlord yet: press {}-O to set one up (or N for the manual form)",
                    rlord_core::keys::alt_name()
                );
            };
            // Its session's lock (locks.rs: after the send lock, before the
            // DB), then a fresh look.
            let _session = d.sessions.lock(&id);
            let s = d.db().overlord()?.context("overlord vanished")?;
            let running = crate::overlord::running(&s);
            let s = if !started || !running {
                // Start it (or pick up fresh flags) once; again only if it
                // stopped while we waited, and not over and over (an agent
                // that exits at once would be respawned until the timeout).
                if starts >= 2 {
                    bail!(
                        "the overlord stopped while starting: look at its pane ({}-O)",
                        rlord_core::keys::alt_name()
                    );
                }
                starts += 1;
                overlord::ensure(&d.db(), &d.config(), &json!({}))?;
                started = true;
                d.changed();
                d.db().overlord()?.context("overlord vanished")?
            } else {
                s
            };
            // Blocked on the user (a permission prompt, a question): the
            // paste's Enter would answer it (OpenCode's dialog takes Enter
            // as "Allow once"). They answer it themselves.
            if s.status == Status::WaitingInput {
                bail!(
                    "the overlord is waiting on you (a permission prompt or a question): \
                     answer it first ({}-O)",
                    rlord_core::keys::alt_name()
                );
            }
            if s.status != Status::Starting {
                let name = s
                    .tmux_session
                    .as_deref()
                    .context("the overlord has no pane")?;
                // Its agent pane (not whichever pane is active); out of
                // copy mode first (left there by the overlay).
                let pane = tmux::pane_id(name)?;
                // Its screen, right before: a dialog takes the Enter as its
                // answer, whatever the status says (a Codex overlord
                // without hooks shows working while it asks). It may be
                // working: Claude and Codex take a message mid-turn.
                let checks = Checks {
                    agent: Agent::named(&s.agent)?,
                    settle: false,
                    attached: None,
                    overlay: None,
                    // (The user's own message: a draft of theirs in the
                    // overlord's pane is theirs to merge.)
                    draft: false,
                    // (And its prompt is theirs: not marked as rlord's.)
                    source: None,
                };
                if let Typed::NotNow(why) = type_into(d, &s.id, &pane, text, &checks)? {
                    bail!(
                        "not sent to the overlord: {why}. Answer it first ({}-O)",
                        rlord_core::keys::alt_name()
                    );
                }
                return Ok(json!({"sent": true}));
            }
        }
        if start.elapsed() > READY_TIMEOUT {
            bail!("the overlord is still starting; try again in a moment");
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// kv key: recent form requests' hints and their provenance, by request id
/// (the `request_id:` line of the message). A reopened form names the request
/// it answers, so its labels come from that request, never another.
pub const PROVENANCE_KEY: &str = "request_provenance";
/// Requests remembered (oldest dropped first).
const PROVENANCE_KEEP: usize = 20;

/// The provenance of form request `id` (Null if unknown).
pub fn provenance_of(d: &Daemon, id: &str) -> Result<Value> {
    Ok(d.db()
        .kv_map(PROVENANCE_KEY)?
        .remove(id.trim())
        .unwrap_or_default())
}

fn remember_provenance(d: &Daemon, id: &str, v: Option<Value>) -> Result<()> {
    let mut m = d.db().kv_map(PROVENANCE_KEY)?;
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
            "workspace": params["workspace"], "tags": params["tags"], "base": params["base"],
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
    // Where a new branch starts. Not sent (an older form): the config's.
    m += &match get("base") {
        Some(b) if b.eq_ignore_ascii_case("latest") => {
            format!("- base: latest default branch ({})\n", source("base"))
        }
        Some(b) if b.eq_ignore_ascii_case("head") => {
            format!("- base: the main checkout's HEAD ({})\n", source("base"))
        }
        Some(b) => format!("- base: {b} ({})\n", source("base")),
        None => "- base: (the configured default)\n".to_string(),
    };
    m += &if tags.is_empty() {
        "- tags: (you choose)\n".to_string()
    } else {
        format!("- tags: {} ({})\n", tags.join(", "), source("tags"))
    };
    m += "description:\n";
    m += description;
    Ok(m)
}

/// `{description, code_dir, code_dir_source, roots, roots_source}` from
/// the code-folder form (`tui/src/setupform.rs`).
pub fn setup_request(d: &Daemon, params: &Value) -> Result<Value> {
    send(d, &format_setup_request(params)?)
}

/// The `[rlord setup request]` message: the form's hints, whose they
/// were, and the description verbatim.
pub fn format_setup_request(p: &Value) -> Result<String> {
    let description = p["description"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .context("the description is empty")?;
    let dir = p["code_dir"].as_str().map(str::trim).unwrap_or("");
    let source = p["code_dir_source"]
        .as_str()
        .filter(|s| {
            matches!(
                *s,
                "suggested by rlord"
                    | "picked by the user"
                    | "current setting"
                    | "proposed by the overlord"
            )
        })
        .unwrap_or("not set");
    let mut m = String::from("[rlord setup request]\n");
    m += "hints (optional; the description wins if they disagree):\n";
    m += &if dir.is_empty() || source == "not set" {
        "- code folder: (not set)\n".to_string()
    } else {
        format!("- code folder: {dir} ({source})\n")
    };
    // Each roots line says how to pass that same choice back to
    // confirm_setup, so a choice the user made survives the round trip.
    m += &match &p["roots"] {
        Value::Array(a) if !a.is_empty() => {
            let list: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            if p["roots_source"] == "config" {
                format!(
                    "- repo finder scans: the code folder and {} (from the user's config; \
                     to keep this: omit repo_roots)\n",
                    list.join(", ")
                )
            } else {
                format!(
                    "- repo finder scans: the code folder and {} (picked by the user; \
                     to keep this: repo_roots: {})\n",
                    list.join(", "),
                    json!(list)
                )
            }
        }
        _ if p["roots_source"] == "default" => {
            "- repo finder scans: the code folder only (rlord's default; \
             to keep this: repo_roots: [])\n"
                .to_string()
        }
        _ => {
            "- repo finder scans: the code folder only (to keep this: repo_roots: [])\n".to_string()
        }
    };
    m += "description:\n";
    m += description;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_request_message() {
        let m = format_setup_request(&json!({
            "description": " mine are in ~/ko and ~/proj ", "code_dir": "/h/ko",
            "code_dir_source": "suggested by rlord", "roots": "follow",
        }))
        .unwrap();
        assert_eq!(
            m,
            "[rlord setup request]\nhints (optional; the description wins if they disagree):\n\
             - code folder: /h/ko (suggested by rlord)\n\
             - repo finder scans: the code folder only (to keep this: repo_roots: [])\n\
             description:\nmine are in ~/ko and ~/proj"
        );
        let m = format_setup_request(&json!({"description": "x", "code_dir": "/h/x",
            "code_dir_source": "picked by the user", "roots": ["~"], "roots_source": "config"}))
        .unwrap();
        assert!(
            m.contains("- code folder: /h/x (picked by the user)\n"),
            "{m}"
        );
        assert!(
            m.contains(
                "- repo finder scans: the code folder and ~ (from the user's config; \
                 to keep this: omit repo_roots)\n"
            ),
            "{m}"
        );
        let m = format_setup_request(&json!({"description": "x", "code_dir": "",
            "roots": ["/a", "/b"], "roots_source": "user"}))
        .unwrap();
        assert!(m.contains("- code folder: (not set)\n"), "{m}");
        assert!(
            m.contains(
                "- repo finder scans: the code folder and /a, /b (picked by the user; \
                 to keep this: repo_roots: [\"/a\",\"/b\"])\n"
            ),
            "{m}"
        );
        let m = format_setup_request(&json!({"description": "x", "roots": "follow",
            "roots_source": "default"}))
        .unwrap();
        assert!(m.contains("the code folder only (rlord's default; "), "{m}");
        assert!(format_setup_request(&json!({"description": " "})).is_err());
    }

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
        // Where its branch starts: the config's unless the user said.
        assert!(c.contains("- base: (the configured default)\n"), "{c}");
        let b = format_request(&json!({"description": "x", "base": "feat",
                                        "chosen": ["base"]}))
        .unwrap();
        assert!(b.contains("- base: feat (picked by the user)\n"), "{b}");
        let b = format_request(&json!({"description": "x", "base": "latest",
                                        "chosen": ["base"]}))
        .unwrap();
        assert!(
            b.contains("- base: latest default branch (picked by the user)\n"),
            "{b}"
        );
    }
}
