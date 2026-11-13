//! OpenCode adapter (OpenCode v2).
//!
//! OpenCode differs from Claude Code and Codex in three ways that shape
//! everything here:
//!
//! - **One shared background server per user** runs the sessions; the TUI
//!   in a pane is a client of it. There are no command hooks: status comes
//!   from the server's event stream (`opencode_service`, read by the
//!   daemon's `opencode` task).
//! - **rlord chooses the conversation id**: `opencode -s <ses_…>` continues
//!   that session. The TUI creates a session only with its first message
//!   (and has no model flag), so a launch first creates it through the
//!   server, with its model (`launch_argv`), and then opens the TUI on it.
//! - **Conversations are rows in OpenCode's SQLite database**, not files. A
//!   session's "transcript path" in rlord is `<opencode.db>/<session id>`:
//!   not a file, but it names the store and the conversation, so the rest
//!   of rlord keeps passing paths (`locate` takes one apart). The functions
//!   under "reading conversations" answer `transcript`'s and `history`'s
//!   questions from the database.
//!
//! The database also holds OpenCode's credentials. rlord opens it
//! read-only and queries only `session_v2` and `session_message`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use super::transcript::{clip, Excerpts, TranscriptStats};

/// The oldest OpenCode rlord runs (v1 has no shared server, another
/// storage and other flags).
pub const MIN_MAJOR: u32 = 2;

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(fallback)
        })
}

/// OpenCode's database: `$XDG_DATA_HOME/opencode/opencode.db`
/// (`~/.local/share/…`, on macOS too).
pub fn db_path() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("opencode/opencode.db")
}

/// Where the background server says how to reach it:
/// `$XDG_STATE_HOME/opencode/service.json`.
pub fn service_file() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("opencode/service.json")
}

// ---- identity ----

/// A new session id in OpenCode's own shape: `ses_`, twelve hex digits of
/// time (descending, so newer sessions sort first, as OpenCode's do), and
/// fourteen random base-62 characters.
pub fn mint_session_id() -> String {
    let ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    mint_at(ms, uuid::Uuid::new_v4().as_bytes())
}

fn mint_at(ms: u64, random: &[u8; 16]) -> String {
    const MASK: u64 = (1 << 48) - 1;
    const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let time = !((ms << 12) | 1) & MASK;
    let tail: String = random[..14]
        .iter()
        .map(|b| B62[(*b as usize) % 62] as char)
        .collect();
    format!("ses_{time:012x}{tail}")
}

/// An id OpenCode would accept, and that is safe in a URL path and a file
/// name: `ses_` and letters and digits.
pub fn is_session_id(id: &str) -> bool {
    id.strip_prefix("ses_")
        .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric()))
}

// ---- commands ----

/// What the pane's shell runs for a launch: create the session through the
/// server (`$0 api POST /api/session -d $1`: OpenCode's own client, which
/// knows how to reach the server, and starts it when none is running),
/// then become the TUI on it. Whatever the first step does, the TUI
/// starts: the step gets `CREATE_LIMIT_S` seconds (polled, so nothing is
/// left behind), and when it fails or is cut off, what it said is shown
/// in the pane for a few seconds before the TUI takes the screen (and is
/// there again when the TUI exits).
///
/// Known limits: the kill reaches only the `opencode` process (an npm
/// wrapper's native child may outlive it; the TUI's own create then
/// races it harmlessly, creating is idempotent). If that step starts
/// OpenCode's server, the server inherits the step's stderr: an unlinked
/// temp file once removed (OpenCode logs to its own files, so it stays
/// small).
const CREATE_THEN_OPEN: &str = r#"e=$(mktemp 2>/dev/null) || e=
trap '[ -n "$e" ] && rm -f "$e"; exit 1' HUP TERM
"$0" api POST /api/session -d "$1" </dev/null >/dev/null 2>"${e:-/dev/null}" & p=$!
n=0
while kill -0 "$p" 2>/dev/null; do
  if [ "$n" -ge 100 ]; then kill "$p" 2>/dev/null; echo "rlord: OpenCode took over 10s to create the session" >&2; break; fi
  sleep 0.1; n=$((n+1))
done
wait "$p" 2>/dev/null; s=$?
if [ "$s" -ne 0 ]; then
  echo "rlord: OpenCode didn't create the session first (exit $s); opening it anyway" >&2
  [ -n "$e" ] && head -c 2000 "$e" >&2
  sleep 3
fi
[ -n "$e" ] && rm -f "$e"
trap - HUP TERM
shift; exec "$0" "$@""#;

/// How long the launch's create step may take (`CREATE_THEN_OPEN` counts
/// tenths of a second up to it), and with its message on screen, how long
/// a launch may go before the TUI is up.
pub const CREATE_LIMIT_S: u64 = 10;
pub const CREATE_PAUSE_S: u64 = 3;

/// `opencode -s <id> [--prompt <text>]`: the full TUI, on the shared
/// background server (never `--standalone`: its events are what rlord
/// reads status from).
pub fn open_argv(id: &str, prompt: Option<&str>, extra: &[String]) -> Vec<String> {
    let mut argv = vec!["opencode".to_string(), "-s".to_string(), id.to_string()];
    argv.extend(extra.iter().cloned());
    if let Some(p) = prompt.map(str::trim).filter(|p| !p.is_empty()) {
        argv.push("--prompt".to_string());
        argv.push(p.to_string());
    }
    argv
}

/// What `POST /api/session` takes to create session `id` in `dir`, on
/// `model` when one is named (and is a `provider/model` name).
pub fn create_body(id: &str, dir: &str, model: Option<&str>) -> Value {
    let mut body = serde_json::json!({"id": id, "location": {"directory": dir}});
    if let Some(m) = model.and_then(model_ref) {
        body["model"] = m;
    }
    body
}

/// A new conversation: the session is created first, then the TUI opened
/// on it (`open_argv`), in one pane command:
/// `sh -c '<create, then exec>' opencode <create body> -s <id> …`.
///
/// Why not the TUI alone: checked against OpenCode 2.2.7, `opencode -s
/// <new id>` creates nothing until the first message, takes no model, and
/// doesn't notice a session created under that id afterwards. Created
/// first, the session exists from the start (so rlord can show it idle)
/// and has its model before any prompt runs. Creating is idempotent: an
/// existing session is returned as it is.
pub fn launch_argv(
    id: &str,
    dir: &str,
    model: Option<&str>,
    prompt: Option<&str>,
    extra: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = vec!["sh".into(), "-c".into(), CREATE_THEN_OPEN.into()];
    let open = open_argv(id, prompt, extra);
    argv.push(open[0].clone());
    argv.push(create_body(id, dir, model).to_string());
    argv.extend(open[1..].iter().cloned());
    argv
}

/// Continue session `id`, given whether OpenCode has it (`has`, from
/// `exists`): the command, and whether it is opened as it is (true) or
/// created anew under the same id (false).
///
/// A session OpenCode has keeps its own model (the user may have changed
/// it there since): just opened. One it doesn't have (never created, or
/// deleted there) starts under the same id, created with the launch's
/// model. When that can't be told (its database couldn't be read), it is
/// opened as it is: never created over, so a model the user chose there
/// is never replaced.
pub fn resume_argv(
    id: &str,
    has: Option<bool>,
    dir: &str,
    model: Option<&str>,
    extra: &[String],
) -> (Vec<String>, bool) {
    match has {
        Some(false) => (launch_argv(id, dir, model, None, extra), false),
        Some(true) | None => (open_argv(id, None, extra), true),
    }
}

/// The overlord folder's `opencode.json`: rlord's MCP server, and the
/// rules that let the overlord call its tools (and read) without a prompt
/// while everything else asks. OpenCode has no flags for these; its server
/// reads the config of the folder a session runs in.
///
/// - `mcp.servers.rlord`: `rlord mcp --socket <socket>`. The server process
///   belongs to OpenCode's shared server, whose environment is whatever
///   first started it (another rlord's, or none), so where the daemon is
///   is given, never looked up; and what `rlord mcp` would read from its
///   environment (`MCP_ENV`: the calling session) is blanked rather than
///   inherited from some other pane.
/// - `permissions`: the last matching rule wins, and an agent starts from
///   "allow everything", so everything is set to ask first, then reading,
///   rlord's tools (named `rlord_<tool>`) and rlord's own sub-agents
///   (`subagents`, by name: any other agent asks before it is started) are
///   allowed. Code Mode (`execute`, the script OpenCode calls MCP tools
///   from) is not allowed by name: OpenCode checks each tool a script
///   calls (an rlord tool runs unasked, `fetch` asks as a web fetch), not
///   the script, and a script can't import `fs` or `child_process`.
/// - `agents.<name>.permissions`: the same rules again, for every agent a
///   conversation in the folder can run as: OpenCode's built-in ones
///   (`build`, `plan`, `general`, `explore`), rlord's sub-agents, and the
///   agents the user defined globally (`user_agents`, names only).
///   OpenCode applies every file's top-level rules first and the
///   agent-level ones after, so a user's own global agent rule (an `allow`
///   for the shell under `agent.build` or `agent.general`) would otherwise
///   come last and win. Folder rules come after the user's global ones at
///   each level.
/// - `confirm_sends` (`[overlord] confirm_sends`): `SEND_TOOLS` ask
///   each time (a rule after `rlord_*`), instead of running unasked like
///   rlord's other tools.
pub fn overlord_config(
    rlord_bin: &str,
    socket: &str,
    subagents: &[&str],
    user_agents: &[String],
    confirm_sends: bool,
) -> Value {
    let rule = |action: &str, resource: &str, effect: &str| serde_json::json!({"action": action, "resource": resource, "effect": effect});
    let mut rules = vec![
        rule("*", "*", "ask"),
        rule("read", "*", "allow"),
        rule("glob", "*", "allow"),
        rule("grep", "*", "allow"),
    ];
    rules.extend(subagents.iter().map(|n| rule("subagent", n, "allow")));
    rules.push(rule("rlord_*", "*", "allow"));
    if confirm_sends {
        for t in super::SEND_TOOLS {
            rules.push(rule(&format!("rlord_{t}"), "*", "ask"));
        }
    }
    let mut agents = serde_json::Map::new();
    for a in OVERLORD_AGENTS
        .iter()
        .chain(subagents)
        .map(|a| a.to_string())
        .chain(user_agents.iter().cloned())
    {
        agents.insert(a, serde_json::json!({"permissions": rules}));
    }
    let environment: serde_json::Map<String, Value> = super::MCP_ENV
        .iter()
        .map(|k| (k.to_string(), Value::String(String::new())))
        .collect();
    let mut command = vec![rlord_bin.to_string()];
    command.extend(super::mcp_args(socket));
    serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "mcp": {"servers": {"rlord": {
            "type": "local",
            "command": command,
            "environment": environment,
        }}},
        "permissions": rules,
        "agents": agents,
    })
}

