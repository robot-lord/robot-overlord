//! `cargo xtask demo-claude DIR`: put the real Claude Code CLI, on the fake
//! model (`cargo xtask fake-model`), into the demo in DIR
//! (`scripts/demo/demo.sh --real-claude`), so rlord drives a real Claude
//! Code terminal with no model calls.
//!
//! - Installs the pinned Claude Code (`CLAUDE_CODE_VERSION`) with npm into
//!   `target/demo-claude/<version>` once; later demos reuse it.
//! - Starts the fake model in the background on a free 127.0.0.1 port
//!   (log `DIR/fake-model.log`, pid `DIR/fake-model.pid`; a fake model
//!   already running for DIR is stopped first). `demo.sh --stop` stops it.
//! - Writes `DIR/bin/claude`, which rlord launches as usual: it drops
//!   every `ANTHROPIC_*` / `CLAUDE*` variable it inherits (a demo started
//!   from inside a Claude Code session would otherwise leak that session's
//!   into it), then sets the safety env (`wrapper`) and execs the pinned
//!   CLI.
//! - Seeds the scratch config (`DIR/claude`) so Claude Code starts straight
//!   into its prompt (`seed_claude_json`, `seed_settings`).
//!
//! Safety: every API request goes to the fake model (ANTHROPIC_BASE_URL),
//! with a dummy API key, a scratch config dir and HOME, so the user's
//! login and ~/.claude are never read. HTTP(S)_PROXY also point at the
//! fake model, which refuses and logs (BLOCKED) anything Claude Code tries
//! to send elsewhere; NO_PROXY keeps 127.0.0.1 direct.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::cmd;
use crate::fake_model::DUMMY_KEY;

/// The Claude Code the demo runs, from npm (`@anthropic-ai/claude-code`).
/// Pinned so the demo's screens and the onboarding flags below hold; bump
/// it on purpose, then check the demo still starts straight into its prompt
/// (a new release can add a first-run dialog, needing another flag).
pub const CLAUDE_CODE_VERSION: &str = "2.1.331";

pub fn main(args: &[String], invoked_from: &Path) -> Result<()> {
    let [dir] = args else {
        return cmd::usage(
            "usage: cargo xtask demo-claude DIR  (a demo made by scripts/demo/demo.sh)",
        );
    };
    let dir = invoked_from.join(dir);
    std::fs::create_dir_all(dir.join("bin"))?;
    let dir = dir
        .canonicalize()
        .with_context(|| dir.display().to_string())?;

    let cli = install_cli()?;
    let port = start_fake_model(&dir)?;
    let home = dir.join("home");
    let config = dir.join("claude");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&config)?;

    let wrapper_path = dir.join("bin/claude");
    std::fs::write(&wrapper_path, wrapper(&cli, port, &dir))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper_path, std::fs::Permissions::from_mode(0o755))?;

    let cj = config.join(".claude.json");
    let seeded = seed_claude_json(read_json(&cj)?, &dir, CLAUDE_CODE_VERSION);
    std::fs::write(&cj, serde_json::to_string_pretty(&seeded)?)?;
    let st = config.join("settings.json");
    std::fs::write(
        &st,
        serde_json::to_string_pretty(&seed_settings(read_json(&st)?, port))?,
    )?;

    let said = cmd::output(Command::new(&wrapper_path).arg("--version"))?;
    if !said.contains(CLAUDE_CODE_VERSION) {
        bail!(
            "{} --version said {said:?}, not {CLAUDE_CODE_VERSION}",
            wrapper_path.display()
        );
    }
    println!(
        "real Claude Code {CLAUDE_CODE_VERSION} on the fake model at 127.0.0.1:{port} \
         (log: {})",
        dir.join("fake-model.log").display()
    );
    Ok(())
}

fn read_json(p: &Path) -> Result<Value> {
    match std::fs::read_to_string(p) {
        Ok(s) => serde_json::from_str(&s).with_context(|| p.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| p.display().to_string()),
    }
}

