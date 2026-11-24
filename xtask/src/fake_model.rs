//! `cargo xtask fake-model [--port N] [--delay-ms MS] [--dump DIR]`: a
//! stand-in for the Anthropic Messages API on 127.0.0.1, so the demo
//! (`scripts/demo/demo.sh --real-claude`) can run the real Claude Code CLI
//! with no model behind it. Test tooling, never shipped.
//!
//! It answers, deterministically:
//!
//! - `POST /v1/messages` (streamed as SSE, or one JSON message): a prompt
//!   whose last user text has the word "tool" in it, in a request that
//!   offers the `Bash` tool, gets a `tool_use` of Bash writing a small file
//!   in the session's folder (not read-only, so Claude Code asks first);
//!   the request carrying its `tool_result` gets a closing line quoting it.
//!   A request with no tools (titles, summaries, a quota check) gets one
//!   short line. Anything else gets a few sentences echoing the prompt.
//!   Streams go out a few words at a time, `--delay-ms` apart (default
//!   60), so spinners and redraws show.
//! - `POST /v1/messages/count_tokens`: a rough count (bytes / 4).
//! - `GET /v1/models` (and `/v1/models/<id>`): a short list.
//! - Anything else: 404 with a JSON error naming the path, logged loudly.
//!   A proxy request (`CONNECT host:443`, or an absolute URL), from a
//!   client that went out through `HTTPS_PROXY` pointed here, is refused
//!   (403) and logged as BLOCKED: the demo's wrapper points the proxy here
//!   so nothing it sends can leave the machine unnoticed.
//!
//! Every request is logged to stderr: method, path, model, how it
//! authenticated (whether it carried the demo's dummy key or something
//! else) and a short preview. `--dump DIR` also saves each request body.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::cmd;

/// The API key the demo's `claude` wrapper sends (`demo_claude`): a request
/// carrying anything else is logged as such.
pub const DUMMY_KEY: &str = "sk-ant-fake-model-demo-0000000000000000";

static SEQ: AtomicU64 = AtomicU64::new(1);

struct Opts {
    delay: Duration,
    dump: Option<PathBuf>,
}

pub fn main(args: &[String]) -> Result<()> {
    let mut port = 0u16;
    let mut delay = 60u64;
    let mut dump = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| cmd::Usage(format!("{name} needs a value")))
        };
        match a.as_str() {
            "--port" => port = val("--port")?.parse().context("--port")?,
            "--delay-ms" => delay = val("--delay-ms")?.parse().context("--delay-ms")?,
            "--dump" => dump = Some(PathBuf::from(val("--dump")?)),
            _ => {
                return cmd::usage(
                    "usage: cargo xtask fake-model [--port N] [--delay-ms MS] [--dump DIR]",
                );
            }
        }
    }
    if let Some(d) = &dump {
        std::fs::create_dir_all(d).with_context(|| d.display().to_string())?;
    }
    let listener = TcpListener::bind(("127.0.0.1", port)).context("bind 127.0.0.1")?;
    let addr = listener.local_addr()?;
    println!("listening on {addr}");
    std::io::stdout().flush()?;
    let opts: &'static Opts = Box::leak(Box::new(Opts {
        delay: Duration::from_millis(delay),
        dump,
    }));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        std::thread::spawn(move || {
            if let Err(e) = serve(conn, opts) {
                log(&format!("connection error: {e:#}"));
            }
        });
    }
    Ok(())
}

fn log(line: &str) {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let _ = writeln!(
        std::io::stderr(),
        "[{}.{:03}] {line}",
        t.as_secs(),
        t.subsec_millis()
    );
}

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One HTTP/1.1 request off `r`; None at a clean end of the connection.
fn read_request(r: &mut impl BufRead) -> Result<Option<Request>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim_end().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut req = Request {
        method,
        target,
        headers,
        body: Vec::new(),
    };
    if req
        .header("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        loop {
            let mut size = String::new();
            r.read_line(&mut size)?;
            let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)
                .context("chunk size")?;
            let mut chunk = vec![0; n + 2];
            r.read_exact(&mut chunk)?;
            if n == 0 {
                break;
            }
            req.body.extend_from_slice(&chunk[..n]);
        }
    } else if let Some(n) = req.header("content-length") {
        let n: usize = n.parse().context("content-length")?;
        req.body = vec![0; n];
        r.read_exact(&mut req.body)?;
    }
    Ok(Some(req))
}

