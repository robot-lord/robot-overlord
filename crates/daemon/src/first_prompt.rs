//! A new conversation's first prompt, kept off the agent's command line.
//!
//! A prompt on argv shows in `ps`, in the tmux pane's command, in kv
//! `launched_args:<id>` and the `launched` event, and is bounded by the
//! system's argument limit. So when a start carries the user's words, they
//! go into the session's own 0600 file (`<data>/sessions/<id>/prompt.md`)
//! and reach the agent another way (`Way`, the user's choice per agent):
//!
//! - **Claude Code**: the file on the agent's stdin (`Way::Stdin`), once
//!   live check V1 has shown that an interactive `claude` takes piped
//!   stdin as its first prompt and keeps its terminal. Until then
//!   (`CLAUDE_STDIN` false) its prompt stays on argv (`Way::Argv`).
//! - **Codex**: argv carries only a fixed line pointing at the file
//!   (`Way::Pointer`); Codex reads it as its first step. That line is also
//!   what its conversation is bound by (`bind_prompt`).
//! - **OpenCode**: POSTed to its server once the session exists
//!   (`Way::Server`, `POST /api/session/<id>/prompt`, by
//!   `opencode::apply_first_prompt`). If that can't be done in time, the
//!   prompt is queued and typed in at its first idle turn (`send.rs`), the
//!   last resort.
//!
//! A first prompt without the user's words (the background pointer alone,
//! `context::pointer_only`) is fixed text naming a path: it stays on argv.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use rlord_core::agents::Agent;
use rlord_core::db::Db;

/// Whether Claude takes its first prompt on stdin. Off until live check V1
/// passes (owed: `docs/working/session-context.md`).
pub const CLAUDE_STDIN: bool = false;

/// How an agent's first prompt reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    /// On its command line, as before.
    Argv,
    /// The file on its stdin.
    Stdin,
    /// POSTed to its server.
    Server,
    /// A fixed line on its command line naming the file.
    Pointer,
}

pub fn way(agent: Agent) -> Way {
    match agent {
        Agent::Claude if CLAUDE_STDIN => Way::Stdin,
        Agent::Claude => Way::Argv,
        Agent::Codex => Way::Pointer,
        Agent::OpenCode => Way::Server,
    }
}

/// `<base>/sessions/<id>/prompt.md`.
pub fn file_path(base: &Path, id: &str) -> PathBuf {
    crate::context::session_dir(base, id).join("prompt.md")
}

/// How every `pointer_line` starts.
const POINTER_HEAD: &str = "[rlord session ";

/// The line Codex is started with instead of the prompt (session `id`).
/// Fixed text: only the id and path vary. The id comes first: Codex's
/// thread index may keep only the start of a first message, and binding
/// matches by it (`discover`), so the start must tell sessions apart.
pub fn pointer_line(id: &str, file: &Path) -> String {
    format!(
        "{POINTER_HEAD}{id}{POINTER_TAIL}{}. Read that file and treat its contents as the \
         user's message to you.",
        file.display()
    )
}

/// The user's words a session was launched with (its launch spec).
pub fn launch_words(db: &Db, id: &str) -> Option<String> {
    let raw = db.launch_spec(id).ok()?;
    let spec: serde_json::Value = serde_json::from_str(&raw).ok()?;
    spec["prompt"]
        .as_str()
        .map(str::to_string)
        .filter(|p| !p.trim().is_empty())
}

/// What follows the id in every `pointer_line`.
const POINTER_TAIL: &str = "] The user's first message for this session is in ";

/// A conversation's task as a transcript shows it: its first prompt, or,
/// where there is none or it is only a pointer line (Codex), the launch's
/// own words. A pointer line is session `id`'s own, or (a Codex fork,
/// whose history starts with its parent's) another session's in full:
/// a path is never shown as the task, and the fork's own words stand in.
pub fn task(id: &str, transcript_first: Option<String>, words: Option<String>) -> Option<String> {
    match transcript_first {
        Some(t) if is_pointer(id, &t) => words,
        None => words,
        t => t,
    }
}

fn is_pointer(id: &str, text: &str) -> bool {
    let t = text.trim_start();
    if t.starts_with(&format!("{POINTER_HEAD}{id}]")) {
        return true;
    }
    // Another session's: its id, then the fixed text.
    t.strip_prefix(POINTER_HEAD)
        .and_then(|rest| rest.split_once(']'))
        .is_some_and(|(other, _)| {
            !other.is_empty()
                && other.chars().all(|c| c.is_ascii_alphanumeric())
                && t.contains(&format!("{other}{POINTER_TAIL}"))
        })
}

/// Where a start's first prompt went.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Placed {
    /// What goes on the command line (and what binds the conversation).
    pub argv: Option<String>,
    /// The file to put on the agent's stdin.
    pub stdin: Option<PathBuf>,
    /// The file to POST to the agent's server once the session exists.
    pub server: Option<PathBuf>,
}

/// Place first prompt `typed` (as the agent should get it, background
/// pointer included) of session `id`. `words`: it carries the user's
/// words (else it is fixed text and stays on argv). A file that can't be
/// written leaves the prompt on argv (logged): a start never fails for it.
pub fn place(base: &Path, id: &str, agent: Agent, typed: Option<&str>, words: bool) -> Placed {
    let Some(typed) = typed else {
        return Placed::default();
    };
    let on_argv = || Placed {
        argv: Some(typed.to_string()),
        ..Placed::default()
    };
    let way = way(agent);
    if !words || way == Way::Argv {
        return on_argv();
    }
    let file = match write(base, id, typed) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(session = %id, error = %e, "writing the first prompt's file");
            return on_argv();
        }
    };
    match way {
        Way::Argv => on_argv(),
        Way::Stdin => Placed {
            stdin: Some(file),
            ..Placed::default()
        },
        Way::Server => Placed {
            server: Some(file),
            ..Placed::default()
        },
        Way::Pointer => Placed {
            argv: Some(pointer_line(id, &file)),
            ..Placed::default()
        },
    }
}

