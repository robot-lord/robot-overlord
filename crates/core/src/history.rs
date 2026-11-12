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
//! Any agent's transcript: lines are read through `agents::transcript`.
//! An OpenCode conversation isn't a file but rows in OpenCode's database
//! (`agents::opencode`): the same two steps ask the database, and a hit's
//! "line" is its message's sequence number there.

use std::path::{Path, PathBuf};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::Lossy;
use grep_searcher::SearcherBuilder;
use serde::Serialize;
use serde_json::Value;

use super::agents::transcript::{clip, Format};
use super::agents::{opencode, Agent};

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
    // word with `"` or `\` is spelled differently in the raw JSON line, and
    // one with `[`/`]` may be rlord's own label (`[Bash]`), so neither can
    // be the one; with no other, every line is checked.
    let key = ws
        .iter()
        .filter(|w| !w.contains(['"', '\\', '[', ']']))
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
        let format = Format::of(path);
        let mut fh = FileHits {
            path: path.clone(),
            hits: Vec::new(),
            total: 0,
        };
        if let Some((db, id)) = opencode::locate(path) {
            // (SQLite's LIKE ignores case for ASCII only.)
            let key = key.as_deref().filter(|k| k.is_ascii());
            let (hits, total) = opencode::search(db, id, &ws, key, per_file);
            if total > 0 {
                fh.total = total;
                fh.hits = hits
                    .into_iter()
                    .map(|m| Hit {
                        line: m.seq,
                        who: m.who,
                        at: Some(opencode::iso_ms(m.at_ms)),
                        snippet: snippet(&m.text, &ws[0], 220),
                    })
                    .collect();
                out.push(fh);
            }
            continue;
        }
        let _ = searcher.search_path(
            &matcher,
            path,
            // Lossy: one bad byte mustn't end the file's search.
            Lossy(|line, raw| {
                let Ok(v) = serde_json::from_str::<Value>(raw) else {
                    return Ok(true);
                };
                let l = format.line(&v);
                let Some((who, text)) = l.message() else {
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
                        at: l.at().map(str::to_string),
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
    let format = Format::of(path);
    let mut all: Vec<Message> = Vec::new();
    let mut target: Option<usize> = None;
    if let Some((db, id)) = opencode::locate(path) {
        if !opencode::exists(db, id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such OpenCode conversation",
            ));
        }
        for m in opencode::conversation(db, id) {
            if target.is_none() && line.is_some_and(|t| m.seq >= t) {
                target = Some(all.len());
            }
            let max = if m.who == "tool" { 400 } else { 2000 };
            all.push(Message {
                line: m.seq,
                who: m.who,
                at: Some(opencode::iso_ms(m.at_ms)),
                text: clip_keep_lines(&m.text, max),
            });
        }
        return Ok(window(all, target, line, before, after, budget));
    }
    let f = std::io::BufReader::new(std::fs::File::open(path)?);
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
        let l = format.line(&v);
        let Some((who, text)) = l.message() else {
            continue;
        };
        if target.is_none() && line.is_some_and(|t| n >= t) {
            target = Some(all.len());
        }
        let max = if who == "tool" { 400 } else { 2000 };
        all.push(Message {
            line: n,
            who,
            at: l.at().map(str::to_string),
            text: clip_keep_lines(&text, max),
        });
    }
    Ok(window(all, target, line, before, after, budget))
}

/// The window of `all` around `target` (the first message at or after
/// `line`), within `budget` characters.
fn window(
    mut all: Vec<Message>,
    target: Option<usize>,
    line: Option<u64>,
    before: usize,
    after: usize,
    budget: usize,
) -> Vec<Message> {
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
    out
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

/// Every agent's transcripts on this machine (for searching outside
/// rlord's own sessions too), newest first.
pub fn all_transcripts() -> Vec<PathBuf> {
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = Agent::ALL
        .into_iter()
        .flat_map(Agent::all_transcripts)
        .collect();
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
    fn an_opencode_conversation_is_searched_and_read_in_its_database() {
        use crate::agents::opencode::tests::{store, SES};
        let d = tempfile::tempdir().unwrap();
        let p = store(d.path()).join(SES);
        // Beside a file transcript, in one search.
        let file = transcript(d.path());
        let r = search(&[file, p.clone()], "repo", 5);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].path, p);
        let hits: Vec<(u64, &str)> = r[0].hits.iter().map(|h| (h.line, h.who)).collect();
        assert_eq!(hits, [(4, "you"), (7, "agent"), (81, "agent")]);
        assert!(r[0].hits[0]
            .at
            .as_deref()
            .is_some_and(|t| t.ends_with(".711Z")));
        assert!(search(std::slice::from_ref(&p), "repo nothing", 5).is_empty());
        // A hit's line is its message's sequence number: read around it.
        let w = read(&p, Some(31), 1, 2, 10_000).unwrap();
        let said: Vec<(u64, &str)> = w.iter().map(|m| (m.line, m.who)).collect();
        assert_eq!(said, [(7, "tool"), (31, "agent"), (31, "tool")]);
        assert_eq!(w[1].text, "[read] README.md");
        // No line: the last ones.
        let w = read(&p, None, 2, 0, 10_000).unwrap();
        assert_eq!(w.iter().map(|m| m.line).collect::<Vec<_>>(), [31, 81]);
        assert!(read(&p.with_file_name("ses_none"), None, 2, 0, 100).is_err());
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
