//! Running other programs (git, gh, cargo, …), and scratch folders.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    let status = cmd
        .status()
        .with_context(|| format!("couldn't start {}", describe(cmd)))?;
    if !status.success() {
        bail!("{} failed ({status})", describe(cmd));
    }
    Ok(())
}

/// Run without output; true if it succeeded.
pub fn succeeds(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Run and return what it printed (trimmed); an error if it fails.
pub fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("couldn't start {}", describe(cmd)))?;
    if !out.status.success() {
        bail!("{} failed ({})", describe(cmd), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Like `output`, with errors hidden: `None` if it fails.
pub fn output_quiet(cmd: &mut Command) -> Option<String> {
    let out = cmd.stderr(Stdio::null()).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
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
