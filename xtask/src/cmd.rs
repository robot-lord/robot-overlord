//! Running other programs (git, gh, cargo, …), and scratch folders.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// A wrong command line: reported like any error, but exits with 2.
#[derive(Debug)]
pub struct Usage(pub String);

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Usage {}

pub fn usage<T>(msg: impl Into<String>) -> Result<T> {
    Err(Usage(msg.into()).into())
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    // Only an atomic store: anything more isn't safe in a signal handler.
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Make Ctrl-C (and SIGHUP) stop xtask by unwinding, not by killing it, so the `Drop`
/// guards run (scratch folders removed, manifests put back) as the
/// scripts' `trap … EXIT` did. The child running at the time gets the
/// SIGINT itself and exits; `run`, `output` and `sleep` then return
/// `Interrupted`, and no new command starts.
pub fn on_interrupt() {
    let handler = on_sigint as extern "C" fn(libc::c_int);
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        // Not SIGTERM: it reaches only xtask, which would then wait for the
        // running child (a whole `gh run watch`) before stopping. SIGINT
        // and SIGHUP reach the child too.
        for signal in [libc::SIGINT, libc::SIGHUP] {
            // A signal ignored on entry (nohup) stays ignored, for xtask
            // and the commands it starts.
            if libc::signal(signal, handler as libc::sighandler_t) == libc::SIG_IGN {
                libc::signal(signal, libc::SIG_IGN);
            }
        }
    }
}

/// Ctrl-C was pressed: exits 130, and is never reported as a failed step.
#[derive(Debug)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

pub fn is_interrupted(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Interrupted>().is_some()
}

pub fn check_interrupted() -> Result<()> {
    if INTERRUPTED.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    Ok(())
}

/// Wait, unless Ctrl-C is pressed.
pub fn sleep(d: Duration) -> Result<()> {
    let until = Instant::now() + d;
    while Instant::now() < until {
        check_interrupted()?;
        std::thread::sleep(Duration::from_millis(100));
    }
    check_interrupted()
}

fn describe(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args().take(3) {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}

/// Run with the terminal attached; an error if it fails.
pub fn run(cmd: &mut Command) -> Result<()> {
    check_interrupted()?;
    let status = cmd
        .status()
        .with_context(|| format!("couldn't start {}", describe(cmd)))?;
    if !status.success() {
        // A step that finished is never "interrupted"; one that failed
        // under Ctrl-C is.
        check_interrupted()?;
        bail!("{} failed ({status})", describe(cmd));
    }
    Ok(())
}

/// Run without output; its exit status. Ctrl-C before it, or during a run
/// that then failed, is `Interrupted`, never a status to act on.
pub fn status_quiet(cmd: &mut Command) -> Result<ExitStatus> {
    check_interrupted()?;
    let status = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("couldn't start {}", describe(cmd)))?;
    if !status.success() {
        check_interrupted()?;
    }
    Ok(status)
}

/// Run without output; whether it succeeded (see `status_quiet`).
pub fn succeeds(cmd: &mut Command) -> Result<bool> {
    Ok(status_quiet(cmd)?.success())
}

/// Run and return what it printed (trimmed); an error if it fails.
pub fn output(cmd: &mut Command) -> Result<String> {
    check_interrupted()?;
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("couldn't start {}", describe(cmd)))?;
    if !out.status.success() {
        check_interrupted()?;
        bail!("{} failed ({})", describe(cmd), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn git() -> Command {
    Command::new("git")
}

pub fn gh() -> Command {
    Command::new("gh")
}

/// cargo, without what `cargo run` set for xtask itself: a nested cargo or
/// a cargo plugin must not take xtask's package for the one it works on.
pub fn cargo() -> Command {
    let mut c = Command::new("cargo");
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy().into_owned();
        if k.starts_with("CARGO_PKG_") || k.starts_with("CARGO_MANIFEST_") {
            c.env_remove(k);
        }
    }
    c
}

/// Whether a program is on PATH.
pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// A scratch folder, removed when dropped.
pub struct TempDir(PathBuf);

impl TempDir {
    /// `<parent>/<prefix>.<unique>`; `parent` is created if missing.
    pub fn new_in(parent: &Path, prefix: &str) -> Result<Self> {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = parent.join(format!("{prefix}.{}-{nanos}", std::process::id()));
        std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The regular files in a folder, by name, sorted.
pub fn file_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}
