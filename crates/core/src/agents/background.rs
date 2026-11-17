//! Background work a session's agent left running when its turn ended.
//!
//! Claude Code can end a turn while work it started still runs: an
//! Agent/Task subagent launched async, a skill forked into the background,
//! a `Bash` with `run_in_background` (or one moved to the background after
//! its timeout). This tracks that work from the transcript, with plain
//! code only:
//!
//! - **Launch**: the `user` line carrying the tool's result has a
//!   structured `toolUseResult` naming the task: `agentId` with `isAsync:
//!   true`, `status: "async_launched"` (Agent) or `background: true` (a
//!   forked skill): `Kind::Agent`; `backgroundTaskId` (Bash):
//!   `Kind::Shell`. Only agents mean the user is waiting on the session:
//!   a background shell is often a dev server or a watcher that never
//!   ends, so it is shown as information, not as a status.
//! - **Finish**: a later line written by the harness (never text a user
//!   typed) holding `<task-notification>` with `<task-id>` and a final
//!   `<status>` (`completed`, `failed`, `killed`, `stopped`). It comes in
//!   two shapes: most often an `attachment` line (`attachment: {type:
//!   "queued_command", commandMode: "task-notification", prompt}`), or a
//!   `user` line with `origin.kind: "task-notification"`
//!   (`message.content`). (The `queue-operation` lines that also carry it
//!   have no origin, so they aren't trusted; the attachment covers them.) The same id can notify more than once (a resumed agent):
//!   finishing an id that isn't open changes nothing. A successful
//!   `SendMessage` that resumes an agent (`toolUseResult: {success: true,
//!   resumedAgentId | revivedAgentId}`) opens it again. A successful
//!   `TaskStop` (or the older `KillShell`) finishes it too: its
//!   `toolUseResult` is `{message: "Successfully stopped task: …",
//!   task_id}` (`shell_id` for `KillShell`).
//!
//! The read is incremental: `Tracker` keeps its byte offset (and the
//! file's inode) and only reads what was appended since, a bounded chunk
//! at a time, starting from the last `FIRST_READ` bytes the first time it
//! sees a file. A line longer than `CHUNK` is skipped unparsed. Codex and
//! OpenCode have no such work yet: `Tracker::advance` leaves them at zero
//! (the place to add an adapter).
//!
//! (Not `transcript_stats`' cursor: that one reads all that is unread in
//! one go and keeps usage totals; this one needs a bounded read and its
//! own state. They share the same reset rule: new inode or shorter file.)

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::transcript::{clip, parse_iso_ms, Format};

/// The first look at a transcript reads at most its last this many bytes
/// (work launched before that is missed; the stale guard covers it).
pub const FIRST_READ: u64 = 4 << 20;
/// At most this much is read per call; the rest waits for the next.
pub const CHUNK: u64 = 4 << 20;

/// Final `<status>` values of a task notification.
const FINAL: [&str; 4] = ["completed", "failed", "killed", "stopped"];

/// Tool calls whose description is remembered until their result comes.
const PENDING_CAP: usize = 64;

/// What a piece of background work is.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An async subagent or a forked skill: the session's work goes on
    /// in it, so the user waits.
    #[default]
    Agent,
    /// A background shell command: often never ends (a dev server).
    Shell,
}

/// One piece of background work still running.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Task {
    /// What it is: the tool call's description (or the tool's name).
    pub label: String,
    /// When it was launched (ms; 0 when the line had no time).
    pub started_at: i64,
    pub kind: Kind,
    /// Where it writes as it goes (`toolUseResult.outputFile`), for
    /// freshness: a subagent writes there, not in the main transcript.
    pub output: Option<PathBuf>,
    /// Its sidechain transcripts' folder (`<transcript>/subagents`), from
    /// the transcript it was launched in: kept when a `/clear` carries it
    /// over to another file.
    pub side: Option<PathBuf>,
}

/// What has been read of one transcript, and the work still open in it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tracker {
    /// Bytes read so far (at a line's end, or inside a line being skipped).
    pub offset: u64,
    /// The file's inode when read: another file at the path starts over.
    pub ino: u64,
    /// Task/agent ids launched and not yet finished.
    pub open: BTreeMap<String, Task>,
    /// Tool calls not yet answered: `tool_use_id → description`, for
    /// the label of a launch (a Bash result doesn't repeat it).
    pending: HashMap<String, String>,
    /// The bytes at `offset` are the rest of a line not to be parsed (cut
    /// by the first read's start, or over `CHUNK` long).
    skip_line: bool,
    /// The sidechain folder of the transcript being read, for launches.
    side: Option<PathBuf>,
}

