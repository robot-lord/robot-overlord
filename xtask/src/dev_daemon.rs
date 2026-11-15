//! `cargo xtask dev-daemon`: the dev loop. Rebuild, then restart the
//! daemon on the new binary (`cargo test` does not refresh
//! target/debug/rlord, so always build first).
//!
//! If a service manager owns the daemon (the launchd agent from
//! scripts/install-launchd.sh on macOS, the systemd user unit from
//! scripts/install-systemd.sh on Linux), it restarts it there; otherwise it
//! stops the old daemon and starts the new one under nohup.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::build::target_dir;
use crate::cmd::{self, cargo};

const USAGE: &str = "usage: cargo xtask dev-daemon";
const LAUNCHD_LABEL: &str = "robot-overlord.daemon";
const SYSTEMD_UNIT: &str = "rlord.service";

/// An environment variable, unset when empty (as the shell's `${X:-…}`).
fn var(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name).filter(|v| !v.is_empty())
}

/// Mirrors rlord_core::paths::runtime_dir().
pub fn runtime_dir(env: &dyn Fn(&str) -> Option<String>, home: &Path) -> PathBuf {
    match var(env, "XDG_RUNTIME_DIR") {
        Some(dir) => Path::new(&dir).join("robot-overlord"),
        None => var(env, "XDG_DATA_HOME")
            .map_or_else(|| home.join(".local/share"), PathBuf::from)
            .join("robot-overlord/run"),
    }
}

/// Mirrors rlord_daemon::autostart::log_path().
pub fn default_log(env: &dyn Fn(&str) -> Option<String>, home: &Path, macos: bool) -> PathBuf {
    if macos {
        home.join("Library/Logs/rlordd.log")
    } else {
        var(env, "XDG_STATE_HOME")
            .map_or_else(|| home.join(".local/state"), PathBuf::from)
            .join("robot-overlord/rlordd.log")
    }
}

/// A pid file's pid.
pub fn parse_pid(text: &str) -> Option<i32> {
    text.trim().parse().ok().filter(|&p| p > 0)
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn read_pid(pidfile: &Path) -> Option<i32> {
    std::fs::read_to_string(pidfile)
        .ok()
        .and_then(|t| parse_pid(&t))
}

/// The first `program` on PATH, as `command -v` finds it.
fn which(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

pub fn main(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return cmd::usage(USAGE);
    }
    cmd::run(cargo().args(["build", "--workspace"]))?;
    let bin = target_dir()?.join("debug/rlord");

    let env = |k: &str| std::env::var(k).ok();
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let macos = std::env::consts::OS == "macos";
    let pidfile = runtime_dir(&env, &home).join("daemon.pid");
    let mut old = read_pid(&pidfile).filter(|&p| alive(p));
    let default_log = default_log(&env, &home, macos);

    // SAFETY: getuid never fails.
    let launchd = format!("gui/{}/{LAUNCHD_LABEL}", unsafe { libc::getuid() });
    let log = if macos && cmd::succeeds(Command::new("launchctl").args(["print", &launchd]))? {
        cmd::run(Command::new("launchctl").args(["kickstart", "-k", &launchd]))?;
        default_log
    } else if !macos && cmd::on_path("systemctl") && systemd_loaded()? {
        cmd::run(Command::new("systemctl").args(["--user", "restart", SYSTEMD_UNIT]))?;
        default_log
    } else {
        if old.is_none() {
            // No usable pidfile: find a process whose argv is exactly
            // `[.../]rlord daemon` (anchored, so it never matches xtask).
            let found = Command::new("pgrep")
                .args(["-f", "(^|/)rlord daemon$"])
                .stderr(Stdio::null())
                .output()?;
            old = String::from_utf8_lossy(&found.stdout)
                .lines()
                .find_map(parse_pid);
        }
        if let Some(pid) = old {
            // SAFETY: plain SIGTERM to a pid we just found.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            for _ in 0..30 {
                if !alive(pid) {
                    break;
                }
                cmd::sleep(Duration::from_millis(100))?;
            }
            if alive(pid) {
                bail!("old daemon {pid} did not exit");
            }
        }
        // Launch through the PATH symlink when it points at this build, so
        // the paths the daemon embeds in hooks/tmux bindings are the
        // stable ones.
        let mut launch = bin.clone();
        if let Some(linked) = which("rlord") {
            if linked
                .canonicalize()
                .ok()
                .is_some_and(|l| Some(l) == bin.canonicalize().ok())
            {
                launch = linked;
            }
        }
        let log = std::env::var_os("RLORD_LOG")
            .filter(|v| !v.is_empty())
            .map_or(default_log, PathBuf::from);
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .with_context(|| format!("opening {}", log.display()))?;
        // Not waited for: it outlives xtask.
        Command::new("nohup")
            .arg(&launch)
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .spawn()
            .context("couldn't start the daemon")?;
        log
    };

    let Some(new) = wait_new(&pidfile, old)? else {
        eprintln!("daemon failed to start (see {})", log.display());
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        eprintln!("{}", crate::smoke::tail(&text, 3));
        bail!("daemon failed to start");
    };
    cmd::run(Command::new(&bin).arg("ping").stdout(Stdio::null()))?;
    println!("daemon restarted (pid {new}, log: {})", log.display());
    Ok(())
}

fn systemd_loaded() -> Result<bool> {
    let out = Command::new("systemctl")
        .args(["--user", "show", SYSTEMD_UNIT, "-p", "LoadState", "--value"])
        .stderr(Stdio::null())
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim() == "loaded")
}

/// Wait (up to 5s) for the pidfile to name a live daemon other than `old`.
fn wait_new(pidfile: &Path, old: Option<i32>) -> Result<Option<i32>> {
    for _ in 0..50 {
        if let Some(new) = read_pid(pidfile) {
            if Some(new) != old && alive(new) {
                return Ok(Some(new));
            }
        }
        cmd::sleep(Duration::from_millis(100))?;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn finds_the_runtime_dir() {
        let home = Path::new("/home/u");
        assert_eq!(
            runtime_dir(&env_of(&[("XDG_RUNTIME_DIR", "/run/user/1")]), home),
            Path::new("/run/user/1/robot-overlord")
        );
        assert_eq!(
            runtime_dir(&env_of(&[("XDG_DATA_HOME", "/data")]), home),
            Path::new("/data/robot-overlord/run")
        );
        // Empty counts as unset.
        assert_eq!(
            runtime_dir(
                &env_of(&[("XDG_RUNTIME_DIR", ""), ("XDG_DATA_HOME", "")]),
                home
            ),
            Path::new("/home/u/.local/share/robot-overlord/run")
        );
    }

    #[test]
    fn finds_the_log() {
        let home = Path::new("/home/u");
        let none = env_of(&[]);
        assert_eq!(
            default_log(&none, home, true),
            Path::new("/home/u/Library/Logs/rlordd.log")
        );
        assert_eq!(
            default_log(&none, home, false),
            Path::new("/home/u/.local/state/robot-overlord/rlordd.log")
        );
        assert_eq!(
            default_log(&env_of(&[("XDG_STATE_HOME", "/s")]), home, false),
            Path::new("/s/robot-overlord/rlordd.log")
        );
    }

    #[test]
    fn reads_pids() {
        assert_eq!(parse_pid("1234\n"), Some(1234));
        assert_eq!(parse_pid(""), None);
        assert_eq!(parse_pid("0"), None);
        assert_eq!(parse_pid("-1"), None);
        assert_eq!(parse_pid("12ab"), None);
    }
}
