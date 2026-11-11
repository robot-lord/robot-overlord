//! Claude Code Remote Control, as rlord sees it from outside the agent:
//! whether this machine can use it, and which running sessions are connected.
//!
//! - **Connected or not** comes from Claude Code's own live-state files,
//!   `<config>/sessions/<pid>.json`: while a session is connected its file
//!   carries `bridgeSessionId`, and the session is at
//!   `https://claude.ai/code/<bridgeSessionId>`.
//! - **Available or not** comes from `claude auth status --json` plus the
//!   settings and environment that rule it out: it needs a claude.ai login on
//!   a paid plan against Anthropic's own API (no API key, Bedrock, Vertex,
//!   Foundry or custom `ANTHROPIC_BASE_URL`), and no `disableRemoteControl`.
//!   A Team or Enterprise org also needs an admin to allow it, which can't be
//!   seen from here: such a session simply never connects.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::claude::config_dir;

/// A running Claude Code process, from its live-state file.
#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub pid: u32,
    /// Claude's own word for it: `idle`, `busy`, `waiting`, …
    pub status: String,
    /// Set while Remote Control is connected.
    pub bridge: Option<String>,
    /// When the process started (ms since the epoch; 0 if unknown).
    pub started_at: i64,
    /// When `status` last changed (ms; 0 if unknown).
    pub status_at: i64,
}

impl Live {
    pub fn url(&self) -> Option<String> {
        self.bridge.as_deref().map(url)
    }
}

pub fn url(bridge: &str) -> String {
    format!("https://claude.ai/code/{bridge}")
}

/// Live state of every running Claude Code process with a state file, by
/// conversation id. `alive(pid)` drops stale files (a crashed process)
/// first; of two live files claiming one conversation, the newest wins.
pub fn live_sessions(alive: impl Fn(u32) -> bool) -> HashMap<String, Live> {
    live_sessions_in(&config_dir().join("sessions"), alive)
}

pub fn live_sessions_in(dir: &Path, alive: impl Fn(u32) -> bool) -> HashMap<String, Live> {
    let mut out: HashMap<String, (i64, Live)> = HashMap::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return HashMap::new();
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        // Small files; bounded anyway.
        if e.metadata().is_ok_and(|m| m.len() > 64 * 1024) {
            continue;
        }
        let Some(v) = std::fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        else {
            continue;
        };
        let (Some(sid), Some(pid)) = (v["sessionId"].as_str(), v["pid"].as_u64()) else {
            continue;
        };
        if !alive(pid as u32) {
            continue;
        }
        let at = v["updatedAt"].as_i64().unwrap_or(0);
        let live = Live {
            pid: pid as u32,
            status: v["status"].as_str().unwrap_or("").to_string(),
            bridge: v["bridgeSessionId"]
                .as_str()
                .filter(|b| !b.is_empty())
                .map(str::to_string),
            started_at: v["startedAt"].as_i64().unwrap_or(0),
            status_at: v["statusUpdatedAt"].as_i64().unwrap_or(0),
        };
        match out.get(sid) {
            Some((t, _)) if *t >= at => {}
            _ => {
                out.insert(sid.to_string(), (at, live));
            }
        }
    }
    out.into_iter().map(|(k, (_, l))| (k, l)).collect()
}

/// Whether this machine can use Remote Control.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Availability {
    pub available: bool,
    /// Why not (when unavailable), in a line the user can act on.
    pub reason: Option<String>,
    /// A caveat when available (e.g. an org admin must allow it).
    pub note: Option<String>,
    /// The check itself failed (`claude` didn't answer): worth retrying
    /// soon rather than trusting for long.
    pub transient: bool,
}

impl Availability {
    fn no(reason: impl Into<String>) -> Self {
        Availability {
            available: false,
            reason: Some(reason.into()),
            ..Default::default()
        }
    }
}

/// What rules Remote Control out, besides the login itself.
#[derive(Debug, Default)]
pub struct Blockers {
    /// `ANTHROPIC_BASE_URL` set (environment or settings `env`).
    pub custom_base_url: bool,
    /// `disableRemoteControl` in user or managed settings.
    pub disabled_by: Option<&'static str>,
}

/// Decide from `claude auth status --json` and the blockers.
pub fn assess(auth: &Value, b: &Blockers) -> Availability {
    if let Some(by) = b.disabled_by {
        return Availability::no(format!("turned off by disableRemoteControl in {by}"));
    }
    if auth["loggedIn"] != true {
        return Availability::no(
            "Claude Code isn't logged in to claude.ai (run `claude auth login`)",
        );
    }
    match auth["apiProvider"].as_str() {
        Some("firstParty") | None => {}
        Some(p) => {
            return Availability::no(format!(
                "Claude Code runs on {p}; Remote Control needs a claude.ai login"
            ))
        }
    }
    if auth["authMethod"].as_str() != Some("claude.ai") {
        return Availability::no(
            "Claude Code uses an API key or token, not a claude.ai login (run `claude auth login`)",
        );
    }
    if b.custom_base_url {
        return Availability::no("ANTHROPIC_BASE_URL points Claude Code at another server");
    }
    let plan = auth["subscriptionType"].as_str().unwrap_or("");
    match plan {
        "free" => Availability::no("needs a Pro, Max, Team or Enterprise plan"),
        "team" | "enterprise" => Availability {
            available: true,
            reason: None,
            transient: false,
            note: Some(
                "on a Team or Enterprise plan an admin must allow Remote Control (claude.ai/admin-settings/claude-code)"
                    .into(),
            ),
        },
        _ => Availability {
            available: true,
            ..Default::default()
        },
    }
}