impl Tracker {
    /// Read what was appended to `path` since the last call (bounded by
    /// `CHUNK`). A file that shrank or was
    /// replaced (new inode) is read again from its tail. (Tests; the
    /// daemon has the metadata already: `advance_with`.)
    #[cfg(test)]
    pub fn advance(&mut self, path: &Path) -> std::io::Result<()> {
        let meta = std::fs::metadata(path)?;
        self.advance_with(path, &meta)
    }

    /// `advance` with the file's metadata already in hand (one stat per
    /// tick): nothing is opened when nothing was appended.
    pub fn advance_with(&mut self, path: &Path, meta: &std::fs::Metadata) -> std::io::Result<()> {
        // Adapter hook: only Claude Code transcripts are read today.
        let (len, ino) = (meta.len(), meta.ino());
        if Format::of(path) != Format::Claude {
            self.open.clear();
            (self.offset, self.ino) = (len, ino);
            return Ok(());
        }
        if self.ino == ino && self.offset == len {
            return Ok(());
        }
        if self.ino != ino || self.offset > len {
            // Another file (a `/clear` moves the session to a new one, in
            // the same process) or a rewrite: read it afresh, but the open
            // work carries over. A new process is what ends it (the
            // daemon drops work launched before the agent's last start).
            let open = std::mem::take(&mut self.open);
            *self = Tracker {
                ino,
                open,
                ..Default::default()
            };
        }
        self.side = Some(path.with_extension("").join("subagents"));
        let mut f = std::fs::File::open(path)?;
        if self.offset == 0 && len > FIRST_READ {
            self.offset = len - FIRST_READ;
            // Starting mid-file: the first line is cut, unless the byte
            // before the start ends a line.
            f.seek(SeekFrom::Start(self.offset - 1))?;
            let mut b = [0u8; 1];
            f.read_exact(&mut b)?;
            self.skip_line = b[0] != b'\n';
        }
        if self.offset >= len {
            return Ok(());
        }
        f.seek(SeekFrom::Start(self.offset))?;
        let mut r = BufReader::new(f.take(CHUNK));
        let mut buf = Vec::new();
        let mut read = 0u64;
        loop {
            buf.clear();
            let n = r.read_until(b'\n', &mut buf)? as u64;
            if n == 0 {
                break;
            }
            let whole = buf.last() == Some(&b'\n');
            if self.skip_line {
                // Drop the rest of the line, a chunk at a time.
                self.offset += n;
                read += n;
                self.skip_line = !whole;
                continue;
            }
            if !whole {
                // A line over `CHUNK` (it filled the whole read): skip it
                // unparsed. Otherwise it is still being written: wait.
                if read == 0 && n == CHUNK {
                    self.offset += n;
                    self.skip_line = true;
                }
                break;
            }
            self.offset += n;
            read += n;
            // (Most lines can't matter: only those are parsed.)
            if worth_parsing(&buf) {
                if let Ok(v) = serde_json::from_slice::<Value>(&buf) {
                    self.feed(&v);
                }
            }
        }
        Ok(())
    }

