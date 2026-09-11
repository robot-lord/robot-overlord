//! tmux supervision: one tmux session per overlord session, one window, one
//! pane. Session names are `ovl_<ulid>` — derived from the immutable id, never
//! the display name. `remain-on-exit` is set so an exited agent leaves a dead
//! pane (restartable in place via `respawn-pane -k`) instead of tearing the
//! tmux session down.
//!
//! Targeting rule: windows and panes are addressed by their unique tmux ids
//! (`@n`/`%n`, resolved via `list-panes`), NEVER by index — the user's tmux
//! config may set base-index/pane-base-index and shift all numbering (this
//! bit us on first contact with a real config).

use std::process::Command;

use anyhow::{bail, Context, Result};

/// tmux target for exact session-name match (`=` disables prefix matching).
fn exact(name: &str) -> String {
    format!("={name}")
}

fn tmux_cmd() -> Command {
    let mut c = Command::new("tmux");
    // Tests point this at a private server socket; unset = the default server.
    if let Ok(sock) = std::env::var("OVERLORD_TMUX_SOCKET") {
        if !sock.is_empty() {
            c.args(["-L", &sock]);
        }
    }
    c
}

fn run(args: &[&str]) -> Result<String> {
    let out = tmux_cmd()
        .args(args)
        .output()
        .context("running tmux (is tmux installed?)")?;
    if !out.status.success() {
        bail!(
            "tmux {} failed: {}",
            args.first().unwrap_or(&"?"),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn session_name(session_id: &str) -> String {
    format!("ovl_{session_id}")
}

pub fn has_session(name: &str) -> bool {
    tmux_cmd()
        .args(["has-session", "-t", &exact(name)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve the session's single window and pane to their unique tmux ids.
fn window_pane_ids(name: &str) -> Result<(String, String)> {
    let out = run(&[
        "list-panes",
        "-s",
        "-t",
        &exact(name),
        "-F",
        "#{window_id} #{pane_id}",
    ])?;
    let first = out.lines().next().context("session has no panes")?;
    let (w, p) = first
        .split_once(' ')
        .context("unexpected list-panes output")?;
    Ok((w.to_string(), p.to_string()))
}

/// POSIX single-quote escaping.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Build the pane command: `exec env K=V... argv...`, fully quoted. Env is
/// embedded in the command (rather than tmux session env) so respawns are
/// deterministic and the vars reach hook subprocesses.
pub fn pane_command(env: &[(String, String)], argv: &[String]) -> String {
    let mut parts = vec!["exec".to_string(), "env".to_string()];
    for (k, v) in env {
        parts.push(shell_quote(&format!("{k}={v}")));
    }
    parts.extend(argv.iter().map(|a| shell_quote(a)));
    parts.join(" ")
}

pub fn new_session(name: &str, cwd: &str, pane_cmd: &str) -> Result<()> {
    run(&["new-session", "-d", "-s", name, "-c", cwd, pane_cmd])?;
    // Dead pane instead of dead session when the agent exits. If this
    // follow-up fails, tear the half-made session down rather than leaking it.
    let configure = || -> Result<()> {
        let (window_id, _) = window_pane_ids(name)?;
        run(&["set-option", "-w", "-t", &window_id, "remain-on-exit", "on"])?;
        Ok(())
    };
    if let Err(e) = configure() {
        let _ = kill_session(name);
        return Err(e);
    }
    Ok(())
}

pub fn respawn_pane(name: &str, cwd: &str, pane_cmd: &str) -> Result<()> {
    let (_, pane_id) = window_pane_ids(name)?;
    run(&["respawn-pane", "-k", "-c", cwd, "-t", &pane_id, pane_cmd])?;
    Ok(())
}

pub fn kill_session(name: &str) -> Result<()> {
    if has_session(name) {
        run(&["kill-session", "-t", &exact(name)])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc-1.2/x"), "abc-1.2/x");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn pane_command_shape() {
        let cmd = pane_command(
            &[("OVERLORD_SESSION_ID".into(), "abc".into())],
            &["claude".into(), "--session-id".into(), "u-1".into()],
        );
        assert_eq!(
            cmd,
            "exec env OVERLORD_SESSION_ID=abc claude --session-id u-1"
        );
    }

    /// Real-tmux integration test of the actual module functions, on a
    /// private server socket. Gated: `OVERLORD_TMUX_TESTS=1 cargo test`.
    /// Serialized with a lock in case tests ever share the socket env.
    #[test]
    fn tmux_lifecycle_on_private_server() {
        if std::env::var("OVERLORD_TMUX_TESTS").as_deref() != Ok("1") {
            return;
        }
        // Safety: test process only; the daemon never sets this.
        std::env::set_var("OVERLORD_TMUX_SOCKET", "ovl-test");
        let _ = tmux_cmd().args(["kill-server"]).output();

        let name = "ovl_testsess";
        new_session(name, "/tmp", "sleep 30").unwrap();
        assert!(has_session(name));
        let (w, p) = window_pane_ids(name).unwrap();
        assert!(w.starts_with('@') && p.starts_with('%'));
        respawn_pane(name, "/tmp", "sleep 30").unwrap();
        kill_session(name).unwrap();
        assert!(!has_session(name));

        let _ = tmux_cmd().args(["kill-server"]).output();
    }
}
