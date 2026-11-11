//! Client-side daemon lifecycle, tmux-style: the first client that finds no
//! daemon starts one in the background, and a daemon running an older build
//! than the client is restarted on the new binary — so upgrading (or
//! rebuilding) is just swapping the binary. If the launchd agent
//! (scripts/install-launchd.sh) is installed, starts and restarts go through
//! launchd so it stays the owner.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use rlord_core::paths;

use crate::{build_id, client_call, methods};

const LAUNCHD_LABEL: &str = "robot-overlord.daemon";
const START_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// Where the daemon logs (shared with the launchd agent and dev-daemon.sh).
pub fn log_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library/Logs/rlordd.log"))
        .unwrap_or_else(|| std::env::temp_dir().join("rlordd.log"))
}

/// Make sure a daemon of this binary's build is answering on `socket`:
/// start one if none is running, restart one from an older build.
pub fn ensure_daemon(socket: &Path) -> Result<()> {
    match client_call(socket, "ping", Value::Null) {
        Ok(pong) if pong["build"].as_u64() == Some(build_id()) => Ok(()),
        Ok(_) => {
            eprintln!("rlord: daemon is from an older build; restarting it");
            restart_daemon(socket)
        }
        Err(_) => start_daemon(socket, None),
    }
}

/// Stop the running daemon (if any) and start a fresh one on this binary.
pub fn restart_daemon(socket: &Path) -> Result<()> {
    let old = read_pid();
    let launchd = launchd_pid();
    if launchd.is_some() && launchd == old {
        // launchd owns the daemon serving our socket: let it do the restart.
        kickstart(true)?;
        return wait_for_new(socket, old);
    }
    if let Some(pid) = old {
        // Wait for the process itself to exit, not just its socket: it holds
        // the instance lock until then, and a successor would bow out.
        // SAFETY: plain syscalls, no memory involved.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = Instant::now() + STOP_TIMEOUT;
        while unsafe { libc::kill(pid, 0) } == 0 {
            if Instant::now() > deadline {
                bail!("old daemon (pid {pid}) did not exit within {STOP_TIMEOUT:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    start_daemon(socket, old)
}

/// Start a daemon — via launchd if its agent is installed, else detached —
/// and wait until one other than `old` answers.
fn start_daemon(socket: &Path, old: Option<i32>) -> Result<()> {
    if launchd_job_loaded() {
        kickstart(false)?;
    } else {
        spawn_detached()?;
    }
    wait_for_new(socket, old)
}

/// Spawn `rlord daemon` in its own session (survives the terminal, never
/// gets its Ctrl-C), stdio to the log, through the stable binary path so the
/// paths it writes into hooks and tmux bindings survive upgrades.
fn spawn_detached() -> Result<()> {
    let log = log_path();
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("opening {}", log.display()))?;
    let mut cmd = Command::new(methods::rlord_bin());
    cmd.arg("daemon")
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        // A client running inside an agent pane must not leak that pane's
        // identity into the daemon (and from there into every pane it opens).
        .env_remove("RLORD_SESSION_ID")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    // SAFETY: setsid is async-signal-safe; nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // Not waited on: the daemon outlives us. Dropping the Child doesn't kill it.
    cmd.spawn().context("spawning `rlord daemon`")?;
    Ok(())
}

/// Wait until the pidfile names a daemon other than `old` and it answers
/// (the old one may still answer while it exits).
fn wait_for_new(socket: &Path, old: Option<i32>) -> Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        let pid = read_pid();
        if pid.is_some() && pid != old && client_call(socket, "ping", Value::Null).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "daemon did not start within {START_TIMEOUT:?}; see {}",
        log_path().display()
    )
}

fn read_pid() -> Option<i32> {
    std::fs::read_to_string(paths::pid_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn launchd_target() -> String {
    // SAFETY: getuid never fails.
    format!("gui/{}/{LAUNCHD_LABEL}", unsafe { libc::getuid() })
}

/// `launchctl print` output for our agent, if it is loaded.
fn launchd_print() -> Option<String> {
    let o = Command::new("launchctl")
        .args(["print", &launchd_target()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The launchd agent's daemon runs without XDG overrides, so it serves the
/// default socket. A client pointed elsewhere (tests, sandboxes) must not
/// route through it, or it would wait on a socket launchd never serves.
fn launchd_job_loaded() -> bool {
    let overridden = ["XDG_RUNTIME_DIR", "XDG_DATA_HOME"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()));
    !overridden && launchd_print().is_some()
}

/// Pid of the daemon launchd is running, if any.
fn launchd_pid() -> Option<i32> {
    launchd_print()?
        .lines()
        .find_map(|l| l.trim().strip_prefix("pid = ")?.parse().ok())
}

fn kickstart(kill: bool) -> Result<()> {
    let target = launchd_target();
    let mut args = vec!["kickstart"];
    if kill {
        args.push("-k");
    }
    args.push(&target);
    let st = Command::new("launchctl").args(&args).status()?;
    if !st.success() {
        bail!("launchctl {} failed", args.join(" "));
    }
    Ok(())
}

/// Exclusive per-user daemon lock, held for the daemon's lifetime. Returns
/// `None` if another daemon holds it, so concurrent starts end with exactly
/// one daemon (and never two draining the hook spool).
pub fn acquire_instance_lock() -> Result<Option<File>> {
    let dir = paths::runtime_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("daemon.lock");
    let f = File::create(&path).with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: plain syscall on a valid fd we own.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(e).with_context(|| format!("locking {}", path.display()));
    }
    Ok(Some(f))
}