/// Write the prompt file (folder 0700, file 0600).
fn write(base: &Path, id: &str, text: &str) -> std::io::Result<PathBuf> {
    let dir = crate::context::session_dir(base, id);
    rlord_core::private::dir_under(base, &dir)?;
    let path = file_path(base, id);
    rlord_core::private::write(&path, text.as_bytes())?;
    Ok(path)
}

/// kv: a first prompt waiting for OpenCode's server, `{"file", "at"}`.
pub fn pending_key(id: &str) -> String {
    format!("first_prompt:{id}")
}

/// Hold (or, with None, clear) the file a start left for the server.
pub fn hold(db: &Db, id: &str, file: Option<&Path>, at: i64) -> Result<()> {
    match file {
        Some(f) => db.kv_set(
            &pending_key(id),
            &json!({"file": f.to_string_lossy(), "at": at}).to_string(),
        ),
        None => db.kv_delete(&pending_key(id)),
    }
}

/// Give a held first prompt (if any) a fresh clock: a restart carries it
/// over rather than dropping it.
pub fn restamp(db: &Db, id: &str, at: i64) -> Result<()> {
    let Some(raw) = db.kv_get(&pending_key(id))? else {
        return Ok(());
    };
    let mut held: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    held["at"] = json!(at);
    // (Running again: no longer held for a stopped session.)
    if let Some(o) = held.as_object_mut() {
        o.remove("stopped");
    }
    db.kv_set(&pending_key(id), &held.to_string())
}

/// Remove what a start that failed left: its prompt file and held POST.
pub fn forget(db: &Db, base: &Path, id: &str) {
    let _ = db.kv_delete(&pending_key(id));
    let _ = std::fs::remove_file(file_path(base, id));
}

/// The pane command with `file` on the agent's stdin (`exec … < file`).
pub fn with_stdin(cmd: String, file: Option<&Path>) -> String {
    match file {
        Some(f) => format!(
            "{cmd} < {}",
            rlord_core::tmux::shell_quote(&f.to_string_lossy())
        ),
        None => cmd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn the_users_words_leave_argv() {
        let b = base();
        let secret = "fix the queue; the password is hunter2";
        let p = place(b.path(), "01a", Agent::Codex, Some(secret), true);
        let line = p.argv.clone().unwrap();
        assert!(!line.contains("hunter2"), "{line}");
        let file = file_path(b.path(), "01a");
        assert!(line.contains(&file.display().to_string()));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), secret);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let p = place(b.path(), "01b", Agent::OpenCode, Some(secret), true);
        assert_eq!(p.argv, None);
        assert_eq!(p.server, Some(file_path(b.path(), "01b")));

        let p = place(b.path(), "01c", Agent::Claude, Some(secret), true);
        if CLAUDE_STDIN {
            assert_eq!((p.argv, p.stdin), (None, Some(file_path(b.path(), "01c"))));
        } else {
            assert_eq!(p.argv.as_deref(), Some(secret));
        }
    }

    #[test]
    fn fixed_text_and_no_prompt_stay_as_they_were() {
        let b = base();
        assert_eq!(
            place(b.path(), "01a", Agent::Codex, None, true),
            Placed::default()
        );
        let p = place(
            b.path(),
            "01a",
            Agent::OpenCode,
            Some("[rlord] read x"),
            false,
        );
        assert_eq!(p.argv.as_deref(), Some("[rlord] read x"));
        assert!(!file_path(b.path(), "01a").exists());
    }

    #[test]
    fn an_unwritable_folder_keeps_the_prompt_on_argv() {
        let b = base();
        // `sessions` is a file: nothing can be made under it.
        std::fs::write(b.path().join("sessions"), "").unwrap();
        let p = place(b.path(), "01a", Agent::Codex, Some("go"), true);
        assert_eq!(p.argv.as_deref(), Some("go"));
    }

    #[test]
    fn the_pointer_starts_with_its_session_and_hides_the_task_from_no_one() {
        let b = base();
        let p = place(b.path(), "01abc", Agent::Codex, Some("do x"), true);
        let line = p.argv.unwrap();
        assert!(line.starts_with("[rlord session 01abc] "), "{line}");
        assert_eq!(
            task("01abc", Some(line.clone()), Some("do x".into())).as_deref(),
            Some("do x")
        );
        assert_eq!(
            task("01abc", Some("own".into()), Some("x".into())).as_deref(),
            Some("own")
        );
        assert_eq!(task("01abc", None, Some("x".into())).as_deref(), Some("x"));
        // A fork's history starts with its parent's pointer: never shown.
        assert_eq!(task("01xyz", Some(line.clone()), None), None);
        assert_eq!(
            task("01xyz", Some(line), Some("fork words".into())).as_deref(),
            Some("fork words")
        );
        // Text that only looks like the start of one is the user's own.
        let own = "[rlord session note] keep this".to_string();
        assert_eq!(task("01xyz", Some(own.clone()), None), Some(own));
    }

    #[test]
    fn stdin_is_redirected_quoted() {
        let cmd = with_stdin("exec claude".into(), Some(Path::new("/d/it's/prompt.md")));
        assert_eq!(cmd, r#"exec claude < '/d/it'\''s/prompt.md'"#);
        assert_eq!(with_stdin("exec claude".into(), None), "exec claude");
    }
}
