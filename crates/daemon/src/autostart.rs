//! Client-side daemon lifecycle, tmux-style: the first client that finds no
//! daemon starts one in the background, and a daemon running an older build
//! than the client is restarted on the new binary — so upgrading (or
//! rebuilding) is just swapping the binary. If a service manager owns the
//! daemon (the launchd agent from scripts/install-launchd.sh on macOS, the
//! systemd user unit from scripts/install-systemd.sh on Linux), starts and
//! restarts go through it so it stays the owner.

use std::fs::File;
use std::io::IsTerminal;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use rlord_core::paths;

use crate::{build_id, client_call, exe_id, methods};

const START_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// After our spawned daemon bows out (lost the lock), how long to wait for
/// the lock holder to answer: long enough for a racing client's daemon to
/// finish starting, short enough that a wedged one fails fast.
const LOCK_HOLDER_GRACE: Duration = Duration::from_secs(3);

/// Where the daemon logs (shared with the service units and dev-daemon.sh).
/// macOS: `~/Library/Logs/rlordd.log`. Elsewhere:
/// `$XDG_STATE_HOME/robot-overlord/rlordd.log` (default `~/.local/state`).
pub fn log_path() -> PathBuf {
    let env = |v: &str| {
        std::env::var_os(v)
            .filter(|x| !x.is_empty())
            .map(PathBuf::from)
    };
    let path = if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Logs/rlordd.log"))
    } else {
        env("XDG_STATE_HOME")
            .or_else(|| env("HOME").map(|h| h.join(".local/state")))
            .map(|d| d.join("robot-overlord/rlordd.log"))
    };
    path.unwrap_or_else(|| std::env::temp_dir().join("rlordd.log"))
}

/// Make sure a daemon is answering on `socket`: start one if none is
/// running, and restart it if it runs an older build of this same binary
/// (an upgrade or rebuild). A daemon running a *different* binary is left
/// alone: restarting it would just hand the flip-flop to the next client.
pub fn ensure_daemon(socket: &Path) -> Result<()> {
    let Ok(pong) = client_call(socket, "ping", Value::Null) else {
        return start_daemon(socket, None);
    };
    if pong["build"].as_u64() == Some(build_id()) {
        return Ok(());
    }
    match pong["exe"].as_str() {
        // Same binary, different build: upgraded or rebuilt in place.
        // (No "exe" at all: a daemon from before this check; replace it.)
        Some(theirs) if theirs == exe_id() => {}
        None => {}
        Some(theirs) => {
            if std::io::stderr().is_terminal() {
                eprintln!("rlord: note: the daemon runs {theirs}, not this binary");
            }
            return Ok(());
        }
    }
    eprintln!("rlord: daemon is from an older build; restarting it");
    restart_daemon(socket)
}

