//! Searching what sessions said and did: their agent transcripts, on disk.
//!
//! rlord never copies transcripts; it knows where each session's file is
//! (`transcripts.source_path`) and searches them in place with ripgrep's
//! engine (`grep-searcher`, linked in: nothing to install). Two steps, both
//! bounded so a broad search can't flood anyone's context:
//!
//! - **search**: which transcripts mention every word of a query, and short
//!   snippets of the messages that do (the user's, the agent's, a tool's).
//! - **read**: a window of messages around one of those hits, text only,
//!   tool output clipped: enough to carry into another session.
//!
//! Claude Code's JSONL format; other agents' transcripts aren't read yet.

use std::path::{Path, PathBuf};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::Lossy;
use grep_searcher::SearcherBuilder;
use serde::Serialize;
use serde_json::Value;

use super::agents::claude::clip;

/// One message that matched.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Hit {
    /// Line in the transcript file (1-based): `read` starts from here.
    pub line: u64,
    /// `you`, `agent` or `tool`.
    pub who: &'static str,
    /// When it was said (the transcript's ISO time, if any).
    pub at: Option<String>,
    /// The message around the match, clipped.
    pub snippet: String,
}

/// A transcript's matches: the first few, and how many there were.
#[derive(Debug, Clone, Serialize)]
pub struct FileHits {
    pub path: PathBuf,
    pub hits: Vec<Hit>,
    pub total: usize,
}