/// OpenCode's own agents a conversation in the overlord's folder can run
/// as: the primary ones (the default, and the one Tab switches to) and the
/// built-in sub-agents.
const OVERLORD_AGENTS: &[&str] = &["build", "plan", "general", "explore"];

/// Most agent names taken from the user's config (a bound on files rlord
/// doesn't own).
const USER_AGENTS_MAX: usize = 200;

/// The names of the agents the user defined for every folder: the keys of
/// `agent` / `agents` in OpenCode's global config (`opencode.json`,
/// `opencode.jsonc` in its config folder or `$OPENCODE_CONFIG_DIR`, the
/// file `$OPENCODE_CONFIG`, the text `$OPENCODE_CONFIG_CONTENT`) and the
/// Markdown agents under its `agent/` and `agents/` folders (by path, as
/// OpenCode names them: `team/reviewer`). Read only: the files are parsed
/// and only those keys' names are kept (nothing else in them, which may
/// hold provider keys, is looked at or logged); Markdown agents by file
/// name only. Whatever can't be read is skipped.
pub fn user_agents() -> Vec<String> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let global = env("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|c| c.join("opencode"));
    let dirs: Vec<PathBuf> = global
        .into_iter()
        .chain(env("OPENCODE_CONFIG_DIR").map(PathBuf::from))
        .collect();
    let files: Vec<PathBuf> = env("OPENCODE_CONFIG")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    let content = std::env::var("OPENCODE_CONFIG_CONTENT").ok();
    user_agents_in(&dirs, &files, content.as_deref())
}

/// `user_agents`, from these config folders, extra config files and inline
/// config text.
pub fn user_agents_in(dirs: &[PathBuf], files: &[PathBuf], content: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut add = |n: String| {
        let ok = !n.is_empty()
            && n.len() <= 100
            && n.chars()
                .all(|c| c.is_alphanumeric() || "-_./ ".contains(c));
        if ok && !names.contains(&n) && names.len() < USER_AGENTS_MAX {
            names.push(n);
        }
    };
    let texts = dirs
        .iter()
        .flat_map(|d| [d.join("opencode.json"), d.join("opencode.jsonc")])
        .chain(files.iter().cloned())
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .chain(content.map(str::to_string));
    for text in texts {
        let Some(v) = parse_jsonc(&text) else {
            continue;
        };
        for key in ["agent", "agents"] {
            if let Some(m) = v.get(key).and_then(Value::as_object) {
                m.keys().cloned().for_each(&mut add);
            }
        }
    }
    for d in dirs {
        for sub in ["agent", "agents"] {
            let root = d.join(sub);
            let mut found = Vec::new();
            md_names(&root, &root, 0, &mut found);
            found.sort();
            found.into_iter().for_each(&mut add);
        }
    }
    names
}

/// The Markdown files under `dir` (a few levels down, a bounded count), as
/// paths relative to `root` without `.md`.
fn md_names(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten().take(USER_AGENTS_MAX) {
        let path = e.path();
        let Ok(kind) = e.file_type() else { continue };
        if kind.is_dir() && depth < 3 {
            md_names(root, &path, depth + 1, out);
        } else if kind.is_file() && path.extension().is_some_and(|x| x == "md") {
            if let Ok(rel) = path.with_extension("").strip_prefix(root) {
                let parts: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.push(parts.join("/"));
            }
        }
    }
}

/// JSON with comments (`//`, `/* */`) and trailing commas, as OpenCode
/// reads its config.
fn parse_jsonc(text: &str) -> Option<Value> {
    // Comments out (outside strings).
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => {
                in_str = c == '"';
                out.push(c);
            }
        }
    }
    // Trailing commas out: a `,` followed (past whitespace) by `}` or `]`.
    let chars: Vec<char> = out.chars().collect();
    let mut clean = String::with_capacity(out.len());
    let (mut in_str, mut escaped) = (false, false);
    for (i, &c) in chars.iter().enumerate() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|n| !n.is_whitespace());
            if matches!(next, Some('}' | ']')) {
                continue;
            }
        }
        clean.push(c);
    }
    serde_json::from_str(&clean).ok()
}

/// A dialog is on OpenCode's screen, where its prompt box was: a
/// permission request ("△ Permission required", then `Allow once  Always
/// allow  Reject`, where Enter allows) or a question (numbered options,
/// `↑↓ select  enter submit  esc dismiss`), as OpenCode 2.2.7 draws them.
///
/// Only the bottom box is read: the last run of `┃`-bordered lines (no
/// gap) and the line under it. The transcript above is drawn with the same
/// border, and a reply may quote these words, so each mark must be a
/// dialog's own line: `△ Permission required` opening a box line, or the
/// choices or keys together on one line.
pub fn dialog_shown(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let boxed = |l: &str| l.trim_start().starts_with('┃');
    let Some(last) = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, l)| !l.trim().is_empty())
        .take(25)
        .find(|(_, l)| boxed(l))
        .map(|(i, _)| i)
    else {
        return false;
    };
    let first = lines[..=last]
        .iter()
        .rposition(|l| !boxed(l))
        .map_or(0, |i| i + 1);
    let under = lines[last + 1..].iter().find(|l| !l.trim().is_empty());
    lines[first..=last]
        .iter()
        .chain(under)
        .any(|l| dialog_line(l))
}

/// A line only a dialog draws (its `┃` border allowed for).
fn dialog_line(line: &str) -> bool {
    let text = line.trim_start_matches(|c: char| c.is_whitespace() || c == '┃');
    let has = |a: &str, b: &str| text.contains(a) && text.contains(b);
    text.starts_with("△ Permission required")
        || has("Allow once", "Reject")
        || has("enter submit", "esc dismiss")
        || has("⇆ select", "enter confirm")
}

/// A sub-agent file for OpenCode (`.opencode/agents/<name>.md`): when to
/// use it, then its instructions as the body. It gets the folder's rules
/// (`overlord_config`), like the overlord.
pub fn subagent_md(description: &str, instructions: &str) -> String {
    format!(
        "---\ndescription: {}\nmode: subagent\n---\n\n{}\n",
        serde_json::Value::String(description.to_string()),
        instructions.trim()
    )
}

/// `provider/model[#variant]` as the server names a model:
/// `{providerID, id[, variant]}`. (A model id may itself contain `/`.)
pub fn model_ref(model: &str) -> Option<Value> {
    let (provider, rest) = model.trim().split_once('/')?;
    let (id, variant) = match rest.split_once('#') {
        Some((id, v)) => (id, Some(v)),
        None => (rest, None),
    };
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    let mut v = serde_json::json!({"providerID": provider, "id": id});
    if let Some(variant) = variant.filter(|v| !v.is_empty()) {
        v["variant"] = Value::String(variant.to_string());
    }
    Some(v)
}

/// A model as rlord shows it: `provider/model` (from `{providerID, id}`).
pub fn model_name(m: &Value) -> Option<String> {
    Some(format!(
        "{}/{}",
        m["providerID"].as_str().filter(|p| !p.is_empty())?,
        m["id"].as_str().filter(|p| !p.is_empty())?
    ))
}

// ---- version ----