/// Check this machine (runs `claude auth status`, a second or so).
pub fn check() -> Availability {
    let auth = match auth_status() {
        Ok(v) => v,
        Err(e) => {
            return Availability {
                transient: true,
                ..Availability::no(e)
            }
        }
    };
    assess(&auth, &blockers())
}

fn blockers() -> Blockers {
    let set = |v: &Value| v.as_str().is_some_and(|s| !s.trim().is_empty());
    let user = read_json(&config_dir().join("settings.json"));
    let managed = managed_settings_paths()
        .into_iter()
        .map(|p| read_json(&p))
        .find(|v| !v.is_null())
        .unwrap_or_default();
    let disabled_by = if managed["disableRemoteControl"] == true {
        Some("your organization's managed settings")
    } else if user["disableRemoteControl"] == true {
        Some("~/.claude/settings.json")
    } else {
        None
    };
    Blockers {
        custom_base_url: std::env::var("ANTHROPIC_BASE_URL").is_ok_and(|v| !v.trim().is_empty())
            || set(&user["env"]["ANTHROPIC_BASE_URL"])
            || set(&managed["env"]["ANTHROPIC_BASE_URL"]),
        disabled_by,
    }
}

fn managed_settings_paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json"),
        PathBuf::from("/etc/claude-code/managed-settings.json"),
    ]
}

fn read_json(p: &Path) -> Value {
    std::fs::read(p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// `claude auth status --json`, bounded to a few seconds.
fn auth_status() -> std::result::Result<Value, String> {
    let mut child = std::process::Command::new("claude")
        .args(["auth", "status", "--json"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't run claude: {e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("`claude auth status` didn't answer".into());
            }
        }
    }
    let mut out = String::new();
    use std::io::Read;
    if let Some(mut so) = child.stdout.take() {
        let _ = so.read_to_string(&mut out);
    }
    serde_json::from_str(&out).map_err(|_| "couldn't read `claude auth status`".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn availability_from_auth_and_settings() {
        let max = json!({"loggedIn": true, "authMethod": "claude.ai",
                         "apiProvider": "firstParty", "subscriptionType": "max"});
        let ok = assess(&max, &Blockers::default());
        assert!(ok.available && ok.note.is_none());
        let team = json!({"loggedIn": true, "authMethod": "claude.ai",
                          "apiProvider": "firstParty", "subscriptionType": "team"});
        assert!(assess(&team, &Blockers::default()).note.is_some());
        let key = json!({"loggedIn": true, "authMethod": "api_key", "apiProvider": "firstParty"});
        assert!(!assess(&key, &Blockers::default()).available);
        let bedrock =
            json!({"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "bedrock"});
        assert!(assess(&bedrock, &Blockers::default())
            .reason
            .unwrap()
            .contains("bedrock"));
        let proxy = Blockers {
            custom_base_url: true,
            ..Default::default()
        };
        assert!(!assess(&max, &proxy).available);
        let off = Blockers {
            disabled_by: Some("x"),
            ..Default::default()
        };
        assert!(!assess(&max, &off).available);
        assert!(!assess(&json!({"loggedIn": false}), &Blockers::default()).available);
    }

    #[test]
    fn live_state_files() {
        let d = tempfile::tempdir().unwrap();
        let w = |n: &str, v: Value| std::fs::write(d.path().join(n), v.to_string()).unwrap();
        w(
            "1.json",
            json!({"pid": 1, "sessionId": "a", "status": "idle",
                           "bridgeSessionId": "session_x", "updatedAt": 5}),
        );
        // An older file for the same conversation loses.
        w(
            "2.json",
            json!({"pid": 2, "sessionId": "a", "status": "busy", "updatedAt": 1}),
        );
        w(
            "3.json",
            json!({"pid": 3, "sessionId": "b", "status": "idle"}),
        );
        w("junk.json", json!("nope"));
        let l = live_sessions_in(d.path(), |_| true);
        assert_eq!(l.len(), 2);
        assert_eq!(l["a"].pid, 1);
        assert_eq!(l["a"].url().unwrap(), "https://claude.ai/code/session_x");
        assert_eq!(l["b"].bridge, None);
        // A dead process's file never hides the live one.
        let l = live_sessions_in(d.path(), |pid| pid != 1);
        assert_eq!(l["a"].pid, 2);
    }
}
