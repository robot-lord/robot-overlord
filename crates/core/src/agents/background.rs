//! Background work a session's agent left running when its turn ended.
//!
//! Claude Code can end a turn while work it started still runs: a `Bash`
//! with `run_in_background` (or one moved to the background after its
//! timeout), an Agent/Task subagent launched async, a skill forked into
//! the background. The user is then really waiting on the agent, not the
//! other way round. This counts that work from the transcript, with plain
//! code only:
//!
//! - **Launch**: the `user` line carrying the tool's result has a
//!   structured `toolUseResult` naming the task: `backgroundTaskId`
//!   (Bash), or `agentId` with `isAsync: true`, `status:
//!   "async_launched"` (Agent) or `background: true` (a forked skill).
//! - **Finish**: a later `user` line whose text holds
//!   `<task-notification>` with `<task-id>` and a final `<status>`
//!   (`completed`, `failed`, `killed`, `stopped`). The same id can notify
//!   more than once (a resumed agent): finishing an id that isn't open
//!   changes nothing.
//!
//! The read is incremental: `Tracker` keeps its byte offset and only
//! reads what was appended since, a bounded chunk at a time, starting
//! from the last `FIRST_READ` bytes the first time it sees a file.
//! Codex and OpenCode have no such work yet: `Tracker::advance` leaves
//! them at zero (the place to add an adapter).

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use super::transcript::Format;

/// The first look at a transcript reads at most its last this many bytes
/// (work launched before that is missed; the stale guard covers it).
pub const FIRST_READ: u64 = 4 << 20;
/// At most this much is read per call; the rest waits for the next.
pub const CHUNK: u64 = 4 << 20;

/// Final `<status>` values of a task notification.
const FINAL: [&str; 4] = ["completed", "failed", "killed", "stopped"];

/// What has been read of one transcript, and the work still open in it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tracker {
    /// Bytes read so far (always at a line's end).
    pub offset: u64,
    /// Task/agent ids launched and not yet finished.
    pub open: BTreeSet<String>,
}

impl Tracker {
    /// Read what was appended to `path` since the last call (bounded by
    /// `CHUNK`). Returns whether `open` changed. A file that shrank (a
    /// rewrite) is read again from its tail.
    pub fn advance(&mut self, path: &Path) -> std::io::Result<bool> {
        // Adapter hook: only Claude Code transcripts are read today.
        if Format::of(path) != Format::Claude {
            let changed = !self.open.is_empty();
            self.open.clear();
            return Ok(changed);
        }
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        let before = self.open.clone();
        let mut skip_partial = false;
        if self.offset > len || (self.offset == 0 && len > FIRST_READ) {
            self.open.clear();
            self.offset = len.saturating_sub(FIRST_READ);
            skip_partial = self.offset > 0;
        }
        if self.offset >= len {
            return Ok(self.open != before);
        }
        f.seek(SeekFrom::Start(self.offset))?;
        let mut r = BufReader::new(f.take(CHUNK));
        let mut buf = Vec::new();
        if skip_partial {
            // Starting mid-file: the first line is cut, skip to its end.
            let n = r.read_until(b'\n', &mut buf)?;
            if buf.last() != Some(&b'\n') {
                return Ok(self.open != before);
            }
            self.offset += n as u64;
        }
        loop {
            buf.clear();
            let n = r.read_until(b'\n', &mut buf)?;
            // Only whole lines: a line still being written waits.
            if n == 0 || buf.last() != Some(&b'\n') {
                break;
            }
            self.offset += n as u64;
            if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
                feed(&mut self.open, &v);
            }
        }
        Ok(self.open != before)
    }
}

/// Apply one transcript line to the set of open background work.
pub fn feed(open: &mut BTreeSet<String>, line: &Value) {
    if line["type"] != "user" {
        return;
    }
    if let Some(id) = launched(&line["toolUseResult"]) {
        open.insert(id.to_string());
    }
    let content = &line["message"]["content"];
    let mut texts: Vec<&str> = Vec::new();
    match content {
        Value::String(s) => texts.push(s),
        Value::Array(blocks) => {
            texts.extend(blocks.iter().filter_map(|b| b["text"].as_str()));
        }
        _ => {}
    }
    for t in texts {
        for (id, status) in notifications(t) {
            if FINAL.contains(&status) {
                open.remove(id);
            }
        }
    }
}

/// The id of background work a tool result says it started.
fn launched(r: &Value) -> Option<&str> {
    if let Some(id) = r["backgroundTaskId"].as_str() {
        return Some(id);
    }
    let id = r["agentId"].as_str()?;
    (r["isAsync"] == true || r["status"] == "async_launched" || r["background"] == true)
        .then_some(id)
}