fn serve(conn: TcpStream, opts: &Opts) -> Result<()> {
    let mut out = conn.try_clone()?;
    let mut r = BufReader::new(conn);
    while let Some(req) = read_request(&mut r)? {
        let close = req
            .header("connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"));
        handle(&req, &mut out, opts)?;
        if close {
            break;
        }
    }
    Ok(())
}

fn auth_note(req: &Request) -> String {
    match (req.header("x-api-key"), req.header("authorization")) {
        (Some(DUMMY_KEY), None) => "auth=dummy-key".into(),
        (None, None) => "auth=none".into(),
        (k, a) => format!(
            "auth=UNEXPECTED(x-api-key {}, authorization {})",
            if k.is_some() { "set" } else { "unset" },
            if a.is_some() { "set" } else { "unset" }
        ),
    }
}

fn handle(req: &Request, out: &mut TcpStream, opts: &Opts) -> Result<()> {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = req.target.split('?').next().unwrap_or_default();
    if req.method == "CONNECT" || !req.target.starts_with('/') {
        log(&format!(
            "BLOCKED outbound {} {} (a proxied request: refused)",
            req.method, req.target
        ));
        return respond_json(
            out,
            403,
            &error_body("permission_error", "fake model: outbound traffic refused"),
        );
    }
    if let Some(d) = &opts.dump {
        let _ = std::fs::write(d.join(format!("{n:05}.json")), &req.body);
    }
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let model = body["model"].as_str().unwrap_or("-");
    let note = auth_note(req);
    match (req.method.as_str(), path) {
        ("POST", "/v1/messages") => {
            let reply = choose_reply(&body);
            let stream = body["stream"] == true;
            log(&format!(
                "#{n} POST {} model={model} {note} stream={stream} tools={} last={:?} -> {}",
                req.target,
                body["tools"].as_array().map_or(0, Vec::len),
                preview(&last_user_text(&body), 60),
                reply.kind()
            ));
            let usage = Usage {
                input: (req.body.len() / 4) as u64,
                output: reply.output_tokens(),
            };
            let id = format!("msg_fake_{n:05}");
            let tool_id = format!("toolu_fake_{n:05}");
            if stream {
                stream_reply(out, &sse_events(&reply, model, &id, &tool_id, usage), opts)
            } else {
                respond_json(out, 200, &message_json(&reply, model, &id, &tool_id, usage))
            }
        }
        ("POST", "/v1/messages/count_tokens") => {
            log(&format!("#{n} POST {} model={model} {note}", req.target));
            respond_json(out, 200, &json!({"input_tokens": req.body.len() / 4}))
        }
        ("GET", "/v1/models") => {
            log(&format!("#{n} GET {} {note}", req.target));
            respond_json(out, 200, &models_json())
        }
        ("GET", p) if p.starts_with("/v1/models/") => {
            log(&format!("#{n} GET {} {note}", req.target));
            let id = &p["/v1/models/".len()..];
            respond_json(out, 200, &model_json(id))
        }
        (m, _) => {
            log(&format!(
                "#{n} UNKNOWN {m} {} {note} body={:?}  <-- not handled (404)",
                req.target,
                preview(&String::from_utf8_lossy(&req.body), 120)
            ));
            respond_json(
                out,
                404,
                &error_body(
                    "not_found_error",
                    &format!("fake model: no route for {m} {path}"),
                ),
            )
        }
    }
}

fn error_body(kind: &str, message: &str) -> Value {
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    }
}

fn respond_json(out: &mut TcpStream, code: u16, body: &Value) -> Result<()> {
    let body = body.to_string();
    write!(
        out,
        "HTTP/1.1 {code} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         request-id: req_fake\r\n\r\n{body}",
        status_text(code),
        body.len()
    )?;
    out.flush()?;
    Ok(())
}

fn stream_reply(out: &mut TcpStream, events: &[(String, Value)], opts: &Opts) -> Result<()> {
    write!(
        out,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\
         request-id: req_fake\r\ntransfer-encoding: chunked\r\n\r\n"
    )?;
    for (event, data) in events {
        let frame = sse_frame(event, data);
        write!(out, "{:x}\r\n{frame}\r\n", frame.len())?;
        out.flush()?;
        if event == "content_block_delta" {
            std::thread::sleep(opts.delay);
        }
    }
    write!(out, "0\r\n\r\n")?;
    out.flush()?;
    Ok(())
}

