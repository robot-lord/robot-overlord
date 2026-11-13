//! Which Codex threads on Codex's shared background server are rlord's:
//! the gate `rlord hook emit codex` reads when a hook runs without
//! `RLORD_SESSION_ID`.
//!
//! A daemon-mode session's hooks (`codex::on_daemon`) run in Codex's daemon,
//! not in the session's pane, so they carry no `RLORD_SESSION_ID` and no
//! `TMUX_PANE` of their own (the daemon's environment is whatever started
//! it). They are told apart by thread id instead. The same hook runs for
//! every Codex thread on the machine, rlord's or not, so the client checks
//! this small file first and goes no further for a thread that isn't
//! listed: one read, no connection, nothing spooled.
//!
//! The daemon rewrites it (`write`) whenever its daemon-mode sessions
//! change: the bound thread of each live one, and the folder of each still
//! waiting for its thread (a `SessionStart` from there may be its first).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

/// `<data>/codex-daemon-threads`.
pub fn path() -> PathBuf {
    crate::paths::data_dir().join("codex-daemon-threads")
}

/// The gate's contents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gate {
    /// Bound thread ids of live daemon-mode sessions.
    pub threads: BTreeSet<String>,
    /// Folders where a daemon-mode session waits for its first thread.
    pub folders: BTreeSet<String>,
}

impl Gate {
    /// One entry per line: `thread <id>` or `folder <path>`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for t in &self.threads {
            out.push_str(&format!("thread {t}\n"));
        }
        for f in &self.folders {
            out.push_str(&format!("folder {f}\n"));
        }
        out
    }

    pub fn parse(text: &str) -> Gate {
        let mut g = Gate::default();
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("thread", id)) if !id.trim().is_empty() => {
                    g.threads.insert(id.trim().to_string());
                }
                Some(("folder", p)) if !p.trim().is_empty() => {
                    g.folders.insert(p.trim_end_matches('\n').to_string());
                }
                _ => {}
            }
        }
        g
    }

    pub fn is_empty(&self) -> bool {
        self.threads.is_empty() && self.folders.is_empty()
    }

    /// Whether a hook event (its slimmed payload) may be rlord's: its
    /// thread is listed, or it is a `SessionStart` in a listed folder.
    pub fn admits(&self, event: &str, payload: &Value) -> bool {
        if payload["session_id"]
            .as_str()
            .is_some_and(|t| self.threads.contains(t))
        {
            return true;
        }
        event == "SessionStart"
            && payload["cwd"].as_str().is_some_and(|cwd| {
                self.folders.contains(cwd)
                    || std::fs::canonicalize(cwd)
                        .ok()
                        .and_then(|p| p.to_str().map(|p| self.folders.contains(p)))
                        .unwrap_or(false)
            })
    }
}

/// The gate as written (None: there is none, or it can't be read).
/// Bounded: a file larger than any real gate is ignored.
pub fn read_from(file: &Path) -> Option<Gate> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(file)
        .ok()?
        .take(256 * 1024)
        .read_to_string(&mut text)
        .ok()?;
    Some(Gate::parse(&text))
}

/// Write the gate (whole, replacing the old one in one step). An empty
/// gate removes the file.
pub fn write_to(file: &Path, gate: &Gate) -> Result<()> {
    if gate.is_empty() {
        match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => return Ok(()),
        }
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, gate.render())?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_listed_threads_and_waiting_folders_pass() {
        let mut g = Gate::default();
        g.threads.insert("t-1".into());
        g.folders.insert("/src/app".into());
        let back = Gate::parse(&g.render());
        assert_eq!(back, g);
        assert!(g.admits("Stop", &json!({"session_id": "t-1"})));
        // Someone else's Codex thread: not ours.
        assert!(!g.admits("Stop", &json!({"session_id": "t-2", "cwd": "/src/app"})));
        // A first SessionStart in a waiting folder may be ours…
        assert!(g.admits(
            "SessionStart",
            &json!({"session_id": "t-3", "cwd": "/src/app"})
        ));
        // …any other event from there isn't.
        assert!(!g.admits(
            "UserPromptSubmit",
            &json!({"session_id": "t-3", "cwd": "/src/app"})
        ));
        assert!(!g.admits("SessionStart", &json!({"cwd": "/src/web"})));
        assert!(!g.admits("Stop", &json!({})));
    }

    #[test]
    fn written_whole_and_removed_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sub/codex-daemon-threads");
        assert_eq!(read_from(&f), None);
        let mut g = Gate::default();
        g.threads.insert("t-1".into());
        write_to(&f, &g).unwrap();
        assert_eq!(read_from(&f), Some(g));
        write_to(&f, &Gate::default()).unwrap();
        assert!(!f.exists());
        // (Already gone: fine.)
        write_to(&f, &Gate::default()).unwrap();
    }
}