/// The major version in what `opencode --version` prints (`opencode
/// v2.2.7`, or a bare `1.4.3`).
pub fn major(version: &str) -> Option<u32> {
    let at = version.find(|c: char| c.is_ascii_digit())?;
    version[at..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Why this OpenCode can't be run by rlord, from its `--version` line
/// (None: it can, or the version can't be told).
pub fn too_old(version: &str) -> Option<String> {
    let m = major(version)?;
    (m < MIN_MAJOR).then(|| {
        format!(
            "rlord needs OpenCode v{MIN_MAJOR} or newer (this is {})",
            version.trim()
        )
    })
}

// ---- models ----

/// What `opencode models` prints, one `provider/model` a line.
pub fn parse_models(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| {
            l.contains('/')
                && !l.contains(char::is_whitespace)
                && l.chars().all(|c| c.is_ascii_graphic())
        })
        .map(str::to_string)
        .collect()
}

/// The model list, and when each way of getting it was last tried.
#[derive(Default)]
struct Listing {
    models: Vec<String>,
    /// A list was last stored.
    listed: Option<Instant>,
    /// The server / the command was last asked, whatever it answered: a
    /// way that failed isn't tried again until the same time has passed.
    server_tried: Option<Instant>,
    command_tried: Option<Instant>,
}

static MODELS: std::sync::Mutex<Listing> = std::sync::Mutex::new(Listing {
    models: Vec::new(),
    listed: None,
    server_tried: None,
    command_tried: None,
});

/// How long a model list (or a failed attempt at one) is taken as current.
const MODELS_FOR: Duration = Duration::from_secs(600);

/// The models OpenCode offers, as last listed (`list_models`). Starts
/// nothing: safe under a lock. Empty until listed: the model is typed in.
pub fn models() -> Vec<String> {
    MODELS.lock().map(|m| m.models.clone()).unwrap_or_default()
}

fn recent(at: Option<Instant>) -> bool {
    at.is_some_and(|t| t.elapsed() < MODELS_FOR)
}

/// Which ways to try now, each marked as tried: (the server, the command).
/// Neither while a list is current; each at most once per `MODELS_FOR`,
/// so a server that doesn't answer costs its wait once, not on every call.
fn models_due(command: bool) -> (bool, bool) {
    let Ok(mut m) = MODELS.lock() else {
        return (false, false);
    };
    if recent(m.listed) {
        return (false, false);
    }
    let server = !recent(m.server_tried);
    let command = command && !recent(m.command_tried);
    let now = Some(Instant::now());
    if server {
        m.server_tried = now;
    }
    if command {
        m.command_tried = now;
    }
    (server, command)
}

fn keep_models(listed: Vec<String>) -> bool {
    // (A failed or empty listing keeps the last one.)
    if listed.is_empty() {
        return false;
    }
    if let Ok(mut m) = MODELS.lock() {
        m.models = listed;
        m.listed = Some(Instant::now());
    }
    true
}

/// List OpenCode's models when there is no current list: from its
/// background server when one is running (`GET /api/model`), and
/// otherwise, only when `bin` is given, by running `<bin> models` (five
/// seconds at most). An attempt that fails is remembered as long as a
/// list is (ten minutes).
///
/// `bin` is for when the user asked to see them (the new-session form,
/// `rlord agents`): the command may start OpenCode's server, which rlord
/// must not do on its own (at daemon start, before a launch). Makes a
/// request or runs a command: never call it holding a lock others wait on.
pub fn list_models(bin: Option<&Path>) {
    let (server, command) = models_due(bin.is_some());
    let served = server
        && super::opencode_service::Service::read(&service_file())
            .and_then(|svc| svc.models())
            .is_some_and(keep_models);
    if served {
        return;
    }
    if let Some(bin) = bin.filter(|_| command) {
        let listed = run_bounded(bin, &["models"], Duration::from_secs(5))
            .map(|out| parse_models(&out))
            .unwrap_or_default();
        keep_models(listed);
    }
}

/// Run a command and take what it prints, giving up at `limit`. Its output
/// goes to a file, not a pipe: `opencode` may start its background server,
/// which would inherit a pipe and hold it open long after the command.
fn run_bounded(bin: &Path, args: &[&str], limit: Duration) -> Option<String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args)
        .env_remove("RLORD_SESSION_ID")
        .stderr(std::process::Stdio::null());
    crate::process::run_to_file(cmd, &std::env::temp_dir(), limit, None)
        .ok()
        .filter(|(st, _)| st.success())
        .map(|(_, text)| text)
}

// ---- archive summaries (the fallback; see the daemon's `summary`) ----

/// `opencode run` for a one-shot answer in a session of its own, created
/// archived so it stays out of OpenCode's session lists; the caller reads
/// the answer from the database and deletes that session.
/// `fork`: the conversation to read in full (a fork of it; the session
/// itself is untouched). Otherwise `own` is the id the new session gets.
pub fn summary_argv(model: &str, fork: Option<&str>, own: &str, ask: &str) -> Vec<String> {
    let mut a: Vec<String> = vec!["opencode".into(), "run".into(), "-s".into()];
    match fork {
        Some(id) => a.extend([id.to_string(), "--fork".to_string()]),
        None => a.push(own.to_string()),
    }
    a.push("--archived".into());
    if !model.trim().is_empty() {
        a.extend(["-m".to_string(), model.trim().to_string()]);
    }
    a.push(ask.to_string());
    a
}

/// `opencode session delete <id>`.
pub fn delete_argv(id: &str) -> Vec<String> {
    ["opencode", "session", "delete", id]
        .map(String::from)
        .to_vec()
}

// ---- reading conversations ----

/// The path rlord keeps for conversation `id`: `<opencode.db>/<id>`.
pub fn transcript_path(id: &str) -> PathBuf {
    db_path().join(id)
}

/// Whether a path names an OpenCode conversation, by its shape alone.
pub fn is_transcript(path: &Path) -> bool {
    locate(path).is_some()
}

/// Take a conversation's path apart: (the database, the session id).
pub fn locate(path: &Path) -> Option<(&Path, &str)> {
    let id = path.file_name()?.to_str()?;
    let db = path.parent()?;
    (is_session_id(id) && db.file_name().is_some_and(|n| n == "opencode.db")).then_some((db, id))
}

fn open(db: &Path) -> Option<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    if !db.is_file() {
        return None;
    }
    let c = rusqlite::Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // OpenCode's server may be writing: wait a little, never long.
    c.busy_timeout(Duration::from_millis(200)).ok()?;
    Some(c)
}

/// One message of a conversation, in rlord's common shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    /// The row's sequence number in its session: what `history` calls the
    /// line. A row with several parts (text, tool calls) shares it.
    pub seq: u64,
    pub at_ms: i64,
    /// `you`, `agent` or `tool`.
    pub who: &'static str,
    pub text: String,
}

/// A tool call's main argument.
fn main_arg(input: &Value) -> String {
    [
        "command",
        "cmd",
        "path",
        "file_path",
        "filePath",
        "pattern",
        "query",
        "url",
        "description",
        "prompt",
    ]
    .iter()
    .find_map(|k| input[*k].as_str().map(str::to_string))
    .unwrap_or_else(|| match input {
        Value::Null => String::new(),
        other => clip(&other.to_string(), 200),
    })
}

fn parts_text(parts: &Value) -> Option<String> {
    let t = parts
        .as_array()?
        .iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!t.trim().is_empty()).then_some(t)
}