/// The pinned CLI's native binary, installed into the cache if missing.
fn install_cli() -> Result<PathBuf> {
    let cache = PathBuf::from("target/demo-claude")
        .join(CLAUDE_CODE_VERSION)
        .canonicalize()
        .or_else(|_| -> Result<PathBuf> {
            let p = PathBuf::from("target/demo-claude").join(CLAUDE_CODE_VERSION);
            std::fs::create_dir_all(&p)?;
            Ok(p.canonicalize()?)
        })?;
    let bin = cache.join("node_modules/@anthropic-ai/claude-code/bin/claude.exe");
    // The npm package's bin is a placeholder until its postinstall copies
    // the native binary over it (hundreds of MB): a small file is not it.
    let native = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.len() > 10_000_000);
    if !native(&bin) {
        if !cmd::on_path("npm") {
            bail!("npm isn't on PATH: needed once to install Claude Code {CLAUDE_CODE_VERSION}");
        }
        println!(
            "installing Claude Code {CLAUDE_CODE_VERSION} into {}",
            cache.display()
        );
        cmd::run(
            Command::new("npm")
                .args(["install", "--prefix"])
                .arg(&cache)
                .args(["--no-audit", "--no-fund", "--loglevel=error"])
                .arg(format!("@anthropic-ai/claude-code@{CLAUDE_CODE_VERSION}")),
        )?;
        if !native(&bin) {
            bail!("npm installed no native Claude Code at {}", bin.display());
        }
    }
    Ok(bin)
}

/// Start `cargo xtask fake-model` (this binary) for `dir` in the
/// background and return its port.
fn start_fake_model(dir: &Path) -> Result<u16> {
    let pid_file = dir.join("fake-model.pid");
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            if is_fake_model(pid) {
                // SAFETY: plain kill(2) of a process we started.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
        }
    }
    let log_path = dir.join("fake-model.log");
    let log = std::fs::File::create(&log_path)?;
    use std::os::unix::process::CommandExt;
    let child = Command::new(std::env::current_exe()?)
        .args(["fake-model", "--port", "0"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Its own process group: a Ctrl-C in the terminal that ran the
        // demo's setup doesn't stop it; `demo.sh --stop` does.
        .process_group(0)
        .spawn()
        .context("start the fake model")?;
    std::fs::write(&pid_file, child.id().to_string())?;
    let start = Instant::now();
    loop {
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        if let Some(port) = listening_port(&text) {
            return Ok(port);
        }
        if start.elapsed() > Duration::from_secs(10) {
            bail!("the fake model didn't start: {}", log_path.display());
        }
        cmd::sleep(Duration::from_millis(100))?;
    }
}

fn is_fake_model(pid: i32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("fake-model"))
}

/// The port in the fake model's `listening on 127.0.0.1:PORT` line.
pub fn listening_port(log: &str) -> Option<u16> {
    log.lines()
        .find_map(|l| l.strip_prefix("listening on 127.0.0.1:"))
        .and_then(|p| p.trim().parse().ok())
}

/// The demo's `claude`: the pinned CLI with the safety env (module doc).
pub fn wrapper(cli: &Path, port: u16, dir: &Path) -> String {
    let url = format!("http://127.0.0.1:{port}");
    let d = dir.display();
    format!(
        r#"#!/bin/sh
# rlord's demo (scripts/demo/demo.sh --real-claude): the real Claude Code
# {CLAUDE_CODE_VERSION} on the fake model (written by `cargo xtask demo-claude`).
# Never a real model: every request goes to 127.0.0.1:{port}.
for v in $(env | awk -F= '/^(ANTHROPIC_|CLAUDE)[A-Za-z0-9_]*=/ {{ print $1 }}'); do
  unset "$v"
done
export ANTHROPIC_BASE_URL={url} ANTHROPIC_API_KEY={DUMMY_KEY}
export CLAUDE_CONFIG_DIR={d}/claude HOME={d}/home
export DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
export DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 DISABLE_GROWTHBOOK=1
export CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL=1 CLAUDE_CODE_MODEL_CATALOG=off
# Anything still sent elsewhere goes through the fake model, which refuses it.
export HTTPS_PROXY={url} https_proxy={url} HTTP_PROXY={url} http_proxy={url}
export ALL_PROXY={url} all_proxy={url}
export NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost
exec {cli} "$@"
"#,
        cli = cli.display()
    )
}

/// The scratch `.claude.json` with what Claude Code
/// `CLAUDE_CODE_VERSION` needs to start straight into its prompt; the rest
/// of `existing` (Claude Code's own state) is kept.
///
/// - `hasCompletedOnboarding`, `lastOnboardingVersion`: no theme picker or
///   login screen. (Onboarding also runs a connectivity check against
///   api.anthropic.com, which would fail behind the proxy.)
/// - `customApiKeyResponses.approved`: the dummy key's last 20 characters,
///   so it is used without the "use this API key?" question.
/// - `projects.<DIR>.hasTrustDialogAccepted`: trusting DIR trusts every
///   folder under it (the demo's sessions, the overlord's folder).
/// - `hasSeenAutoDefaultNudge`: no "make auto mode your default?" offer.
pub fn seed_claude_json(mut existing: Value, dir: &Path, version: &str) -> Value {
    if !existing.is_object() {
        existing = json!({});
    }
    let key_tail = &DUMMY_KEY[DUMMY_KEY.len() - 20..];
    let o = existing.as_object_mut().expect("an object");
    o.insert("hasCompletedOnboarding".into(), json!(true));
    o.insert("lastOnboardingVersion".into(), json!(version));
    o.insert("hasSeenAutoDefaultNudge".into(), json!(true));
    o.entry("theme").or_insert(json!("dark"));
    o.insert(
        "customApiKeyResponses".into(),
        json!({"approved": [key_tail], "rejected": []}),
    );
    let projects = o.entry("projects").or_insert(json!({}));
    if !projects.is_object() {
        *projects = json!({});
    }
    let p = projects[dir.to_string_lossy().as_ref()]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut p = Value::Object(p);
    p["hasTrustDialogAccepted"] = json!(true);
    projects[dir.to_string_lossy().as_ref()] = p;
    existing
}