/// What the fake model answers.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// Plain text (a normal turn, or a side request's one line).
    Text(String),
    /// A line of text, then a call of tool `name` with `input`.
    ToolUse {
        lead: String,
        name: String,
        input: Value,
    },
}

impl Reply {
    fn kind(&self) -> &'static str {
        match self {
            Reply::Text(_) => "text",
            Reply::ToolUse { .. } => "tool_use",
        }
    }

    fn output_tokens(&self) -> u64 {
        let n = match self {
            Reply::Text(t) => t.len(),
            Reply::ToolUse { lead, input, .. } => lead.len() + input.to_string().len(),
        };
        (n / 4).max(1) as u64
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
}

/// The file the tool turn writes, in the session's folder.
pub const TOOL_FILE: &str = "fake-model-hello.txt";

/// The reply to a Messages request `body` (see the module doc).
pub fn choose_reply(body: &Value) -> Reply {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let last = messages.last().cloned().unwrap_or(Value::Null);
    if let Some(result) = tool_result_text(&last) {
        return Reply::Text(format!(
            "The tool ran and said: \"{}\". That's all: this answer came from rlord's \
             fake model, not a real one.",
            preview(&result, 80)
        ));
    }
    let tools: Vec<&str> = body["tools"]
        .as_array()
        .map(|t| t.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    let text = last_user_text(body);
    if tools.is_empty() {
        return Reply::Text("Fake model session".into());
    }
    if has_word(&text, "tool") && tools.contains(&"Bash") {
        return Reply::ToolUse {
            lead: "Sure, I'll run a harmless command so you can see a tool call.".into(),
            name: "Bash".into(),
            input: json!({
                "command": format!("echo 'hello from the fake model' > {TOOL_FILE}"),
                "description": "Write a greeting from the fake model"
            }),
        };
    }
    let turn = messages.iter().filter(|m| m["role"] == "user").count();
    Reply::Text(format!(
        "This is rlord's fake model (turn {turn}): no real model was called. You said: \
         \"{}\". Ask for a tool to see a tool call and its permission prompt.",
        preview(&text, 60)
    ))
}

/// Whether `text` has `word` as a whole word, any case.
fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .any(|w| w.eq_ignore_ascii_case(word))
}

/// The text of a user message's tool_result blocks, if it has any.
fn tool_result_text(msg: &Value) -> Option<String> {
    if msg["role"] != "user" {
        return None;
    }
    let blocks = msg["content"].as_array()?;
    let mut found = false;
    let mut text = String::new();
    for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
        found = true;
        match &b["content"] {
            Value::String(s) => text.push_str(s),
            Value::Array(parts) => {
                for p in parts {
                    text.push_str(p["text"].as_str().unwrap_or_default());
                }
            }
            _ => {}
        }
    }
    found.then(|| {
        if text.is_empty() {
            "(no output)".into()
        } else {
            text
        }
    })
}