    /// Apply one transcript line to the open background work.
    pub fn feed(&mut self, line: &Value) {
        let content = &line["message"]["content"];
        // Notifications the harness queued for the agent.
        if line["type"] == "attachment" {
            let a = &line["attachment"];
            if a["type"] == "queued_command" && a["commandMode"] == "task-notification" {
                finish(&mut self.open, &a["prompt"]);
            }
            return;
        }
        if line["type"] == "assistant" {
            for b in content.as_array().into_iter().flatten() {
                if b["type"] == "tool_use" {
                    // (Bounded: answers that never came are forgotten.)
                    if self.pending.len() >= PENDING_CAP {
                        self.pending.clear();
                    }
                    let label = b["input"]["description"]
                        .as_str()
                        .or(b["name"].as_str())
                        .unwrap_or("task");
                    if let Some(id) = b["id"].as_str() {
                        self.pending.insert(id.to_string(), clip(label, 60));
                    }
                }
            }
            return;
        }
        if line["type"] != "user" {
            return;
        }
        let asked: Option<String> = content
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b["tool_use_id"].as_str())
            .find_map(|id| self.pending.remove(id));
        let r = &line["toolUseResult"];
        if let Some((id, kind)) = launched(r) {
            let label = r["description"]
                .as_str()
                .map(|d| clip(d, 60))
                .or(asked)
                .unwrap_or_else(|| "task".to_string());
            let started_at = line["timestamp"]
                .as_str()
                .and_then(parse_iso_ms)
                .unwrap_or(0);
            let output = r["outputFile"].as_str().map(PathBuf::from);
            self.open.insert(
                id.to_string(),
                Task {
                    label,
                    started_at,
                    kind,
                    output,
                    side: self.side.clone(),
                },
            );
        }
        if line["origin"]["kind"] == "task-notification" {
            finish(&mut self.open, content);
        }
        if let Some(id) = stopped(r, content) {
            self.open.remove(id);
        }
        // An agent resumed by a message: running again.
        let resumed = r["resumedAgentId"]
            .as_str()
            .or(r["revivedAgentId"].as_str());
        if let Some(id) = resumed.filter(|_| r["success"] == true) {
            let started_at = line["timestamp"]
                .as_str()
                .and_then(parse_iso_ms)
                .unwrap_or(0);
            let task = self.open.entry(id.to_string()).or_insert_with(|| Task {
                label: "resumed agent".into(),
                kind: Kind::Agent,
                ..Default::default()
            });
            task.started_at = started_at;
        }
    }

    /// Open work of `kind`.
    pub fn of(&self, kind: Kind) -> impl Iterator<Item = (&String, &Task)> {
        self.open.iter().filter(move |(_, t)| t.kind == kind)
    }
}

/// Close the work whose final notification is in `content`.
fn finish(open: &mut BTreeMap<String, Task>, content: &Value) {
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

/// The id a successful `TaskStop` / `KillShell` result stopped.
fn stopped<'a>(r: &'a Value, content: &Value) -> Option<&'a str> {
    let failed = content
        .as_array()
        .into_iter()
        .flatten()
        .any(|b| b["is_error"] == true);
    let said = r["message"].as_str().is_some_and(|m| {
        m.starts_with("Successfully stopped") || m.starts_with("Successfully killed")
    });
    let id = match (r["task_id"].as_str(), r["shell_id"].as_str()) {
        (Some(id), _) if said => id,
        // KillShell: `success` with its `shell_id` is enough.
        (None, Some(id)) if said || r["success"] == true => id,
        _ => return None,
    };
    (!failed).then_some(id)
}

/// Words one of the lines `Tracker::feed` acts on must hold: a cheap
/// check before the full JSON parse.
const NEEDLES: [&[u8]; 4] = [
    b"toolUseResult",
    b"task-notification",
    b"resumedAgentId",
    br#""type":"tool_use""#,
];

fn worth_parsing(line: &[u8]) -> bool {
    use memchr::memmem::Finder;
    static FINDERS: std::sync::OnceLock<Vec<Finder<'static>>> = std::sync::OnceLock::new();
    FINDERS
        .get_or_init(|| NEEDLES.iter().map(|n| Finder::new(*n)).collect())
        .iter()
        .any(|f| f.find(line).is_some())
}