/// The scratch `settings.json` (rlord adds its hooks to it later):
///
/// - `permissions.defaultMode = "default"`: Claude Code asks before a tool
///   runs (auto mode, its default, would decide for itself);
/// - `env.ANTHROPIC_BASE_URL`: also tells rlord that Remote Control can't
///   work here (it reads this file), as `disableRemoteControl` does.
pub fn seed_settings(mut existing: Value, port: u16) -> Value {
    if !existing.is_object() {
        existing = json!({});
    }
    if !existing["permissions"].is_object() {
        existing["permissions"] = json!({});
    }
    existing["permissions"]["defaultMode"] = json!("default");
    if !existing["env"].is_object() {
        existing["env"] = json!({});
    }
    existing["env"]["ANTHROPIC_BASE_URL"] = json!(format!("http://127.0.0.1:{port}"));
    existing["disableRemoteControl"] = json!(true);
    existing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_port_comes_from_the_listening_line() {
        assert_eq!(
            listening_port("x\nlistening on 127.0.0.1:4567\n"),
            Some(4567)
        );
        assert_eq!(listening_port("starting\n"), None);
    }

    #[test]
    fn the_wrapper_points_everything_at_the_fake_model() {
        let w = wrapper(Path::new("/c/claude.exe"), 4567, Path::new("/d"));
        for want in [
            "ANTHROPIC_BASE_URL=http://127.0.0.1:4567",
            &format!("ANTHROPIC_API_KEY={DUMMY_KEY}"),
            "CLAUDE_CONFIG_DIR=/d/claude HOME=/d/home",
            "DISABLE_AUTOUPDATER=1",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1",
            "DISABLE_TELEMETRY=1",
            "DISABLE_ERROR_REPORTING=1",
            "HTTPS_PROXY=http://127.0.0.1:4567",
            "NO_PROXY=127.0.0.1,localhost",
            "exec /c/claude.exe \"$@\"",
        ] {
            assert!(w.contains(want), "missing {want}:\n{w}");
        }
        // The inherited ANTHROPIC_* / CLAUDE* are dropped before ours are set.
        let unset = w.find("unset \"$v\"").unwrap();
        assert!(unset < w.find("export ANTHROPIC_BASE_URL").unwrap());
    }

    #[test]
    fn the_seed_skips_first_run_and_keeps_claudes_state() {
        let old = json!({"numStartups": 3, "theme": "light",
                         "projects": {"/d": {"allowedTools": ["Bash"]}}});
        let s = seed_claude_json(old, Path::new("/d"), "9.9.9");
        assert_eq!(s["numStartups"], 3);
        assert_eq!(s["theme"], "light");
        assert_eq!(s["hasCompletedOnboarding"], true);
        assert_eq!(s["lastOnboardingVersion"], "9.9.9");
        assert_eq!(s["hasSeenAutoDefaultNudge"], true);
        assert_eq!(s["projects"]["/d"]["hasTrustDialogAccepted"], true);
        assert_eq!(s["projects"]["/d"]["allowedTools"][0], "Bash");
        let tail = s["customApiKeyResponses"]["approved"][0].as_str().unwrap();
        assert_eq!(tail.len(), 20);
        assert!(DUMMY_KEY.ends_with(tail));
        assert_eq!(
            seed_claude_json(json!(null), Path::new("/d"), "1")["theme"],
            "dark"
        );
    }

    #[test]
    fn settings_ask_before_tools_and_keep_hooks() {
        let old = json!({"hooks": {"Stop": []}, "permissions": {"allow": ["Read"]}});
        let s = seed_settings(old, 4567);
        assert_eq!(s["permissions"]["defaultMode"], "default");
        assert_eq!(s["permissions"]["allow"][0], "Read");
        assert!(s["hooks"]["Stop"].is_array());
        assert_eq!(s["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:4567");
        assert_eq!(s["disableRemoteControl"], true);
    }
}