/// A message's text and who said it, from one transcript line. None for
/// lines that aren't conversation (titles, snapshots, meta).
pub fn message(v: &Value) -> Option<(&'static str, String)> {
    if v["isMeta"] == true || v["isVisibleInTranscriptOnly"] == true {
        return None;
    }
    let content = &v["message"]["content"];
    match v["type"].as_str()? {
        "user" => match content {
            Value::String(s) => Some(("you", s.clone())),
            Value::Array(a) => {
                // Tool results come back as user lines.
                let mut text = Vec::new();
                let mut tool = false;
                for x in a {
                    match x["type"].as_str() {
                        Some("text") => text.extend(x["text"].as_str().map(str::to_string)),
                        Some("tool_result") => {
                            tool = true;
                            match &x["content"] {
                                Value::String(s) => text.push(s.clone()),
                                Value::Array(c) => text.extend(
                                    c.iter()
                                        .filter_map(|y| y["text"].as_str())
                                        .map(str::to_string),
                                ),
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                let t = text.join("\n");
                (!t.trim().is_empty()).then_some((if tool { "tool" } else { "you" }, t))
            }
            _ => None,
        },
        "assistant" => {
            let a = content.as_array()?;
            let mut text = Vec::new();
            for x in a {
                match x["type"].as_str() {
                    Some("text") => text.extend(x["text"].as_str().map(str::to_string)),
                    // What it ran: the tool and its main argument.
                    Some("tool_use") => {
                        let input = &x["input"];
                        let arg = ["command", "file_path", "pattern", "url", "description"]
                            .iter()
                            .find_map(|k| input[*k].as_str())
                            .unwrap_or("");
                        text.push(format!("[{}] {arg}", x["name"].as_str().unwrap_or("tool")));
                    }
                    _ => {}
                }
            }
            let t = text.join("\n");
            (!t.trim().is_empty()).then_some(("agent", t))
        }
        _ => None,
    }
}

/// The words of a query, lowercased (every one must appear in a message).
fn words(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// `text` around the first word, about `width` characters.
fn snippet(text: &str, first: &str, width: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    // Lowercase one character for one (some grow, e.g. İ), so positions
    // in it are positions in `chars`.
    let lower: String = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let Some(byte) = lower.find(first) else {
        return clip(&flat, width);
    };
    let at = lower[..byte].chars().count();
    let start = at.saturating_sub(width / 3).min(chars.len());
    let end = (start + width).min(chars.len());
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        chars[start..end].iter().collect::<String>(),
        if end < chars.len() { "…" } else { "" }
    )
}

/// Search transcripts for messages containing every word of `query`
/// (case-insensitive). Up to `per_file` hits are kept from each file (all
/// are counted); files without one are left out.
pub fn search(paths: &[PathBuf], query: &str, per_file: usize) -> Vec<FileHits> {
    let ws = words(query);
    if ws.is_empty() {
        return Vec::new();
    }
    // The rarest word would be ideal; the longest is a fine stand-in: the
    // searcher finds lines with it, the JSON check confirms the rest. A
    // word with `"` or `\` is spelled differently in the raw JSON line, so
    // it can't be the one; with no other, every line is checked.
    let key = ws
        .iter()
        .filter(|w| !w.contains(['"', '\\']))
        .max_by_key(|w| w.len())
        .cloned();
    let built = match &key {
        Some(k) => RegexMatcherBuilder::new()
            .case_insensitive(true)
            .fixed_strings(true)
            .build(k),
        None => RegexMatcherBuilder::new().build("^"),
    };
    let Ok(matcher) = built else {
        return Vec::new();
    };
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut out = Vec::new();
    for path in paths {
        let mut fh = FileHits {
            path: path.clone(),
            hits: Vec::new(),
            total: 0,
        };
        let _ = searcher.search_path(
            &matcher,
            path,
            // Lossy: one bad byte mustn't end the file's search.
            Lossy(|line, raw| {
                let Ok(v) = serde_json::from_str::<Value>(raw) else {
                    return Ok(true);
                };
                let Some((who, text)) = message(&v) else {
                    return Ok(true);
                };
                let lower = text.to_lowercase();
                if !ws.iter().all(|w| lower.contains(w.as_str())) {
                    return Ok(true);
                }
                fh.total += 1;
                if fh.hits.len() < per_file {
                    fh.hits.push(Hit {
                        line,
                        who,
                        at: v["timestamp"].as_str().map(str::to_string),
                        snippet: snippet(&text, &ws[0], 220),
                    });
                }
                Ok(true)
            }),
        );
        if fh.total > 0 {
            out.push(fh);
        }
    }
    out
}

/// One message of a read window.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub line: u64,
    pub who: &'static str,
    pub at: Option<String>,
    pub text: String,
}

/// Messages around `line` (or, without one, the last ones): up to `before`
/// before it and `after` from it, text only. Tool output is clipped to 400
/// characters and every message to 2000; the whole window stops at
/// `budget` characters.
pub fn read(
    path: &Path,
    line: Option<u64>,
    before: usize,
    after: usize,
    budget: usize,
) -> std::io::Result<Vec<Message>> {
    use std::io::BufRead;
    let f = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut all: Vec<Message> = Vec::new();
    let mut target: Option<usize> = None;
    for (i, l) in f.lines().enumerate() {
        let n = i as u64 + 1;
        let Ok(l) = l else { continue };
        // Past the window: done.
        if let Some(t) = target {
            if all.len() >= t + after {
                break;
            }
        }
        let Ok(v) = serde_json::from_str::<Value>(&l) else {
            continue;
        };
        let Some((who, text)) = message(&v) else {
            continue;
        };
        if target.is_none() && line.is_some_and(|t| n >= t) {
            target = Some(all.len());
        }
        let max = if who == "tool" { 400 } else { 2000 };
        all.push(Message {
            line: n,
            who,
            at: v["timestamp"].as_str().map(str::to_string),
            text: clip_keep_lines(&text, max),
        });
    }
    let t = target.unwrap_or(all.len());
    let start = t.saturating_sub(before);
    let end = if line.is_some() {
        (t + after).min(all.len())
    } else {
        all.len()
    };
    let window: Vec<Message> = all.drain(start..end).collect();
    // Over budget, keep what matters: from the line on, or (no line) the
    // newest messages.
    let newest_first = line.is_none();
    let mut out = Vec::new();
    let mut used = 0;
    let ordered: Box<dyn Iterator<Item = Message>> = if newest_first {
        Box::new(window.into_iter().rev())
    } else {
        Box::new(window.into_iter())
    };
    for m in ordered {
        used += m.text.len();
        if used > budget && !out.is_empty() {
            break;
        }
        out.push(m);
    }
    if newest_first {
        out.reverse();
    }
    Ok(out)
}

/// Clip to `max` characters, keeping line breaks (code reads better).
fn clip_keep_lines(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// Every Claude Code transcript on this machine (for searching outside
/// rlord's own sessions too), newest first.
pub fn all_claude_transcripts() -> Vec<PathBuf> {
    let root = super::agents::claude::config_dir().join("projects");
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for d in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().is_some_and(|x| x == "jsonl") {
                let t = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                v.push((t, p));
            }
        }
    }
    v.sort_by_key(|x| std::cmp::Reverse(x.0));
    v.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn transcript(dir: &Path) -> PathBuf {
        let p = dir.join("t.jsonl");
        let lines = [
            json!({"type": "user", "message": {"content": "fix the Auth token refresh"}, "timestamp": "t1"}),
            json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": "Looking at the refresh path."},
                {"type": "tool_use", "name": "Bash", "input": {"command": "rg refresh src/auth"}}]}}),
            json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "content": "src/auth/token.rs: fn refresh()"}]}}),
            json!({"type": "ai-title", "aiTitle": "auth refresh"}),
            json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": "The auth token now refreshes before expiry."}]}}),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn search_needs_every_word_in_one_message() {
        let d = tempfile::tempdir().unwrap();
        let p = transcript(d.path());
        let r = search(std::slice::from_ref(&p), "auth refresh", 5);
        assert_eq!(r.len(), 1);
        let who: Vec<&str> = r[0].hits.iter().map(|h| h.who).collect();
        // The prompt, the tool call, the tool output, the answer; not the title.
        assert_eq!(who, ["you", "agent", "tool", "agent"]);
        assert_eq!(r[0].hits[0].line, 1);
        assert!(r[0].hits[0].snippet.contains("Auth token refresh"));
        assert!(search(std::slice::from_ref(&p), "auth nothing", 5).is_empty());
        assert_eq!(search(std::slice::from_ref(&p), "refresh", 1)[0].total, 4);
    }

    #[test]
    fn read_windows_around_a_line() {
        let d = tempfile::tempdir().unwrap();
        let p = transcript(d.path());
        let w = read(&p, Some(3), 1, 2, 10_000).unwrap();
        let lines: Vec<u64> = w.iter().map(|m| m.line).collect();
        assert_eq!(lines, [2, 3, 5]);
        // No line: the last ones.
        let w = read(&p, None, 2, 0, 10_000).unwrap();
        assert_eq!(w.iter().map(|m| m.line).collect::<Vec<_>>(), [3, 5]);
        // The budget stops it (one message always comes back): from the
        // line on, or the newest without one.
        assert_eq!(read(&p, Some(1), 0, 10, 10).unwrap()[0].line, 1);
        let w = read(&p, None, 10, 0, 10).unwrap();
        assert_eq!(w.iter().map(|m| m.line).collect::<Vec<_>>(), [5]);
    }

    #[test]
    fn snippets_center_on_the_word() {
        let long = format!("{} needle {}", "a ".repeat(300), "b ".repeat(300));
        let s = snippet(&long, "needle", 60);
        assert!(s.starts_with('…') && s.ends_with('…') && s.contains("needle"));
        // Characters that grow when lowercased don't throw it off.
        let turkish = format!("{} needle", "İ".repeat(400));
        assert!(snippet(&turkish, "needle", 60).contains("needle"));
    }
}