/// Stop the running daemon (if any) and start a fresh one on this binary.
pub fn restart_daemon(socket: &Path) -> Result<()> {
    // A pidfile left by a crashed daemon may name a pid the OS has since
    // reused: only ever signal a process that is actually `rlord daemon`.
    let old = read_pid().filter(|&pid| is_rlord_daemon(pid));
    let managed = service::pid();
    if managed.is_some() && managed == old {
        // The service manager owns the daemon serving our socket: let it do
        // the restart.
        service::kick(true)?;
        return wait_for_new(socket, old, None);
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

/// Start a daemon — via the service manager if its unit is installed, else
/// detached — and wait until one other than `old` answers.
fn start_daemon(socket: &Path, old: Option<i32>) -> Result<()> {
    if service::installed() {
        service::kick(false)?;
        wait_for_new(socket, old, None)
    } else {
        let mut child = spawn_detached()?;
        wait_for_new(socket, old, Some(&mut child))
    }
}

/// Spawn `rlord daemon` in its own session (survives the terminal, never
/// gets its Ctrl-C), stdio to the log, through the stable binary path so the
/// paths it writes into hooks and tmux bindings survive upgrades.
fn spawn_detached() -> Result<Child> {
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
    // Only polled, never waited on: the daemon outlives us, and dropping the
    // Child doesn't kill it.
    cmd.spawn().context("spawning `rlord daemon`")
}

/// Wait until the pidfile names a daemon other than `old` and it answers
/// (the old one may still answer while it exits). If `child` (the daemon we
/// spawned) exits first, it lost the instance lock: some other daemon holds
/// it, and we give that one only a short grace to answer.
fn wait_for_new(socket: &Path, old: Option<i32>, mut child: Option<&mut Child>) -> Result<()> {
    let mut deadline = Instant::now() + START_TIMEOUT;
    let mut bowed_out = false;
    loop {
        let pid = read_pid();
        if pid.is_some() && pid != old && client_call(socket, "ping", Value::Null).is_ok() {
            return Ok(());
        }
        if !bowed_out
            && child
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(Some(_))))
        {
            bowed_out = true;
            deadline = deadline.min(Instant::now() + LOCK_HOLDER_GRACE);
        }
        if Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if bowed_out {
        bail!(
            "another rlord daemon holds the lock but isn't answering on {} \
             (`rlord reset` restarts it; see {})",
            socket.display(),
            log_path().display()
        );
    }
    bail!(
        "daemon did not start within {START_TIMEOUT:?}; see {}",
        log_path().display()
    )
}

/// Whether `pid` is a live `rlord daemon` process (by its argv).
fn is_rlord_daemon(pid: i32) -> bool {
    let Ok(o) = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    let cmd = String::from_utf8_lossy(&o.stdout);
    let mut argv = cmd.split_whitespace();
    let exe_is_rlord = argv
        .next()
        .is_some_and(|e| Path::new(e).file_name().is_some_and(|n| n == "rlord"));
    exe_is_rlord && argv.next() == Some("daemon")
}

fn read_pid() -> Option<i32> {
    std::fs::read_to_string(paths::pid_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// A client pointed at a non-default data dir (tests, sandboxes) must not
/// route through the service manager: its daemon serves the default socket,
/// and we would wait on one it never serves.
fn data_overridden() -> bool {
    std::env::var_os("XDG_DATA_HOME").is_some_and(|x| !x.is_empty())
}

/// The launchd user agent (scripts/install-launchd.sh).
#[cfg(target_os = "macos")]
mod service {
    use std::process::{Command, Stdio};

    use anyhow::{bail, Result};

    const LABEL: &str = "robot-overlord.daemon";

    fn target() -> String {
        // SAFETY: getuid never fails.
        format!("gui/{}/{LABEL}", unsafe { libc::getuid() })
    }

    /// `launchctl print` output for our agent, if it is loaded.
    fn print() -> Option<String> {
        let o = Command::new("launchctl")
            .args(["print", &target()])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    /// Whether the agent is loaded and serves the socket this client uses.
    /// The agent's daemon runs without XDG overrides, so it serves the
    /// default socket.
    pub fn installed() -> bool {
        let overridden = super::data_overridden()
            || std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|x| !x.is_empty());
        !overridden && print().is_some()
    }

    /// Pid of the daemon launchd is running, if any.
    pub fn pid() -> Option<i32> {
        print()?
            .lines()
            .find_map(|l| l.trim().strip_prefix("pid = ")?.parse().ok())
    }

    /// Start the daemon (`restart`: kill the running one first).
    pub fn kick(restart: bool) -> Result<()> {
        let target = target();
        let mut args = vec!["kickstart"];
        if restart {
            args.push("-k");
        }
        args.push(&target);
        let st = Command::new("launchctl").args(&args).status()?;
        if !st.success() {
            bail!("launchctl {} failed", args.join(" "));
        }
        Ok(())
    }
}

/// The systemd user unit `rlord.service` (scripts/install-systemd.sh), when
/// there is one; otherwise clients spawn the daemon on demand.
#[cfg(not(target_os = "macos"))]
mod service {
    use std::process::{Command, Stdio};

    use anyhow::{bail, Result};

    const UNIT: &str = "rlord.service";

    /// `systemctl --user show` properties of the unit (empty if systemd
    /// isn't there).
    fn show(props: &str) -> Vec<(String, String)> {
        let Ok(o) = Command::new("systemctl")
            .args(["--user", "show", UNIT, "-p", props])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            return Vec::new();
        };
        if !o.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| {
                let (k, v) = l.split_once('=')?;
                Some((k.to_string(), v.to_string()))
            })
            .collect()
    }

    fn prop(props: &[(String, String)], name: &str) -> Option<String> {
        props
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }

    /// Whether the unit is installed and serves the socket this client
    /// uses. The unit's daemon gets the user manager's environment, where
    /// XDG_RUNTIME_DIR is `/run/user/<uid>`; a client with another one, none
    /// (its socket is then under the data dir), or another data dir talks
    /// to a different daemon.
    pub fn installed() -> bool {
        if super::data_overridden() {
            return false;
        }
        // SAFETY: getuid never fails.
        let default_run = format!("/run/user/{}", unsafe { libc::getuid() });
        let run_ok =
            std::env::var("XDG_RUNTIME_DIR").is_ok_and(|v| v.trim_end_matches('/') == default_run);
        if !run_ok {
            return false;
        }
        let p = show("LoadState");
        prop(&p, "LoadState").as_deref() == Some("loaded")
    }

    /// Pid of the daemon systemd is running, if any.
    pub fn pid() -> Option<i32> {
        if !installed() {
            return None;
        }
        let p = show("MainPID");
        prop(&p, "MainPID")?.parse().ok().filter(|&n: &i32| n > 0)
    }

    /// Start the daemon (`restart`: restart the running one).
    pub fn kick(restart: bool) -> Result<()> {
        let verb = if restart { "restart" } else { "start" };
        let st = Command::new("systemctl")
            .args(["--user", verb, UNIT])
            .stdin(Stdio::null())
            .status()?;
        if !st.success() {
            bail!("systemctl --user {verb} {UNIT} failed");
        }
        Ok(())
    }
}

/// Exclusive daemon lock, held for the daemon's lifetime. Returns `None` if
/// another daemon holds it, so concurrent starts end with exactly one daemon.
/// It lives in the data dir, next to what it protects (the DB and hook
/// spool), not the runtime dir: two daemons with different runtime dirs
/// would otherwise both get "their" lock and share one state.db.
pub fn acquire_instance_lock() -> Result<Option<File>> {
    let dir = paths::data_dir();
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