/// `(task id, status)` of each `<task-notification>` in `text`.
fn notifications(text: &str) -> Vec<(&str, &str)> {
    text.split("<task-notification>")
        .skip(1)
        .filter_map(|n| {
            let n = n.split("</task-notification>").next()?;
            Some((tag(n, "task-id")?, tag(n, "status")?))
        })
        .collect()
}

fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)? + start;
    Some(s[start..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn bash_launch(id: &str) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1",
             "content": format!("Command running in background with ID: {id}.")}]},
            "toolUseResult": {"stdout": "", "stderr": "", "backgroundTaskId": id}})
    }

    fn agent_launch(id: &str) -> Value {
        json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_2",
             "content": [{"type": "text", "text": format!("Async agent launched successfully.\nagentId: {id}")}]}]},
            "toolUseResult": {"isAsync": true, "status": "async_launched", "agentId": id}})
    }

    fn note(id: &str, status: &str) -> Value {
        json!({"type": "user", "origin": {"kind": "task-notification"}, "message": {"role": "user",
            "content": format!("<task-notification>\n<task-id>{id}</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n<status>{status}</status>\n<summary>done</summary>\n</task-notification>")}})
    }

    fn count(lines: &[Value]) -> usize {
        let mut open = BTreeSet::new();
        for l in lines {
            feed(&mut open, l);
        }
        open.len()
    }

    #[test]
    fn launches_minus_finishes() {
        assert_eq!(count(&[bash_launch("b1"), agent_launch("a1")]), 2);
        assert_eq!(
            count(&[
                bash_launch("b1"),
                agent_launch("a1"),
                note("b1", "completed")
            ]),
            1
        );
        for s in FINAL {
            assert_eq!(count(&[agent_launch("a1"), note("a1", s)]), 0, "{s}");
        }
        // Not a final status: still open.
        assert_eq!(count(&[agent_launch("a1"), note("a1", "running")]), 1);
    }

    #[test]
    fn a_repeat_notification_or_an_unknown_id_changes_nothing() {
        let lines = [
            agent_launch("a1"),
            note("a1", "completed"),
            note("a1", "completed"),
            note("zz", "completed"),
            bash_launch("b1"),
        ];
        assert_eq!(count(&lines), 1);
    }

    #[test]
    fn foreground_results_and_other_lines_are_not_launches() {
        let fg = json!({"type": "user", "toolUseResult": {"stdout": "ok", "agentId": "a9", "status": "completed"}});
        let forked = json!({"type": "user", "toolUseResult": {"agentId": "a8", "background": true, "status": "forked"}});
        // Notification text in an assistant line (quoted) doesn't count.
        let quoted = json!({"type": "assistant", "message": {"content": [{"type": "text",
            "text": "<task-notification><task-id>a8</task-id><status>completed</status></task-notification>"}]}});
        assert_eq!(count(std::slice::from_ref(&fg)), 0);
        assert_eq!(count(&[fg, forked, quoted]), 1);
    }

    fn write(f: &mut std::fs::File, lines: &[Value]) {
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn reads_incrementally_and_waits_for_whole_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        write(&mut f, &[agent_launch("a1"), bash_launch("b1")]);
        let mut t = Tracker::default();
        assert!(t.advance(&p).unwrap());
        assert_eq!(t.open.len(), 2);
        assert!(!t.advance(&p).unwrap());
        // Half a line: not read yet.
        let n = note("a1", "completed").to_string();
        write!(f, "{}", &n[..10]).unwrap();
        assert!(!t.advance(&p).unwrap());
        writeln!(f, "{}", &n[10..]).unwrap();
        assert!(t.advance(&p).unwrap());
        assert_eq!(t.open.iter().collect::<Vec<_>>(), ["b1"]);
    }

    #[test]
    fn a_large_file_is_read_from_its_tail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        // An old launch, then more than FIRST_READ of filler.
        write(&mut f, &[agent_launch("old")]);
        let filler = json!({"type": "assistant", "pad": "x".repeat(1 << 16)});
        for _ in 0..(FIRST_READ / (1 << 16) + 2) {
            write(&mut f, std::slice::from_ref(&filler));
        }
        write(&mut f, &[bash_launch("new")]);
        let mut t = Tracker::default();
        while t.offset < std::fs::metadata(&p).unwrap().len() {
            t.advance(&p).unwrap();
        }
        assert_eq!(t.open.iter().collect::<Vec<_>>(), ["new"]);
    }

    #[test]
    fn other_agents_have_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rollout-2026-01-01T00-00-00-x.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        write(&mut f, &[bash_launch("b1")]);
        let mut t = Tracker::default();
        assert!(!t.advance(&p).unwrap());
        assert!(t.open.is_empty());
    }
}