/// The last user message's own text (not system reminders Claude Code
/// adds as separate blocks wrapped in `<system-reminder>`).
fn last_user_text(body: &Value) -> String {
    let Some(msg) = body["messages"]
        .as_array()
        .and_then(|m| m.iter().rev().find(|m| m["role"] == "user"))
    else {
        return String::new();
    };
    match &msg["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .filter(|t| !t.trim_start().starts_with("<system-reminder>"))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn preview(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

/// `text` in pieces of a few words, each piece keeping its trailing space.
pub fn chunks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut words = 0;
    for piece in text.split_inclusive(' ') {
        cur.push_str(piece);
        words += 1;
        if words == 3 {
            out.push(std::mem::take(&mut cur));
            words = 0;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn usage_json(u: Usage) -> Value {
    json!({
        "input_tokens": u.input,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "output_tokens": u.output
    })
}

fn stop_reason(reply: &Reply) -> &'static str {
    match reply {
        Reply::Text(_) => "end_turn",
        Reply::ToolUse { .. } => "tool_use",
    }
}

/// The whole reply as one (non-streamed) Messages response.
pub fn message_json(reply: &Reply, model: &str, id: &str, tool_id: &str, u: Usage) -> Value {
    let content = match reply {
        Reply::Text(t) => json!([{"type": "text", "text": t}]),
        Reply::ToolUse { lead, name, input } => json!([
            {"type": "text", "text": lead},
            {"type": "tool_use", "id": tool_id, "name": name, "input": input}
        ]),
    };
    json!({
        "id": id, "type": "message", "role": "assistant", "model": model,
        "content": content, "stop_reason": stop_reason(reply), "stop_sequence": null,
        "usage": usage_json(u)
    })
}

/// The reply as the Messages API's stream of server-sent events.
pub fn sse_events(
    reply: &Reply,
    model: &str,
    id: &str,
    tool_id: &str,
    u: Usage,
) -> Vec<(String, Value)> {
    let mut ev: Vec<(String, Value)> = Vec::new();
    let mut push = |name: &str, data: Value| ev.push((name.to_string(), data));
    push(
        "message_start",
        json!({"type": "message_start", "message": {
            "id": id, "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": usage_json(Usage { input: u.input, output: 1 })
        }}),
    );
    push("ping", json!({"type": "ping"}));
    let text_block = |push: &mut dyn FnMut(&str, Value), index: usize, text: &str| {
        push(
            "content_block_start",
            json!({"type": "content_block_start", "index": index,
                   "content_block": {"type": "text", "text": ""}}),
        );
        for c in chunks(text) {
            push(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": index,
                       "delta": {"type": "text_delta", "text": c}}),
            );
        }
        push(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        );
    };
    match reply {
        Reply::Text(t) => text_block(&mut push, 0, t),
        Reply::ToolUse { lead, name, input } => {
            text_block(&mut push, 0, lead);
            push(
                "content_block_start",
                json!({"type": "content_block_start", "index": 1,
                       "content_block": {"type": "tool_use", "id": tool_id, "name": name, "input": {}}}),
            );
            let raw = input.to_string();
            let cut = raw
                .char_indices()
                .nth(raw.chars().count() / 2)
                .map_or(0, |(i, _)| i);
            for part in [&raw[..cut], &raw[cut..]] {
                push(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": 1,
                           "delta": {"type": "input_json_delta", "partial_json": part}}),
                );
            }
            push(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            );
        }
    }
    push(
        "message_delta",
        json!({"type": "message_delta",
               "delta": {"stop_reason": stop_reason(reply), "stop_sequence": null},
               "usage": {"output_tokens": u.output}}),
    );
    push("message_stop", json!({"type": "message_stop"}));
    ev
}

/// One server-sent event.
pub fn sse_frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

const MODELS: [(&str, &str); 3] = [
    ("claude-opus-4-5", "Claude Opus 4.5"),
    ("claude-sonnet-4-5", "Claude Sonnet 4.5"),
    ("claude-haiku-4-5", "Claude Haiku 4.5"),
];

fn model_json(id: &str) -> Value {
    let name = MODELS
        .iter()
        .find(|(m, _)| *m == id)
        .map_or(id, |(_, n)| *n);
    json!({"type": "model", "id": id, "display_name": name, "created_at": "2025-01-01T00:00:00Z"})
}