/// The id and kind of background work a tool result says it started.
fn launched(r: &Value) -> Option<(&str, Kind)> {
    if let Some(id) = r["backgroundTaskId"].as_str() {
        return Some((id, Kind::Shell));
    }
    let id = r["agentId"].as_str()?;
    (r["isAsync"] == true || r["status"] == "async_launched" || r["background"] == true)
        .then_some((id, Kind::Agent))
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
        let mut t = Tracker::default();
        for l in lines {
            t.feed(l);
        }
        t.open.len()
    }

    #[test]
    fn labels_and_start_times() {
        let call = json!({"type": "assistant", "message": {"content": [{"type": "tool_use",
            "id": "toolu_1", "name": "Bash", "input": {"command": "x", "description": "Run tests",
            "run_in_background": true}}]}});
        let mut launch = bash_launch("b1");
        launch["timestamp"] = json!("1970-01-01T00:00:01.000Z");
        let mut t = Tracker::default();
        t.feed(&call);
        t.feed(&launch);
        t.feed(&agent_launch("a1"));
        assert_eq!(
            t.open["b1"],
            Task {
                label: "Run tests".into(),
                started_at: 1000,
                kind: Kind::Shell,
                output: None,
                side: None,
            }
        );
        assert_eq!(t.open["a1"].kind, Kind::Agent);
        // An agent's result names its own description; none here.
        assert_eq!(t.open["a1"].label, "task");
        assert!(t.pending.is_empty());
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
        t.advance(&p).unwrap();
        assert_eq!(t.open.len(), 2);
        t.advance(&p).unwrap();
        // Half a line: not read yet.
        let n = note("a1", "completed").to_string();
        write!(f, "{}", &n[..10]).unwrap();
        t.advance(&p).unwrap();
        assert_eq!(t.open.len(), 2);
        writeln!(f, "{}", &n[10..]).unwrap();
        t.advance(&p).unwrap();
        assert_eq!(t.open.keys().collect::<Vec<_>>(), ["b1"]);
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
        assert_eq!(t.open.keys().collect::<Vec<_>>(), ["new"]);
    }

    #[test]
    fn a_shell_is_shell_kind_and_agents_are_agents() {
        let mut t = Tracker::default();
        t.feed(&bash_launch("b1"));
        t.feed(&agent_launch("a1"));
        let forked = json!({"type": "user", "toolUseResult": {"agentId": "a2", "background": true, "status": "forked"}});
        t.feed(&forked);
        assert_eq!(t.of(Kind::Shell).count(), 1);
        assert_eq!(t.of(Kind::Agent).count(), 2);
    }

    #[test]
    fn a_notification_counts_only_from_the_harness() {
        // The same text typed or pasted by the user (no origin) is not one.
        let mut pasted = note("a1", "completed");
        pasted.as_object_mut().unwrap().remove("origin");
        assert_eq!(count(&[agent_launch("a1"), pasted]), 1);
        assert_eq!(count(&[agent_launch("a1"), note("a1", "completed")]), 0);
    }

    #[test]
    fn a_line_over_the_chunk_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("long.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        write(&mut f, &[agent_launch("a1")]);
        let mut t = Tracker::default();
        t.advance(&p).unwrap();
        // One line longer than CHUNK, then a finish after it.
        let long = json!({"type": "user", "pad": "x".repeat(CHUNK as usize + 10)});
        write(&mut f, &[long, note("a1", "completed")]);
        let len = std::fs::metadata(&p).unwrap().len();
        for _ in 0..5 {
            t.advance(&p).unwrap();
        }
        assert_eq!(t.offset, len);
        assert!(t.open.is_empty());
    }

    #[test]
    fn a_replaced_file_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("r.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        write(&mut f, &[agent_launch("a1"), agent_launch("a2")]);
        let mut t = Tracker::default();
        t.advance(&p).unwrap();
        // Same path, a new file (new inode), longer than the old offset.
        let q = dir.path().join("r2.jsonl");
        let mut g = std::fs::File::create(&q).unwrap();
        write(
            &mut g,
            &[bash_launch("b1"), bash_launch("b2"), bash_launch("b3")],
        );
        std::fs::rename(&q, &p).unwrap();
        t.advance(&p).unwrap();
        // Read from the start; the open work carries over (same process).
        assert_eq!(
            t.open.keys().collect::<Vec<_>>(),
            ["a1", "a2", "b1", "b2", "b3"]
        );
    }

    #[test]
    fn queued_notifications_finish_too() {
        let text = |id: &str| {
            format!("<task-notification>\n<task-id>{id}</task-id>\n<status>completed</status>\n</task-notification>")
        };
        let attachment = json!({"type": "attachment", "attachment": {"type": "queued_command",
            "commandMode": "task-notification", "prompt": text("a1"),
            "origin": {"kind": "task-notification", "producer": "session-task"}}});
        assert_eq!(count(&[agent_launch("a1"), attachment]), 0);
        // A queue-operation has no origin: not trusted (its attachment is).
        let queued =
            json!({"type": "queue-operation", "operation": "enqueue", "content": text("a2")});
        assert_eq!(count(&[agent_launch("a2"), queued]), 1);
        // The same text typed by the user (a plain prompt) closes nothing.
        let typed_user =
            json!({"type": "user", "message": {"role": "user", "content": text("a4")}});
        assert_eq!(count(&[agent_launch("a4"), typed_user]), 1);
        // Another kind of queued command (the user's own prompt) isn't one.
        let typed = json!({"type": "attachment", "attachment": {"type": "queued_command",
            "commandMode": "prompt", "prompt": text("a3")}});
        assert_eq!(count(&[agent_launch("a3"), typed]), 1);
    }

    #[test]
    fn a_resumed_agent_is_open_again() {
        let resumed = json!({"type": "user", "timestamp": "1970-01-01T00:00:05.000Z",
            "toolUseResult": {"success": true, "message": "Resuming agent a1",
            "resumedAgentId": "a1", "revivedAgentId": "a1"}});
        let mut t = Tracker::default();
        for l in [agent_launch("a1"), note("a1", "completed"), resumed] {
            t.feed(&l);
        }
        assert_eq!(t.open["a1"].kind, Kind::Agent);
        assert_eq!(t.open["a1"].started_at, 5000);
        let failed =
            json!({"type": "user", "toolUseResult": {"success": false, "resumedAgentId": "a2"}});
        assert_eq!(count(&[failed]), 0);
    }

    #[test]
    fn a_successful_stop_finishes_it() {
        // As TaskStop answers (Claude Code 2.1.3xx).
        let stop = |id: &str, err: bool| {
            json!({"type": "user", "message": {"content": [{"type": "tool_result",
                "tool_use_id": "toolu_9", "is_error": err,
                "content": format!("{{\"message\":\"Successfully stopped task: {id} (x)\"}}")}]},
                "toolUseResult": {"message": format!("Successfully stopped task: {id} (x)"),
                    "task_id": id, "task_type": "local_agent", "command": "x"}})
        };
        assert_eq!(count(&[agent_launch("a1"), stop("a1", false)]), 0);
        assert_eq!(count(&[agent_launch("a1"), stop("a1", true)]), 1);
        let kill = json!({"type": "user", "toolUseResult": {"success": true,
            "message": "Successfully killed shell: b1 (x)", "shell_id": "b1"}});
        assert_eq!(count(&[bash_launch("b1"), kill]), 0);
        // `success` and a task id alone, without the message: not a stop.
        let bare = json!({"type": "user", "toolUseResult": {"success": true, "task_id": "a1"}});
        assert_eq!(count(&[agent_launch("a1"), bare]), 1);
        // Some other message.
        let other = json!({"type": "user", "toolUseResult": {"message": "Successfully sent", "task_id": "a1"}});
        assert_eq!(count(&[agent_launch("a1"), other]), 1);
    }

    #[test]
    fn the_prefilter_keeps_what_matters() {
        for l in [agent_launch("a"), bash_launch("b"), note("a", "completed")] {
            assert!(worth_parsing(l.to_string().as_bytes()));
        }
        assert!(!worth_parsing(
            br#"{"type":"assistant","message":{"content":"hi"}}"#
        ));
        // The words alone in text don't make a tool call.
        assert!(!worth_parsing(
            br#"{"type":"user","message":{"content":"a tool_use block"}}"#
        ));
        let call = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t"}]}}"#;
        assert!(worth_parsing(call.as_bytes()));
    }

    /// The ids read from a file whose last line, a launch of `b`, is
    /// `FIRST_READ + extra` bytes long (newline included), after a launch of
    /// `a`: the first read starts `extra` bytes into it.
    fn tail_ids(extra: u64) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        write(&mut f, &[agent_launch("a")]);
        let empty = json!({"type": "user", "toolUseResult": {"backgroundTaskId": "b"}, "pad": ""});
        let pad = (FIRST_READ + extra) as usize - empty.to_string().len() - 1;
        let mut last = empty;
        last["pad"] = json!("x".repeat(pad));
        writeln!(f, "{last}").unwrap();
        let mut t = Tracker::default();
        t.advance(&p).unwrap();
        t.open.keys().cloned().collect()
    }

    #[test]
    fn the_first_read_skips_only_a_cut_line() {
        // Starts right after a newline: that line is whole, read it.
        assert_eq!(tail_ids(0), ["b"]);
        // Starts inside it: skipped.
        assert!(tail_ids(1).is_empty());
    }

    #[test]
    fn other_agents_have_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rollout-2026-01-01T00-00-00-x.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        write(&mut f, &[bash_launch("b1")]);
        let mut t = Tracker::default();
        t.advance(&p).unwrap();
        assert!(t.open.is_empty());
    }
}