/// A `session_message` row as conversation messages: the user's prompt; an
/// assistant row's text, each tool call (`[tool] main argument`) and what
/// it returned. Reasoning and bookkeeping rows (`idle`, …) aren't
/// conversation.
pub fn said(kind: &str, data: &Value) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    match kind {
        "user" => {
            if let Some(t) = data["text"].as_str().filter(|t| !t.trim().is_empty()) {
                out.push(("you", t.to_string()));
            }
        }
        "assistant" => {
            for part in data["content"].as_array().into_iter().flatten() {
                match part["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = part["text"].as_str().filter(|t| !t.trim().is_empty()) {
                            out.push(("agent", t.to_string()));
                        }
                    }
                    Some("tool") => {
                        let name = part["name"].as_str().unwrap_or("tool");
                        let state = &part["state"];
                        out.push((
                            "agent",
                            format!("[{name}] {}", main_arg(&state["input"]))
                                .trim_end()
                                .to_string(),
                        ));
                        let result = parts_text(&state["content"]).or_else(|| {
                            state["error"]
                                .as_str()
                                .filter(|e| !e.trim().is_empty())
                                .map(str::to_string)
                        });
                        if let Some(t) = result {
                            out.push(("tool", t));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// The text an assistant row answered with (its text parts).
fn reply_text(data: &Value) -> Option<String> {
    parts_text(&data["content"])
}

/// A LIKE pattern (with `ESCAPE '\'`) for rows whose JSON holds `word` in
/// a string: the word as JSON writes it (`"` is `\"`, a line break `\n`),
/// and LIKE's own wildcards (`%`, `_`) and escape character taken
/// literally.
fn like_pattern(word: &str) -> String {
    let json = serde_json::to_string(word).unwrap_or_default();
    let inner = json
        .strip_prefix('"')
        .and_then(|j| j.strip_suffix('"'))
        .unwrap_or(word);
    let mut out = String::from("%");
    for c in inner.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// Rows of one session, oldest first, through `each` (false stops).
/// `like`: only rows whose JSON contains it (ASCII case-insensitive).
fn each_row(
    c: &rusqlite::Connection,
    id: &str,
    like: Option<&str>,
    mut each: impl FnMut(u64, &str, i64, &Value) -> bool,
) {
    let sql = format!(
        "SELECT seq, type, time_created, data FROM session_message \
         WHERE session_id = ?1 AND type IN ('user', 'assistant') {} ORDER BY seq",
        if like.is_some() {
            "AND data LIKE ?2 ESCAPE '\\'"
        } else {
            "AND ?2 IS NULL"
        }
    );
    let Ok(mut q) = c.prepare(&sql) else {
        return; // not a schema rlord knows
    };
    let pattern = like.map(like_pattern);
    let Ok(mut rows) = q.query(rusqlite::params![id, pattern]) else {
        return;
    };
    while let Ok(Some(r)) = rows.next() {
        let (Ok(seq), Ok(kind), Ok(at), Ok(data)) = (
            r.get::<_, i64>(0),
            r.get::<_, String>(1),
            r.get::<_, i64>(2),
            r.get::<_, String>(3),
        ) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if !each(seq.max(0) as u64, &kind, at, &v) {
            break;
        }
    }
}

/// The whole conversation, oldest first.
pub fn conversation(db: &Path, id: &str) -> Vec<Said> {
    let Some(c) = open(db) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    each_row(&c, id, None, |seq, kind, at_ms, data| {
        out.extend(said(kind, data).into_iter().map(|(who, text)| Said {
            seq,
            at_ms,
            who,
            text,
        }));
        true
    });
    out
}

/// OpenCode's database, opened once for several questions (a search over
/// many sessions).
pub struct Store(rusqlite::Connection);

impl Store {
    pub fn open(db: &Path) -> Option<Store> {
        open(db).map(Store)
    }

    /// Messages of `id` containing every word of `words` (lowercased),
    /// oldest first: all of them counted, the first `keep` returned.
    /// `key`: a word that must appear in a row's raw JSON (narrows what is
    /// parsed).
    pub fn search(
        &self,
        id: &str,
        words: &[String],
        key: Option<&str>,
        keep: usize,
    ) -> (Vec<Said>, usize) {
        let (mut hits, mut total) = (Vec::new(), 0);
        each_row(&self.0, id, key, |seq, kind, at_ms, data| {
            for (who, text) in said(kind, data) {
                let lower = text.to_lowercase();
                if words.iter().all(|w| lower.contains(w.as_str())) {
                    total += 1;
                    if hits.len() < keep {
                        hits.push(Said {
                            seq,
                            at_ms,
                            who,
                            text,
                        });
                    }
                }
            }
            true
        });
        (hits, total)
    }
}

/// `Store::search` on a database opened for it.
pub fn search(
    db: &Path,
    id: &str,
    words: &[String],
    key: Option<&str>,
    keep: usize,
) -> (Vec<Said>, usize) {
    match Store::open(db) {
        Some(store) => store.search(id, words, key, keep),
        None => (Vec::new(), 0),
    }
}

/// One row's `data` by a query that selects it.
fn one_data(c: &rusqlite::Connection, sql: &str, id: &str) -> Option<Value> {
    let data: String = c.query_row(sql, [id], |r| r.get(0)).ok()?;
    serde_json::from_str(&data).ok()
}

/// The prompt that started it, the latest prompt, the latest reply and the
/// title, by a few indexed look-ups (never the whole conversation).
pub fn excerpts(db: &Path, id: &str) -> Excerpts {
    let Some(c) = open(db) else {
        return Excerpts::default();
    };
    let prompt = |order: &str| {
        one_data(
            &c,
            &format!(
                "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'user' \
                 ORDER BY seq {order} LIMIT 1"
            ),
            id,
        )
        .and_then(|v| v["text"].as_str().map(|t| clip(t, 300)))
        .filter(|t| !t.is_empty())
    };
    let first_prompt = prompt("ASC");
    let last_prompt = prompt("DESC").filter(|l| Some(l) != first_prompt.as_ref());
    Excerpts {
        first_prompt,
        last_prompt,
        last_reply: last_reply(&c, id).map(|t| clip(&t, 500)),
        title: c
            .query_row("SELECT title FROM session_v2 WHERE id = ?1", [id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .ok()
            .flatten()
            .filter(|t| !t.trim().is_empty())
            .map(|t| clip(&t, 120)),
    }
}

/// The latest text the agent answered with (among its last rows: a turn
/// ends in tool-free text, but the very last row may be a tool call).
fn last_reply(c: &rusqlite::Connection, id: &str) -> Option<String> {
    let mut q = c
        .prepare(
            "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'assistant' \
             ORDER BY seq DESC LIMIT 20",
        )
        .ok()?;
    let rows = q.query_map([id], |r| r.get::<_, String>(0)).ok()?;
    for data in rows.flatten() {
        let reply = serde_json::from_str::<Value>(&data)
            .ok()
            .and_then(|v| reply_text(&v));
        if reply.is_some() {
            return reply;
        }
    }
    None
}

/// The latest reply of conversation `id` (for a summary run's answer).
pub fn latest_reply(db: &Path, id: &str) -> Option<String> {
    last_reply(&open(db)?, id)
}

/// When a session's latest turn began and ended, as its row records them
/// (`time_busy`, `time_idle`; ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Turn {
    pub busy: Option<i64>,
    pub idle: Option<i64>,
}

impl Turn {
    /// A turn is running: it began after the last one ended.
    pub fn working(&self) -> bool {
        match (self.busy, self.idle) {
            (Some(b), Some(i)) => b > i,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

/// The latest turn of session `id`. None: OpenCode has no such session
/// (yet), or its database can't be read.
pub fn turn(db: &Path, id: &str) -> Option<Turn> {
    open(db)?
        .query_row(
            "SELECT time_busy, time_idle FROM session_v2 WHERE id = ?1",
            [id],
            |r| {
                Ok(Turn {
                    busy: r.get(0)?,
                    idle: r.get(1)?,
                })
            },
        )
        .ok()
}

/// What OpenCode's session row says: token totals, when it started and
/// was last written, and whether a turn is running (`time_busy` after
/// `time_idle`). None: no such session (yet).
pub fn stats(db: &Path, id: &str) -> Option<TranscriptStats> {
    let c = open(db)?;
    c.query_row(
        "SELECT tokens_input, tokens_output, time_created, time_updated, time_busy, time_idle \
         FROM session_v2 WHERE id = ?1",
        [id],
        |r| {
            let (busy, idle): (Option<i64>, Option<i64>) = (r.get(4)?, r.get(5)?);
            let updated: i64 = r.get(3)?;
            Ok(TranscriptStats {
                input_tokens: r.get(0)?,
                output_tokens: r.get(1)?,
                first: Some(r.get(2)?),
                // (A row's own times; `time_updated` stops at the last
                // message, `time_idle` is when the turn ended.)
                last: Some(updated.max(idle.unwrap_or(0))),
                ended: busy.is_some() && !Turn { busy, idle }.working(),
                turn_seen: busy.is_some(),
                modified: Some(
                    SystemTime::UNIX_EPOCH
                        + Duration::from_millis(updated.max(idle.unwrap_or(0)).max(0) as u64),
                ),
                // (The Usage column's numbers: `usage`.)
                ..Default::default()
            })
        },
    )
    .ok()
}

// ---- usage limits ----
//
// OpenCode 2.2.7 ends a step its provider refused with the error on the
// assistant row: `finish: "error"`, `time.completed`, and `error: {type,
// message, status, response: {body}, retryAfterMs}` (`session.step.failed`
// in `core/src/session/message-updater.ts`; the shape is `SessionError`
// in `schema/src/session-error.ts`). `toSessionError`
// (`core/src/session/to-session-error.ts`) names the limits of OpenCode's
// own service by `type`, and OpenCode's timeline shows when they reset
// (`usageLimit` and `budgetLimit` in `session-ui/src/timeline/projection.ts`):
//
// - `provider.free-limit`, `provider.go-limit`: `retryAfterMs` (the 429's
//   `retry-after`) after the step ended, at most 45 days;
// - `provider.budget-limit`, `provider.model-budget-limit`: `resetsAt`
//   (ISO) in the response body's metadata.
//
// Others carry no reset time (`provider.chatgpt-limit`, `provider.quota`)
// or are throttles OpenCode retries itself (`provider.rate-limit`): not
// limits rlord can wait out.

/// Longest `retryAfterMs` taken as a reset (OpenCode's timeline's own
/// bound).
const MAX_RETRY_AFTER_MS: i64 = 45 * 86_400_000;

/// An assistant row's usage limit: (when it hit, when it resets, which;
/// ms). None: no limit, or one with no reset time.
pub(crate) fn usage_limit(data: &Value) -> Option<(i64, i64, String)> {
    let e = &data["error"];
    let kind = e["type"].as_str()?.strip_prefix("provider.")?;
    let t = &data["time"];
    let at = t["completed"].as_i64().or_else(|| t["created"].as_i64())?;
    let resets_at = match kind {
        "free-limit" | "go-limit" => {
            at + e["retryAfterMs"]
                .as_i64()
                .filter(|d| (0..=MAX_RETRY_AFTER_MS).contains(d))?
        }
        "budget-limit" | "model-budget-limit" => {
            let body: Value = serde_json::from_str(e["response"]["body"].as_str()?).ok()?;
            super::transcript::parse_iso_ms(find_str(&body, "resetsAt")?)?
        }
        _ => return None,
    };
    Some((at, resets_at, kind.to_string()))
}

/// The first string under `key` anywhere in `v` (OpenCode's envelope has
/// it in `metadata`, Google's in `error.details[].metadata`).
fn find_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    match v {
        Value::Object(m) => m
            .get(key)
            .and_then(Value::as_str)
            .or_else(|| m.values().find_map(|x| find_str(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find_str(x, key)),
        _ => None,
    }
}

/// The usage limit conversation `id` is stopped on: its latest assistant
/// row ended on one. `touched`: a prompt came after it (an answer after
/// it would be a newer assistant row). Two indexed look-ups.
pub fn limit_hit(db: &Path, id: &str) -> Option<super::transcript::LimitHit> {
    let c = open(db)?;
    let (row, seq, data): (String, i64, String) = c
        .query_row(
            "SELECT id, seq, data FROM session_message WHERE session_id = ?1 \
             AND type = 'assistant' ORDER BY seq DESC LIMIT 1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()?;
    // (Every limit's type ends so: skip parsing the rest.)
    if !data.contains("-limit\"") {
        return None;
    }
    let (at, resets_at, kind) = usage_limit(&serde_json::from_str(&data).ok()?)?;
    let touched = c
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM session_message WHERE session_id = ?1 \
             AND type = 'user' AND seq > ?2)",
            rusqlite::params![id, seq],
            |r| r.get::<_, bool>(0),
        )
        .unwrap_or(false);
    Some(super::transcript::LimitHit {
        at,
        resets_at,
        kind,
        touched,
        key: super::transcript::fnv(&row),
    })
}

/// The size of the context the latest request sent (its input and cached
/// tokens), 0 when there has been none.
pub fn context_tokens(db: &Path, id: &str) -> u64 {
    let Some(c) = open(db) else {
        return 0;
    };
    one_data(
        &c,
        "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'assistant' \
         ORDER BY seq DESC LIMIT 1",
        id,
    )
    .map(|v| {
        let t = &v["tokens"];
        [&t["input"], &t["cache"]["read"], &t["cache"]["write"]]
            .iter()
            .filter_map(|n| n.as_u64())
            .sum()
    })
    .unwrap_or(0)
}

/// The Usage column's numbers for a conversation: prompts typed (its
/// `user` rows), tokens (the session row's input and output totals), the
/// latest request's context and model. None: no such session (yet). See
/// `usage_many`.
pub fn usage(db: &Path, id: &str) -> Option<TranscriptStats> {
    usage_many(db, &[id]).remove(id)
}

/// `usage` for several conversations in one database (a list's rows):
/// three indexed look-ups each, only in `session_v2` and
/// `session_message`, over one connection. Answers are kept until the
/// database or its write-ahead log changes (size or time), so a list that
/// asks again and again costs a `stat`. Sessions not there are left out.
pub fn usage_many(db: &Path, ids: &[&str]) -> std::collections::HashMap<String, TranscriptStats> {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    type Stamp = [(u64, Option<SystemTime>); 2];
    static CACHE: LazyLock<Mutex<HashMap<String, (Stamp, TranscriptStats)>>> =
        LazyLock::new(Default::default);
    let stamp_of = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| (m.len(), m.modified().ok()))
            .unwrap_or((0, None))
    };
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    let stamp: Stamp = [stamp_of(db), stamp_of(Path::new(&wal))];
    let key = |id: &str| format!("{}/{id}", db.display());
    let mut out = HashMap::new();
    let mut missed = Vec::new();
    if let Ok(cache) = CACHE.lock() {
        for id in ids {
            match cache.get(&key(id)) {
                Some((s, st)) if *s == stamp => {
                    out.insert(id.to_string(), st.clone());
                }
                _ => missed.push(*id),
            }
        }
    }
    if missed.is_empty() {
        return out;
    }
    let Some(c) = open(db) else { return out };
    let mut fresh = Vec::new();
    for id in missed {
        let Ok((input, output)) = c.query_row(
            "SELECT tokens_input, tokens_output FROM session_v2 WHERE id = ?1",
            [id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        ) else {
            continue;
        };
        let turns: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM session_message WHERE session_id = ?1 AND type = 'user'",
                [id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let last = one_data(
            &c,
            "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'assistant' \
             ORDER BY seq DESC LIMIT 1",
            id,
        );
        let st = TranscriptStats {
            input_tokens: input,
            output_tokens: output,
            turns: turns.max(0) as u64,
            tokens: input + output,
            context: last
                .as_ref()
                .map(|v| {
                    let t = &v["tokens"];
                    [&t["input"], &t["cache"]["read"], &t["cache"]["write"]]
                        .iter()
                        .filter_map(|n| n.as_u64())
                        .sum()
                })
                .unwrap_or(0),
            model: last.as_ref().and_then(|v| model_name(&v["model"])),
            ..Default::default()
        };
        fresh.push((id, st));
    }
    if let Ok(mut cache) = CACHE.lock() {
        // (Bounded: one entry per OpenCode session rlord has listed.)
        if cache.len() + fresh.len() > 1000 {
            cache.clear();
        }
        for (id, st) in &fresh {
            cache.insert(key(id), (stamp, st.clone()));
        }
    }
    out.extend(fresh.into_iter().map(|(id, st)| (id.to_string(), st)));
    out
}

/// The model of the latest turn (`provider/model`); before any, the one
/// the session is set to.
pub fn observed_model(db: &Path, id: &str) -> Option<String> {
    let c = open(db)?;
    one_data(
        &c,
        "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'assistant' \
         ORDER BY seq DESC LIMIT 1",
        id,
    )
    .and_then(|v| model_name(&v["model"]))
    .or_else(|| {
        one_data(&c, "SELECT model FROM session_v2 WHERE id = ?1", id).and_then(|m| model_name(&m))
    })
}

/// Whether OpenCode has a session `id`: `Some(false)` when its database
/// doesn't list it (or there is no database: OpenCode has no sessions),
/// None when that can't be told (the database couldn't be read just now).
pub fn exists(db: &Path, id: &str) -> Option<bool> {
    use rusqlite::OptionalExtension;
    if !db.exists() {
        return Some(false);
    }
    open(db)?
        .query_row("SELECT 1 FROM session_v2 WHERE id = ?1", [id], |_| Ok(()))
        .optional()
        .ok()
        .map(|row| row.is_some())
}

/// The folder a session runs in.
pub fn directory(db: &Path, id: &str) -> Option<String> {
    open(db)?
        .query_row(
            "SELECT directory FROM session_v2 WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .ok()
}

/// The session `id` is a child of (a sub-agent's session names its
/// parent): `Some(Some(parent))`, or `Some(None)` for a session that has
/// none. `None` when it can't be told (the database couldn't be read, or
/// doesn't list the session yet): ask again, don't remember that.
pub fn parent(db: &Path, id: &str) -> Option<Option<String>> {
    use rusqlite::OptionalExtension;
    open(db)?
        .query_row(
            "SELECT parent_id FROM session_v2 WHERE id = ?1",
            [id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .ok()?
}

/// The sessions under `id`, with their latest turn: its sub-agents' child
/// sessions and theirs (four levels, forty sessions at most, newest first
/// within a level).
pub fn descendants(db: &Path, id: &str) -> Vec<(String, Turn)> {
    let Some(c) = open(db) else {
        return Vec::new();
    };
    let Ok(mut q) = c.prepare(
        "SELECT id, time_busy, time_idle FROM session_v2 WHERE parent_id = ?1 \
         ORDER BY time_created DESC LIMIT 40",
    ) else {
        return Vec::new();
    };
    let mut out: Vec<(String, Turn)> = Vec::new();
    let mut level = vec![id.to_string()];
    for _ in 0..4 {
        let mut next = Vec::new();
        for parent in &level {
            let Ok(rows) = q.query_map([parent], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Turn {
                        busy: r.get(1)?,
                        idle: r.get(2)?,
                    },
                ))
            }) else {
                continue;
            };
            for (child, turn) in rows.flatten() {
                if out.len() < 40 && is_session_id(&child) && !out.iter().any(|(c, _)| c == &child)
                {
                    out.push((child.clone(), turn));
                    next.push(child);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }
    out
}

/// The model session `id` is set to (`provider/model`), from its row.
pub fn session_model(db: &Path, id: &str) -> Option<String> {
    let c = open(db)?;
    one_data(&c, "SELECT model FROM session_v2 WHERE id = ?1", id).and_then(|m| model_name(&m))
}

/// The newest archived fork of `id` created at or after `since_ms`: the
/// one a summary run made (`summary_argv`). Archived only, so a fork the
/// user made themselves at that moment is never taken for it.
pub fn fork_of(db: &Path, id: &str, since_ms: i64) -> Option<String> {
    open(db)?
        .query_row(
            "SELECT id FROM session_v2 WHERE fork_session_id = ?1 AND time_created >= ?2 \
             AND time_archived IS NOT NULL ORDER BY time_created DESC LIMIT 1",
            rusqlite::params![id, since_ms],
            |r| r.get(0),
        )
        .ok()
}

/// Every top-level session (not sub-agents' child sessions), with when it
/// was last written.
pub fn sessions(db: &Path) -> Vec<(SystemTime, String)> {
    let Some(c) = open(db) else {
        return Vec::new();
    };
    let Ok(mut q) = c.prepare("SELECT id, time_updated FROM session_v2 WHERE parent_id IS NULL")
    else {
        return Vec::new();
    };
    let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)));
    match rows {
        Ok(rows) => rows
            .flatten()
            .filter(|(id, _)| is_session_id(id))
            .map(|(id, ms)| {
                (
                    SystemTime::UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64),
                    id,
                )
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Milliseconds since the epoch as ISO-8601 (`2026-11-12T16:55:21.075Z`),
/// the form the other agents' transcripts give message times in.
pub fn iso_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    // civil from days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_users_global_agents_are_named() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("opencode");
        std::fs::create_dir_all(d.join("agents/team")).unwrap();
        std::fs::create_dir_all(d.join("agent")).unwrap();
        std::fs::write(
            d.join("opencode.json"),
            r#"{"provider": {"x": {"options": {"apiKey": "secret"}}},
                "agent": {"general": {"permission": {"bash": "allow"}}, "mine": {}}}"#,
        )
        .unwrap();
        std::fs::write(
            d.join("opencode.jsonc"),
            "// mine\n{\"agents\": {\"review\": {\"prompt\": \"a // not a comment\",},}, /* x */}\n",
        )
        .unwrap();
        std::fs::write(d.join("agents/team/nested.md"), "---\nmode: primary\n---\n").unwrap();
        std::fs::write(d.join("agent/solo.md"), "x").unwrap();
        std::fs::write(d.join("agent/notes.txt"), "x").unwrap();
        let extra = tmp.path().join("extra.json");
        std::fs::write(&extra, r#"{"agent": {"fromfile": {}}}"#).unwrap();
        let names = user_agents_in(
            &[d.clone(), tmp.path().join("missing")],
            &[extra],
            Some(r#"{"agent": {"inline": {}, "mine": {}}}"#),
        );
        assert_eq!(
            names,
            [
                "general",
                "mine",
                "review",
                "fromfile",
                "inline",
                "solo",
                "team/nested"
            ]
        );
        // Nothing else from the files.
        assert!(!names.iter().any(|n| n.contains("secret") || n == "x"));
        // A broken file is skipped.
        std::fs::write(d.join("opencode.json"), "{ not json").unwrap();
        assert!(!user_agents_in(&[d], &[], None).contains(&"mine".to_string()));
    }

    #[test]
    fn every_agent_gets_the_rules() {
        let users = vec!["mine".to_string()];
        let cfg = overlord_config("/bin/rlord", "/s", &["rlord-tidy"], &users, false);
        for a in ["build", "plan", "general", "explore", "rlord-tidy", "mine"] {
            assert_eq!(cfg["agents"][a]["permissions"], cfg["permissions"], "{a}");
        }
        let last = |c: &Value| {
            c["permissions"]
                .as_array()
                .unwrap()
                .last()
                .cloned()
                .unwrap()
        };
        assert_eq!(last(&cfg)["action"], "rlord_*");
        // Sends confirmed: asked, after rlord's tools are allowed.
        let cfg = overlord_config("/bin/rlord", "/s", &[], &users, true);
        assert_eq!(last(&cfg)["action"], "rlord_resume_after_limit");
        assert_eq!(last(&cfg)["effect"], "ask");
        let rules = cfg["permissions"].as_array().unwrap();
        assert_eq!(rules[rules.len() - 2]["action"], "rlord_send_to_session");
        assert_eq!(cfg["agents"]["mine"]["permissions"], cfg["permissions"]);
    }

    /// OpenCode 2.2.7's screens, as captured (the sidebar cut off).
    #[test]
    fn a_dialog_is_told_from_the_prompt_box() {
        let idle = "  ┃  $ touch probe.txt\n  ┃  The user declined this tool call\n\n\
                 Build · Big Pickle · 31.7s · interrupted\n\n\n\
              ┃\n  ┃\n  ┃  Build · Big Pickle OpenCode\n  ╹▀▀▀▀▀▀▀▀▀▀\n\
              /tmp/proj  5.5K (3%)  ctrl+p commands\n";
        assert!(!dialog_shown(idle));
        let permission = "  ┃  ⠴ touch probe.txt\n\n\
                 Press ctrl+b to move running work to the background\n\n\n\
              ┃\n  ┃  △ Permission required\n  ┃\n  ┃  $ touch probe.txt\n  ┃\n\
              ┃   Allow once   Always allow   Reject           ctrl+f fullscreen  ⇆ select  enter confirm\n\
              ┃                                                  /tmp/proj\n";
        assert!(dialog_shown(permission));
        let question = "     → Asked 1 question\n\n\
              ┃\n  ┃  Questions\n  ┃\n  ┃  Which colour would you like to pick?\n  ┃\n\
              ┃  1. Red\n  ┃     Choose red\n  ┃  2. Green\n  ┃     Choose green\n\
              ┃  3. Type your own answer\n  ┃\n  ┃  ↑↓ select  enter submit  esc dismiss\n\
              ┃                                                  /tmp/proj\n";
        assert!(dialog_shown(question));
        for s in [permission, question] {
            assert!(!crate::agents::Agent::OpenCode.at_prompt(s));
        }
        // The same words in the transcript above the prompt box (a reply
        // quoting them, the user's own prompt): not a dialog.
        let quoted = "  ┃  △ Permission required\n  ┃  Allow once   Always allow   Reject\n\
              ┃  ↑↓ select  enter submit  esc dismiss\n\n\
                 Build · Big Pickle · 3.1s\n\n\n";
        let quoted = format!("{quoted}{idle}");
        assert!(!dialog_shown(&quoted));
        // One mark alone in the box isn't enough: "Reject" in a draft.
        let draft = idle.replace(
            "┃  Build · Big Pickle OpenCode",
            "┃  Reject the PR. Build · Big Pickle",
        );
        assert!(!dialog_shown(&draft));
        assert!(crate::agents::Agent::OpenCode.at_prompt(idle));
    }

    pub(crate) const SES: &str = "ses_e3192cfeeffeUElN5gOL3f5nhg";

    /// A database as OpenCode 2.2.7 keeps one (the columns rlord reads),
    /// with a session shaped like a real export: a prompt, tool calls with
    /// reasoning, the answer, the `idle` row that ends the turn.
    pub(crate) fn store(dir: &Path) -> PathBuf {
        let db = dir.join("opencode.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE session_v2 (id text PRIMARY KEY, project_id text NOT NULL, \
               parent_id text, fork_session_id text, directory text NOT NULL, title text, \
               tokens_input integer DEFAULT 0 NOT NULL, tokens_output integer DEFAULT 0 NOT NULL, \
               model text, time_created integer NOT NULL, time_updated integer NOT NULL, \
               time_busy integer, time_idle integer, time_archived integer);
             CREATE TABLE session_message (id text PRIMARY KEY, session_id text NOT NULL, \
               type text NOT NULL, seq integer NOT NULL, time_created integer NOT NULL, \
               time_updated integer NOT NULL, data text NOT NULL);
             CREATE TABLE credential (id text PRIMARY KEY, secret text);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO session_v2 VALUES (?1, 'p', NULL, NULL, '/src/app', 'Repo purpose walkthrough', \
             18736, 1545, ?2, 1794464624674, 1794464641936, 1794464624706, 1794464641938, NULL)",
            rusqlite::params![SES, json!({"id": "deepseek-v4.1-flash-free", "providerID": "opencode"}).to_string()],
        )
        .unwrap();
        let model = json!({"id": "deepseek-v4.1-flash-free", "providerID": "opencode", "variant": "default"});
        let tool = |name: &str, input: Value, output: &str| {
            json!({"type": "tool", "id": "call_1", "name": name, "executed": false,
                   "state": {"status": "completed", "input": input,
                             "content": [{"type": "text", "text": output}],
                             "metadata": {"exit": 0}}})
        };
        let rows = [
            (
                4,
                "user",
                1794464624711_i64,
                json!({"time": {"created": 1794464624711_i64},
                "text": "take a look at this repo and describe what it does", "files": [], "agents": []}),
            ),
            (
                7,
                "assistant",
                1794464624720,
                json!({"agent": "build", "model": model,
                "content": [{"type": "text", "text": "I'll explore the repo to understand what it does."},
                            tool("shell", json!({"command": "ls -la"}), "total 616\nCLAUDE.md\nMakefile")],
                "finish": "tool-calls",
                "tokens": {"input": 900, "output": 40, "reasoning": 0, "cache": {"read": 0, "write": 0}}}),
            ),
            (
                31,
                "assistant",
                1794464627768,
                json!({"agent": "build", "model": model,
                "content": [{"type": "reasoning", "text": "Let me look at the structure a bit more."},
                            tool("read", json!({"path": "README.md"}), "# wait2watch"),
                            tool("glob", json!({"pattern": "apps/*"}), "apps/web\napps/worker")],
                "finish": "tool-calls",
                "tokens": {"input": 1200, "output": 60, "reasoning": 12, "cache": {"read": 9000, "write": 0}}}),
            ),
            (
                81,
                "assistant",
                1794464634808,
                json!({"agent": "build", "model": model,
                "content": [{"type": "reasoning", "text": "I have enough to describe the repo well."},
                            {"type": "text", "text": "The repo tracks where films are streaming and notifies you."}],
                "finish": "stop",
                "tokens": {"input": 1517, "output": 880, "reasoning": 18, "cache": {"read": 20736, "write": 0}}}),
            ),
            (
                88,
                "idle",
                1794464641938,
                json!({"time": {"created": 1794464641938_i64}, "outcome": "succeeded"}),
            ),
        ];
        for (seq, kind, at, data) in rows {
            c.execute(
                "INSERT INTO session_message VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
                rusqlite::params![format!("msg_{seq}"), SES, kind, seq, at, data.to_string()],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn minted_ids_have_opencodes_shape() {
        // The time part of a real id: its session was created at …624674.
        let id = mint_at(1794464624657, &[7; 16]);
        assert!(id.starts_with("ses_e3192cfeeffe"), "{id}");
        assert_eq!(id.len(), "ses_".len() + 12 + 14);
        assert!(is_session_id(&id));
        let (a, b) = (mint_session_id(), mint_session_id());
        assert!(is_session_id(&a) && a.len() == 30);
        assert_ne!(a, b);
        // Newer ids sort first.
        assert!(mint_at(2_000, &[0; 16]) < mint_at(1_000, &[0; 16]));
        for bad in ["", "ses_", "e3192cfeeffe", "ses_../x", "ses_a b", "msg_abc"] {
            assert!(!is_session_id(bad), "{bad}");
        }
    }

    /// Run a launch's command with `bin` (a stand-in script) as OpenCode.
    fn run_launch(bin: &Path, argv: &[String]) -> std::process::Output {
        let mut argv = argv.to_vec();
        argv[3] = bin.to_string_lossy().into_owned();
        std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .unwrap()
    }

    #[test]
    fn a_launch_creates_then_opens() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("opencode");
        let log = dir.path().join("log");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\n[ \"$1\" = api ] && exit 0\necho TUI \"$@\"\n",
                log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let argv = launch_argv("ses_1a", "/src/app", None, Some("it's \"quoted\""), &[]);
        let out = run_launch(&bin, &argv);
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "TUI -s ses_1a --prompt it's \"quoted\"\n"
        );
        assert!(out.stderr.is_empty(), "nothing said when it worked");
        let asked = std::fs::read_to_string(&log).unwrap();
        assert!(
            asked.starts_with(r#"api POST /api/session -d {"id":"ses_1a","#),
            "{asked}"
        );
    }

    #[test]
    fn argv() {
        assert_eq!(
            open_argv("ses_1a", Some("hello"), &[]),
            ["opencode", "-s", "ses_1a", "--prompt", "hello"]
        );
        assert_eq!(
            open_argv("ses_1a", Some("  "), &["--auto".to_string()]),
            ["opencode", "-s", "ses_1a", "--auto"]
        );
        // A launch creates the session (with its model) and then opens it.
        let l = launch_argv(
            "ses_1a",
            "/src/app",
            Some("opencode/x-free"),
            Some("hi"),
            &[],
        );
        assert_eq!(l[..2], ["sh", "-c"]);
        assert!(l[2].contains(r#""$0" api POST /api/session -d "$1""#) && l[2].contains("exec"));
        assert_eq!(l[3], "opencode");
        assert_eq!(
            serde_json::from_str::<Value>(&l[4]).unwrap(),
            json!({"id": "ses_1a", "location": {"directory": "/src/app"},
                   "model": {"providerID": "opencode", "id": "x-free"}})
        );
        assert_eq!(l[5..], ["-s", "ses_1a", "--prompt", "hi"]);
        // The create step's bounds are the ones the daemon allows for.
        assert!(l[2].contains(&format!("-ge {} ]", CREATE_LIMIT_S * 10)));
        assert!(l[2].contains(&format!("over {CREATE_LIMIT_S}s")));
        assert!(l[2].contains(&format!("sleep {CREATE_PAUSE_S}\n")));
        // No model, or a name that isn't one: created on OpenCode's default.
        assert_eq!(
            create_body("ses_1a", "/src/app", Some("opus")),
            json!({"id": "ses_1a", "location": {"directory": "/src/app"}})
        );
        let a = summary_argv("opencode/x-free", Some("ses_1a"), "ses_2b", "Summarize");
        assert_eq!(
            a,
            [
                "opencode",
                "run",
                "-s",
                "ses_1a",
                "--fork",
                "--archived",
                "-m",
                "opencode/x-free",
                "Summarize"
            ]
        );
        let b = summary_argv("", None, "ses_2b", "Summarize");
        assert_eq!(
            b,
            ["opencode", "run", "-s", "ses_2b", "--archived", "Summarize"]
        );
        // Never auto-approving, never a private server.
        for argv in [&a, &b] {
            assert!(!argv.iter().any(|x| x == "--auto" || x == "--standalone"));
        }
    }

    /// The overlord's folder config: rlord's server, and rules that end
    /// with its tools allowed after everything was set to ask.
    #[test]
    fn overlord_config_allows_only_what_it_names() {
        let cfg = overlord_config("/bin/rlord", "/run/d.sock", &["rlord-tidy"], &[], false);
        let server = &cfg["mcp"]["servers"]["rlord"];
        assert_eq!(server["type"], "local");
        assert_eq!(
            server["command"],
            serde_json::json!(["/bin/rlord", "mcp", "--socket", "/run/d.sock"])
        );
        // The caller's id isn't inherited from whichever pane started
        // OpenCode's shared server.
        assert_eq!(server["environment"]["RLORD_SESSION_ID"], "");
        let rules = cfg["permissions"].as_array().unwrap();
        // The last matching rule wins: "ask" for everything comes first.
        assert_eq!(
            rules[0],
            serde_json::json!({"action": "*", "resource": "*", "effect": "ask"})
        );
        let effect = |action: &str| {
            rules
                .iter()
                .rev()
                .find(|r| r["action"] == action)
                .map(|r| r["effect"].as_str().unwrap().to_string())
        };
        assert_eq!(effect("rlord_*").as_deref(), Some("allow"));
        assert_eq!(effect("read").as_deref(), Some("allow"));
        // Nothing that writes or runs commands is named: it asks. Code Mode
        // (`execute`) too.
        for a in ["shell", "edit", "webfetch", "external_directory", "execute"] {
            assert_eq!(effect(a), None, "{a}");
        }
        assert!(rules.iter().all(|r| r["effect"] != "deny"));
        // Only rlord's own sub-agents start unasked.
        let sub: Vec<&Value> = rules.iter().filter(|r| r["action"] == "subagent").collect();
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0]["resource"], "rlord-tidy");
        // The same rules at agent level (applied after every file's
        // top-level rules), for the overlord's agents and rlord's.
        for a in ["build", "plan", "rlord-tidy"] {
            assert_eq!(cfg["agents"][a]["permissions"], cfg["permissions"], "{a}");
        }
    }

    #[test]
    fn a_subagent_file_has_its_description_up_front() {
        let md = subagent_md("Tidy: names, \"tags\"", "Do the pass.\n");
        assert!(md.starts_with("---\ndescription: \"Tidy: names, \\\"tags\\\"\"\nmode: subagent\n---\n\nDo the pass.\n"));
    }

    #[test]
    fn models_are_named_provider_slash_model() {
        assert_eq!(
            model_ref("opencode/glm-5.3-flash-free"),
            Some(json!({"providerID": "opencode", "id": "glm-5.3-flash-free"}))
        );
        assert_eq!(
            model_ref(" openrouter/anthropic/claude#high "),
            Some(json!({"providerID": "openrouter", "id": "anthropic/claude", "variant": "high"}))
        );
        assert_eq!(model_ref("opus"), None);
        assert_eq!(model_ref("/x"), None);
        let out = "opencode/beam-free\nopencode/deepseek-v4.1-flash-free\n\nsome notice here\n";
        assert_eq!(
            parse_models(out),
            ["opencode/beam-free", "opencode/deepseek-v4.1-flash-free"]
        );
    }

    #[test]
    fn version_two_is_required() {
        assert_eq!(major("opencode v2.2.7"), Some(2));
        assert_eq!(major("1.4.3"), Some(1));
        assert_eq!(major("opencode"), None);
        assert_eq!(too_old("opencode v2.2.7"), None);
        assert_eq!(too_old("v3.0.0"), None);
        assert!(too_old("1.4.3").unwrap().contains("v2"));
        // Unknown: not refused.
        assert_eq!(too_old("dev build"), None);
    }

    #[test]
    fn a_path_names_the_store_and_the_conversation() {
        let p = PathBuf::from("/h/.local/share/opencode/opencode.db").join(SES);
        assert_eq!(
            locate(&p),
            Some((Path::new("/h/.local/share/opencode/opencode.db"), SES))
        );
        for other in [
            "/h/.claude/projects/-x/abc.jsonl",
            "/h/.codex/sessions/2026/11/11/rollout-x.jsonl",
            "/h/opencode.db/notes.txt",
            "/h/other.db/ses_abc",
        ] {
            assert_eq!(locate(Path::new(other)), None, "{other}");
        }
    }

    #[test]
    fn rows_read_as_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        let all = conversation(&db, SES);
        let who: Vec<&str> = all.iter().map(|m| m.who).collect();
        // Not the reasoning, nor the `idle` row.
        assert_eq!(
            who,
            ["you", "agent", "agent", "tool", "agent", "tool", "agent", "tool", "agent"]
        );
        assert_eq!(all[2].text, "[shell] ls -la");
        assert_eq!(all[3].text, "total 616\nCLAUDE.md\nMakefile");
        assert_eq!(all[4].text, "[read] README.md");
        assert_eq!(all[6].text, "[glob] apps/*");
        assert_eq!((all[0].seq, all[1].seq, all[8].seq), (4, 7, 81));
        assert_eq!(
            super::super::transcript::parse_iso_ms(&iso_ms(all[0].at_ms)),
            Some(all[0].at_ms)
        );
        assert_eq!(iso_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_ms(1_789_158_047_939), "2026-09-11T20:20:47.939Z");
        assert!(conversation(&db, "ses_none").is_empty());
        assert!(conversation(&dir.path().join("absent.db"), SES).is_empty());
    }

    #[test]
    fn excerpts_stats_model_and_turn_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        let e = excerpts(&db, SES);
        assert_eq!(
            e.first_prompt.as_deref(),
            Some("take a look at this repo and describe what it does")
        );
        assert_eq!(e.last_prompt, None, "one prompt so far: not repeated");
        assert_eq!(
            e.last_reply.as_deref(),
            Some("The repo tracks where films are streaming and notifies you.")
        );
        assert_eq!(e.title.as_deref(), Some("Repo purpose walkthrough"));
        let st = stats(&db, SES).unwrap();
        assert_eq!((st.input_tokens, st.output_tokens), (18736, 1545));
        assert!(st.ended && st.turn_seen);
        assert_eq!(st.last.unwrap() - st.first.unwrap(), 17_264);
        assert_eq!(context_tokens(&db, SES), 1517 + 20736);
        assert_eq!(
            observed_model(&db, SES).as_deref(),
            Some("opencode/deepseek-v4.1-flash-free")
        );
        // The Usage column: one prompt, the row's totals, the last request's
        // context; a model with no known window.
        let u = crate::usage::of(&db.join(SES), None).unwrap();
        assert_eq!(
            (u.turns, u.tokens, u.context),
            (1, 18736 + 1545, 1517 + 20736)
        );
        assert_eq!(u.window, None);
        // Asked again with nothing changed: the cached answer.
        assert_eq!(crate::usage::of(&db.join(SES), None), Some(u.clone()));
        // A list's rows: one connection; sessions not there are left out.
        let many = crate::usage::of_opencode(&db, &[(SES, None), ("ses_none", None)]);
        assert_eq!((many.get(SES), many.len()), (Some(&u), 1));
        assert_eq!(
            (exists(&db, SES), exists(&db, "ses_none")),
            (Some(true), Some(false))
        );
        // No database: OpenCode has no sessions. One that isn't a database
        // can't tell.
        assert_eq!(exists(&dir.path().join("absent.db"), SES), Some(false));
        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, "not a database").unwrap();
        assert_eq!(exists(&junk, SES), None);
        assert_eq!(directory(&db, SES).as_deref(), Some("/src/app"));
        assert_eq!(stats(&db, "ses_none"), None);

        // A second prompt: a turn is running again.
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute(
            "INSERT INTO session_message VALUES ('msg_95', ?1, 'user', 95, 1794464700000, \
             1794464700000, ?2)",
            rusqlite::params![SES, json!({"text": "now add a retry budget"}).to_string()],
        )
        .unwrap();
        c.execute(
            "UPDATE session_v2 SET time_busy = 1794464700001, time_updated = 1794464700000",
            [],
        )
        .unwrap();
        let st = stats(&db, SES).unwrap();
        assert!(!st.ended && st.turn_seen);
        assert!(turn(&db, SES).unwrap().working());
        assert_eq!(turn(&db, "ses_none"), None);
        // Created, nothing asked yet: no turn.
        let fresh = Turn {
            busy: None,
            idle: None,
        };
        assert!(!fresh.working());
        assert_eq!(
            excerpts(&db, SES).last_prompt.as_deref(),
            Some("now add a retry budget")
        );
        // A child session and a fork.
        c.execute_batch(
            "INSERT INTO session_v2 (id, project_id, parent_id, directory, time_created, time_updated) \
               VALUES ('ses_child', 'p', 'ses_e3192cfeeffeUElN5gOL3f5nhg', '/src/app', 5, 5);
             INSERT INTO session_v2 (id, project_id, fork_session_id, directory, time_created, time_updated, time_archived) \
               VALUES ('ses_fork', 'p', 'ses_e3192cfeeffeUElN5gOL3f5nhg', '/src/app', 9000, 9000, 9000);",
        )
        .unwrap();
        assert_eq!(parent(&db, "ses_child"), Some(Some(SES.to_string())));
        // No parent is an answer; a session the database doesn't list, or
        // a database that can't be read, is not.
        assert_eq!(parent(&db, SES), Some(None));
        assert_eq!(parent(&db, "ses_unknown"), None);
        assert_eq!(parent(&dir.path().join("absent.db"), SES), None);
        c.execute_batch(
            "INSERT INTO session_v2 (id, project_id, parent_id, directory, time_created, time_updated) \
               VALUES ('ses_grandchild', 'p', 'ses_child', '/src/app', 6, 6);
             UPDATE session_v2 SET time_busy = 8, time_idle = 7 WHERE id = 'ses_grandchild';",
        )
        .unwrap();
        let under = descendants(&db, SES);
        let ids: Vec<&str> = under.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["ses_child", "ses_grandchild"]);
        assert!(!under[0].1.working() && under[1].1.working());
        assert!(descendants(&db, "ses_grandchild").is_empty());
        assert_eq!(
            session_model(&db, SES).as_deref(),
            Some("opencode/deepseek-v4.1-flash-free")
        );
        assert_eq!(session_model(&db, "ses_child"), None);
        assert_eq!(fork_of(&db, SES, 8000).as_deref(), Some("ses_fork"));
        assert_eq!(fork_of(&db, SES, 9001), None);
        // A fork the user made (not archived) isn't the summary's.
        c.execute(
            "UPDATE session_v2 SET time_archived = NULL WHERE id = 'ses_fork'",
            [],
        )
        .unwrap();
        assert_eq!(fork_of(&db, SES, 8000), None);
        let mut ids: Vec<String> = sessions(&db).into_iter().map(|(_, id)| id).collect();
        ids.sort();
        assert_eq!(ids, [SES, "ses_fork"], "not the child session");
    }

    #[test]
    fn search_needs_every_word_in_one_message() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        let ws = |q: &str| -> Vec<String> { q.split_whitespace().map(str::to_string).collect() };
        let (hits, total) = search(&db, SES, &ws("repo describe"), Some("describe"), 5);
        assert_eq!((hits.len(), total), (1, 1));
        assert_eq!((hits[0].who, hits[0].seq), ("you", 4));
        let (hits, total) = search(&db, SES, &ws("repo"), Some("repo"), 2);
        assert_eq!((hits.len(), total), (2, 3));
        assert_eq!((hits[1].who, hits[1].seq), ("agent", 7));
        // A tool's label and its output are searched; the count goes on
        // past what is kept.
        assert_eq!(search(&db, SES, &ws("[shell]"), None, 5).1, 1);
        assert_eq!(search(&db, SES, &ws("apps"), Some("apps"), 1).1, 2);
        assert_eq!(search(&db, SES, &ws("apps"), Some("apps"), 1).0.len(), 1);
        assert_eq!(
            search(&db, SES, &ws("repo nothing"), Some("nothing"), 5).1,
            0
        );
        // `%` and `_` in a word are themselves, not wildcards.
        assert_eq!(search(&db, SES, &ws("100%"), Some("100%"), 5).1, 0);
        assert_eq!(search(&db, SES, &ws("r_po"), Some("r_po"), 5).1, 0);
        assert_eq!(like_pattern("50%_a\\b"), "%50\\%\\_a\\\\\\\\b%");
        // A word is looked for as JSON writes it: quotes and line breaks
        // are escaped in the stored row.
        assert_eq!(like_pattern("\"hi\""), "%\\\\\"hi\\\\\"%");
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute(
            "INSERT INTO session_message VALUES ('msg_99', ?1, 'user', 99, 5, 5, ?2)",
            rusqlite::params![
                SES,
                json!({"text": "rename it to \"wait_2_watch\" at 100%\nplease"}).to_string()
            ],
        )
        .unwrap();
        for word in ["\"wait_2_watch\"", "100%", "wait_2_watch"] {
            let (hits, total) = search(&db, SES, &ws(word), Some(word), 5);
            assert_eq!((total, hits[0].seq), (1, 99), "{word}");
        }
        assert_eq!(
            search(&db, SES, &ws("waitx2xwatch"), Some("wait_2_watch"), 5).1,
            0
        );
    }

    /// A Go-limit refusal as OpenCode 2.2.7 records it, hand-written from
    /// its source (no real one to copy): the error is
    /// `core/test/session-error.test.ts`'s (`provider.go-limit`, the 429's
    /// `retry-after` as `retryAfterMs`), set on the assistant row by
    /// `session.step.failed` (`core/src/session/message-updater.ts`:
    /// `finish: "error"`, `time.completed`), and the turn closed by an
    /// `idle` row with `outcome: "failed"`.
    fn go_limit_rows(c: &rusqlite::Connection) {
        let body = json!({"type": "error",
            "error": {"type": "GoUsageLimitError", "message": "Go usage limit exceeded"},
            "metadata": {"workspace": "wrk_go_limit", "limitName": "5 hour"}});
        let error = json!({"type": "provider.go-limit",
            "message": "OpenCode Go 5 hour usage limit reached. Switch to a free model, or turn on Extra Usage on the Go page in OpenCode Console to keep going.",
            "status": 429, "response": {"body": body.to_string()}, "retryAfterMs": 3_600_000});
        let rows = [
            (
                95,
                "user",
                json!({"time": {"created": 1794470000000_i64}, "text": "and the worker?",
                "files": [], "agents": []}),
            ),
            (
                96,
                "assistant",
                json!({"time": {"created": 1794470000010_i64, "completed": 1794470000900_i64},
                "agent": "build", "model": {"id": "kimi-k3", "providerID": "opencode-go"},
                "content": [], "finish": "error", "error": error}),
            ),
            (
                99,
                "idle",
                json!({"time": {"created": 1794470000905_i64}, "outcome": "failed",
                "error": error}),
            ),
        ];
        for (seq, kind, data) in rows {
            c.execute(
                "INSERT INTO session_message VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
                rusqlite::params![
                    format!("msg_{seq}"),
                    SES,
                    kind,
                    seq,
                    1794470000000_i64,
                    data.to_string()
                ],
            )
            .unwrap();
        }
    }

    #[test]
    fn an_opencode_usage_limit_is_read_from_the_failed_step() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        // A turn that ended well: no limit.
        assert_eq!(limit_hit(&db, SES), None);
        let c = rusqlite::Connection::open(&db).unwrap();
        go_limit_rows(&c);
        let hit = limit_hit(&db, SES).unwrap();
        assert_eq!(hit.at, 1794470000900);
        assert_eq!(hit.resets_at, 1794470000900 + 3_600_000);
        assert_eq!(hit.kind, "go-limit");
        assert!(!hit.touched);
        assert_eq!(hit.key, crate::agents::transcript::fnv("msg_96"));
        // Through the transcript path rlord keeps for it.
        assert_eq!(
            crate::agents::transcript::limit_hit(&db.join(SES)),
            Some(hit)
        );
        // A prompt after it touches it.
        c.execute(
            "INSERT INTO session_message VALUES ('msg_100', ?1, 'user', 100, 5, 5, ?2)",
            rusqlite::params![
                SES,
                json!({"time": {"created": 5}, "text": "continue"}).to_string()
            ],
        )
        .unwrap();
        assert!(limit_hit(&db, SES).unwrap().touched);
        // An answer after it (a newer assistant row) ends it.
        c.execute(
            "INSERT INTO session_message VALUES ('msg_101', ?1, 'assistant', 101, 6, 6, ?2)",
            rusqlite::params![
                SES,
                json!({"time": {"created": 6, "completed": 7}, "finish": "stop",
                "content": [{"type": "text", "text": "Carrying on."}]})
                .to_string()
            ],
        )
        .unwrap();
        assert_eq!(limit_hit(&db, SES), None);
    }

    #[test]
    fn opencode_limits_reset_as_its_own_timeline_says() {
        let row =
            |error: Value| json!({"time": {"created": 1_000, "completed": 2_000}, "error": error});
        // The free limit: `retryAfterMs` after the step ended
        // (`core/test/session-error.test.ts`).
        let free = json!({"type": "provider.free-limit", "message": "Free usage limit reached.",
            "status": 429, "retryAfterMs": 3_600_000});
        assert_eq!(
            usage_limit(&row(free.clone())),
            Some((2_000, 3_602_000, "free-limit".into()))
        );
        // Past OpenCode's own 45-day bound, or absent: no reset known.
        let mut far = free.clone();
        far["retryAfterMs"] = json!(46 * 86_400_000_i64);
        assert_eq!(usage_limit(&row(far)), None);
        let mut none = free;
        none.as_object_mut().unwrap().remove("retryAfterMs");
        assert_eq!(usage_limit(&row(none)), None);
        // Budgets: `resetsAt` in either envelope
        // (`session-ui/src/timeline/rows-current.test.ts`).
        let budget = |kind: &str, body: &str| {
            row(
                json!({"type": kind, "message": "Budget reached.", "status": 429,
                "response": {"body": body}}),
            )
        };
        let reset = crate::agents::transcript::parse_iso_ms("2026-11-01T00:00:00.000Z").unwrap();
        assert_eq!(
            usage_limit(&budget(
                "provider.model-budget-limit",
                r#"{"type":"error","error":{"type":"BudgetLimitError","message":"A budget policy for this model has been reached."},"metadata":{"coverage":"models","resetsAt":"2026-11-01T00:00:00.000Z"}}"#
            )),
            Some((2_000, reset, "model-budget-limit".into()))
        );
        assert_eq!(
            usage_limit(&budget(
                "provider.budget-limit",
                r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Budget reached.","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"BudgetLimitError","domain":"opencode.ai","metadata":{"coverage":"all","resetsAt":"2026-11-01T00:00:00.000Z"}}]}}"#
            )),
            Some((2_000, reset, "budget-limit".into()))
        );
        // Throttles OpenCode retries, and limits with no reset: not these.
        for kind in [
            "provider.rate-limit",
            "provider.chatgpt-limit",
            "provider.quota",
            "provider.transport",
        ] {
            let e = json!({"type": kind, "message": "x", "status": 429, "retryAfterMs": 45_000});
            assert_eq!(usage_limit(&row(e)), None, "{kind}");
        }
    }
}