fn models_json() -> Value {
    let data: Vec<Value> = MODELS.iter().map(|(id, _)| model_json(id)).collect();
    json!({"data": data, "has_more": false, "first_id": MODELS[0].0, "last_id": MODELS[2].0})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(text: &str, tools: &[&str]) -> Value {
        let tools: Vec<Value> = tools.iter().map(|t| json!({"name": t})).collect();
        json!({"model": "m", "tools": tools, "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "<system-reminder>use a tool</system-reminder>"},
                {"type": "text", "text": text}
            ]}
        ]})
    }

    #[test]
    fn a_prompt_echoes_back() {
        let Reply::Text(t) = choose_reply(&req("fix the login test", &["Bash"])) else {
            panic!("text expected")
        };
        assert!(t.contains("fix the login test"), "{t}");
        assert!(t.contains("turn 1"), "{t}");
    }

    #[test]
    fn the_word_tool_asks_for_bash_and_reminders_do_not_count() {
        let r = choose_reply(&req("please use a Tool now", &["Read", "Bash"]));
        let Reply::ToolUse { name, input, .. } = r else {
            panic!("tool_use expected")
        };
        assert_eq!(name, "Bash");
        assert!(input["command"].as_str().unwrap().contains(TOOL_FILE));
        // "tools" isn't "tool", and the system reminder's "tool" is ignored.
        assert!(matches!(
            choose_reply(&req("what tools exist", &["Bash"])),
            Reply::Text(_)
        ));
        // No Bash offered: plain text.
        assert!(matches!(
            choose_reply(&req("use a tool", &["Read"])),
            Reply::Text(_)
        ));
    }

    #[test]
    fn a_tool_result_gets_the_closing_line() {
        let body = json!({"tools": [{"name": "Bash"}], "messages": [
            {"role": "user", "content": "use a tool"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t",
                "content": [{"type": "text", "text": "done ok"}]}]}
        ]});
        let Reply::Text(t) = choose_reply(&body) else {
            panic!("text expected")
        };
        assert!(t.contains("done ok"), "{t}");
    }

    #[test]
    fn a_side_request_without_tools_gets_one_line() {
        assert_eq!(
            choose_reply(&req("write a title", &[])),
            Reply::Text("Fake model session".into())
        );
    }

    #[test]
    fn chunks_rejoin_to_the_text() {
        let t = "one two three four five six seven";
        let c = chunks(t);
        assert_eq!(c.concat(), t);
        assert_eq!(c.len(), 3);
        assert!(chunks("").is_empty());
    }

    #[test]
    fn a_text_stream_is_well_formed() {
        let u = Usage {
            input: 10,
            output: 5,
        };
        let ev = sse_events(&Reply::Text("a b c d".into()), "m", "msg", "tu", u);
        let names: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "ping",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(ev[0].1["message"]["id"], "msg");
        assert_eq!(ev[6].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(ev[6].1["usage"]["output_tokens"], 5);
        // Every event's data says its own type.
        assert!(ev.iter().all(|(n, d)| d["type"] == *n));
    }

    #[test]
    fn a_tool_stream_rebuilds_its_input() {
        let input = json!({"command": "echo hi", "description": "d"});
        let r = Reply::ToolUse {
            lead: "ok".into(),
            name: "Bash".into(),
            input: input.clone(),
        };
        let ev = sse_events(
            &r,
            "m",
            "msg",
            "tu",
            Usage {
                input: 1,
                output: 1,
            },
        );
        let start = ev
            .iter()
            .find(|(n, d)| n == "content_block_start" && d["index"] == 1)
            .unwrap();
        assert_eq!(start.1["content_block"]["id"], "tu");
        assert_eq!(start.1["content_block"]["name"], "Bash");
        let json: String = ev
            .iter()
            .filter(|(_, d)| d["delta"]["type"] == "input_json_delta")
            .map(|(_, d)| d["delta"]["partial_json"].as_str().unwrap())
            .collect();
        assert_eq!(serde_json::from_str::<Value>(&json).unwrap(), input);
        let delta = ev.iter().find(|(n, _)| n == "message_delta").unwrap();
        assert_eq!(delta.1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn non_streamed_message_has_both_blocks() {
        let r = Reply::ToolUse {
            lead: "ok".into(),
            name: "Bash".into(),
            input: json!({}),
        };
        let m = message_json(
            &r,
            "m",
            "msg",
            "tu",
            Usage {
                input: 1,
                output: 1,
            },
        );
        assert_eq!(m["content"][1]["type"], "tool_use");
        assert_eq!(m["stop_reason"], "tool_use");
    }

    #[test]
    fn frames_are_sse() {
        assert_eq!(
            sse_frame("ping", &json!({"type": "ping"})),
            "event: ping\ndata: {\"type\":\"ping\"}\n\n"
        );
    }

    #[test]
    fn requests_parse_with_length_or_chunks() {
        let raw = b"POST /v1/messages?beta=true HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}\
                    POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n1\r\nc\r\n0\r\n\r\n";
        let mut r = BufReader::new(&raw[..]);
        let a = read_request(&mut r).unwrap().unwrap();
        assert_eq!(
            (a.method.as_str(), a.target.as_str()),
            ("POST", "/v1/messages?beta=true")
        );
        assert_eq!(a.body, b"{}");
        let b = read_request(&mut r).unwrap().unwrap();
        assert_eq!(b.body, b"abc");
        assert!(read_request(&mut r).unwrap().is_none());
    }
}
