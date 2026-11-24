//! The daemon's side of the web UI. The web server itself is an opt-in
//! plugin, `rlord-web` (`crates/web`), and every network socket is there;
//! this module has no HTTP types. The plugin reaches the daemon over the
//! Unix socket with these methods:
//!
//! - `web.hello {protocol, version, pid, port}`: its first call. Another
//!   protocol is refused with `plugin::RPC_ERR_INCOMPATIBLE`.
//! - `web.call {op, args, via, page | device}`: the only way a browser's or
//!   a paired device's request reaches the daemon. `op` comes from a fixed
//!   list (`OPS`), not a pass-through: the list lives here, on the
//!   always-installed side, so a parsing bug in the plugin can't reach a
//!   method the web was never meant to have. `via` says who asked: a local
//!   page (`"local"`, with its `page`) or a device through the relay
//!   (`"remote"`, with its `device` id); an op not marked `remote` is
//!   refused from a device.
//! - `web.remote_reply` and `web.remote_state`: the plugin's side of remote
//!   access (`relay.rs`).
//! - `web.subscribe`: a push stream of `{"notify":"changed","rev"}`, at
//!   most one per 100 ms (`Daemon::push_loop`).
//! - `web.pane_open {target, surface, cols, rows}`, `web.pane_size`,
//!   `web.pane_closed`: a local page's pane socket opening, resizing and
//!   closing a pane, so the daemon knows which screen holds it
//!   (`control.rs`). Opening starts nothing.
//! - `web.subscribe` also carries `{"notify":"control","sessions"}`, every
//!   held pane, on each change (and once at the start).
//! - `web.pages {pages}`: the pages (control sockets) open now, by the
//!   page id the plugin gave each. `web.call` carries the id of the page
//!   that asked (`page`), never set by the browser. The daemon keeps what
//!   each page shows (`WebUi`), so the overlord's UI commands can land on
//!   a page (`ui::target`).
//! - Paired devices are surfaces too (the phone): the daemon keeps the same
//!   record per device, by the id the plugin stamps on its `web.call`s, and
//!   parks the last UI command for each (`page.pending` from that device
//!   hands it over). `web.ui_delivered {device, n}`: the plugin sent
//!   command `n` on that device's live session, so it isn't handed over
//!   again.
//!
//! The daemon also runs the plugin, as a child process (`Web`): started by
//! `web.start` (`rlord web`) or at daemon start with `[web] enabled`,
//! restarted with backoff when it dies, stopped with the daemon. The CLI
//! drives it with `web.status`, `web.start`, `web.stop` and `web.rotate`;
//! none of those, nor `web.hello`, is a browser op.
//!
//! See docs/working/web-ui-remote.md, "Daemon RPC additions" and "How it
//! starts, stops and is supervised".

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::db::{SessionQuery, SortMode};
use rlord_core::types::Status;
use rlord_core::{groups, plugin};

use crate::control::{self, Surface};
use crate::{methods, Daemon};

/// A `web.hello` from a plugin speaking another protocol. Answered with
/// RPC code `plugin::RPC_ERR_INCOMPATIBLE`; the plugin then exits with
/// `plugin::EXIT_INCOMPATIBLE`, which isn't retried.
#[derive(Debug)]
pub struct Incompatible(pub String);

impl fmt::Display for Incompatible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Incompatible {}

/// A browser op that isn't on `OPS`, one a device may not call, or a call
/// that doesn't say who asked. Answered with RPC code
/// `plugin::RPC_ERR_NOT_ALLOWED`; the message says which.
#[derive(Debug)]
pub struct NotAllowed(pub String);

impl NotAllowed {
    /// An op that isn't on `OPS`.
    fn op(op: &str) -> NotAllowed {
        NotAllowed(format!("{op}: not available to the web UI"))
    }
}

impl fmt::Display for NotAllowed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotAllowed {}

/// An allowed op asked by the wrong caller (`device.leave` from a local
/// page). Answered with RPC code `plugin::RPC_ERR_BAD_REQUEST`; the page
/// or device sees `bad_request`.
#[derive(Debug)]
pub struct BadRequest(pub String);

impl fmt::Display for BadRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadRequest {}

/// The remote API level: the ops, the snapshot and the `ui` commands as a
/// paired device sees them. A device's `hello` carries it (the plugin
/// takes it from `web.hello`'s answer); the phone app supports a range
/// and asks for an update outside it. Within a level, changes are
/// additive only: `tests/web_api.rs` pins the remote ops and the remote
/// snapshot's keys against `tests/fixtures/remote_api.json`.
pub const API: u32 = 2;

/// One browser op: the daemon method it calls ("" when it is answered
/// here), the only arguments passed on (any others are dropped, so a
/// browser can't set what a method takes from its trusted callers, such
/// as `session.stop`'s `interrupt` or `session.restart`'s `hooks_work`),
/// whether a paired device may call it through the relay (`remote`), and
/// whether running it twice is the same as once (`idempotent`).
pub struct Op {
    pub op: &'static str,
    pub method: &'static str,
    pub args: &'static [&'static str],
    pub remote: bool,
    /// Safe to repeat: a look-up that changes nothing. The plugin runs such
    /// a device request as often as it is asked; any other it runs at most
    /// once per request id, answering a resend from its store
    /// (`docs/working/e2e-protocol.md`, "Delivery and recovery"). Told to
    /// the plugin in `web.hello`'s answer.
    pub idempotent: bool,
}

/// An op for local pages and paired devices alike.
const fn op(op: &'static str, method: &'static str, args: &'static [&'static str]) -> Op {
    Op {
        op,
        method,
        args,
        remote: true,
        idempotent: false,
    }
}

/// An op for local pages only: what writes `config.toml`, manages
/// devices, or shows the key screen.
const fn local(op: &'static str, method: &'static str, args: &'static [&'static str]) -> Op {
    Op {
        op,
        method,
        args,
        remote: false,
        idempotent: false,
    }
}

impl Op {
    /// Marks a look-up: running it twice is the same as once.
    const fn idempotent(self) -> Op {
        Op {
            idempotent: true,
            ..self
        }
    }
}

/// The browser's ops. Each new op comes with a test
/// (`crates/daemon/tests/web_api.rs`). A new op is local (`local`) unless
/// a paired device has a reason to call it: then `op`, and a test that it
/// reaches its method from a device.
pub const OPS: &[Op] = &[
    op("snapshot", "", &["view", "rev"]).idempotent(),
    op("session.get", "session.get", &["ref"]).idempotent(),
    // Opening the overlord's pane when the user asks (never by itself).
    op("overlord.ensure", "overlord.ensure", &[]).idempotent(),
    // A stopped session's Resume: started on its conversation (an
    // overlord is ensured), as Enter in the list does.
    op("session.resume", "", &["ref"]),
    op("session.restart", "session.restart", &["ref"]),
    op("session.stop", "session.stop", &["ref"]),
    // The "tell the overlord" box: typed into the overlord's pane. Local
    // only: a phone reaches the overlord through its pane (`pane.open`).
    local("overlord.ask", "overlord.ask", &["text"]),
    op("session.archive", "session.archive", &["ref"]),
    op("session.unarchive", "session.unarchive", &["ref"]),
    op("session.rename", "session.rename", &["ref", "name"]),
    op(
        "session.describe",
        "session.describe",
        &["ref", "description"],
    ),
    op("session.notes", "session.notes", &["ref", "notes"]),
    op("session.tag", "session.tag", &["ref", "add", "remove"]),
    op("session.pin", "session.pin", &["ref", "pinned"]),
    // Make another agent's overlord the active one.
    op("overlord.switch", "overlord.switch", &["agent"]),
    // What the page (or the device) shows (answered here; see `WebUi`).
    // Setting it twice is the same as once.
    op(
        "page.report",
        "",
        &["route", "view", "visible", "form", "input"],
    )
    .idempotent(),
    // A UI command that never reached its page or device (asked on
    // connecting).
    op("page.pending", "", &[]),
    // The list view a page with none of its own starts from.
    op("view.default", "", &[]).idempotent(),
    // The key screen, read-only (keys change only in a terminal).
    local("keys.list", "keys.list", &[]).idempotent(),
    // The forms' look-ups: agents, models and defaults; the repo finder;
    // the code folder as set now and rlord's suggestion.
    op("form.defaults", "session.defaults", &[]).idempotent(),
    op("form.repos", "repo.find", &[]).idempotent(),
    local("form.setup", "", &[]).idempotent(),
    // The code-folder form's save: what the user confirmed there.
    local(
        "form.setup_save",
        "",
        &["code_dir", "roots", "create", "allow_home"],
    ),
    // The describe-a-session form: a request to the overlord.
    op(
        "overlord.new_session",
        "overlord.new_session",
        &[
            "description",
            "repo",
            "project",
            "agent",
            "model",
            "workspace",
            "base",
            "prefer",
            "tags",
            "chosen",
            "resolved",
            "confirmed",
        ],
    ),
    // The code-folder form with a description: a request to the overlord.
    local(
        "overlord.setup_request",
        "overlord.setup_request",
        &[
            "description",
            "code_dir",
            "code_dir_source",
            "roots",
            "roots_source",
        ],
    ),
    // The manual form's launch. The overlord's background for a session it
    // proposed comes from the daemon (`proposal`), never from the page.
    op(
        "session.new",
        "",
        &[
            "repo",
            "project",
            "name",
            "branch",
            "agent",
            "model",
            "workspace",
            "prompt",
            "base",
            "prefer",
            "proposal",
        ],
    ),
    op(
        "session.fork",
        "session.fork",
        &["ref", "prompt", "name", "workspace"],
    ),
    // Remote access, from the page's Devices view (relay.rs): the state,
    // a pairing offer and its cancel, the waiting device's confirmation,
    // unpairing a device.
    local("device.list", "", &[]).idempotent(),
    local("device.pair", "", &[]),
    local("device.pair_cancel", "", &[]),
    local("device.confirm", "", &[]),
    local("device.reject", "", &[]),
    local("device.revoke", "", &["device"]),
    // A session's (or, by its id, the overlord's) recent conversation as
    // text: its transcript, read-only, beside its pane.
    op("session.messages", "", &["ref", "before", "limit"]).idempotent(),
    // A paired device unpairs itself (the plugin removes it once the
    // answer is on its way). From a local page: `bad_request`.
    op("device.leave", "", &[]),
    // A device opens a live pane on a stream of its link: the daemon
    // resolves the target and says who holds it (control.rs), the plugin
    // attaches the view client. "Idempotent" stretches the word here: it
    // opens something in the plugin, though in rlord it changes nothing a
    // second open would change again (a free pane is its opener's either
    // way). It keeps the op out of the request store and the outbox, which
    // is what is wanted: a pane is never opened late. A local page opens
    // panes on its pane socket (`web.pane_open`): refused from one.
    op("pane.open", "", &["stream", "target", "cols", "rows"]).idempotent(),
    // Take a pane over: this page or device holds it now (twice is once).
    // Refused for a view-only device.
    op("pane.take", "", &["target", "cols", "rows"]).idempotent(),
    // Whether a paired device may type into panes (false: view-only).
    local("device.input", "", &["device", "input"]),
];

/// Ops that don't count as the user doing something on the page (look-ups
/// and reports a page makes by itself): every other op moves the page's
/// last use (`ui::target`). A `page.report` counts only with `input`.
const PASSIVE: &[&str] = &[
    "snapshot",
    "session.get",
    "page.report",
    "page.pending",
    "view.default",
    "keys.list",
    "form.defaults",
    "form.repos",
    "form.setup",
    "session.messages",
    // (Opened when a view shows: on a reconnect too.)
    "pane.open",
    "device.list",
    "device.leave",
];

/// Method families no op may name, whatever `OPS` says: a second check.
const FORBIDDEN: &[&str] = &[
    "admin.", "mcp.", "hook.", "web.", "config.", "ui.", "tmux.", "setup.", "relay.", "control.",
];

/// `web.hello`: the plugin's first call. From the plugin this daemon
/// started, it also says the plugin is up, and on which port.
pub fn hello(d: &Daemon, params: &Value) -> Result<Value> {
    let theirs = params["protocol"].as_u64();
    if theirs != Some(plugin::WEB_PROTOCOL.into()) {
        return Err(Incompatible(format!(
            "rlord-web {} speaks web protocol {}, this daemon (rlord {}) speaks {}: \
             run `rlord web install`",
            params["version"].as_str().unwrap_or("?"),
            theirs.map_or("?".to_string(), |p| p.to_string()),
            env!("CARGO_PKG_VERSION"),
            plugin::WEB_PROTOCOL,
        ))
        .into());
    }
    let pid = params["pid"].as_u64().and_then(|p| u32::try_from(p).ok());
    let port = params["port"].as_u64().and_then(|p| u16::try_from(p).ok());
    if let (Some(pid), Some(port)) = (pid, port) {
        d.web().hello(pid, port);
    }
    Ok(json!({
        "protocol": plugin::WEB_PROTOCOL,
        "version": env!("CARGO_PKG_VERSION"),
        // The remote API level, for each device's hello.
        "api": API,
        // The remote ops a device may repeat freely; the plugin runs any
        // other at most once per request id.
        "idempotent": OPS
            .iter()
            .filter(|o| o.remote && o.idempotent)
            .map(|o| o.op)
            .collect::<Vec<_>>(),
    }))
}

/// Who sent a `web.call`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Via<'a> {
    /// A local page, by the id the plugin gave it.
    Local(&'a str),
    /// A paired device through the relay, by its id.
    Remote(&'a str),
}

impl<'a> Via<'a> {
    /// `via` and `page` or `device` from `web.call`'s parameters (the
    /// plugin's, never the browser's or the device's).
    fn of(params: &'a Value) -> std::result::Result<Via<'a>, NotAllowed> {
        match params["via"].as_str() {
            Some("local") => Ok(Via::Local(params["page"].as_str().unwrap_or(""))),
            Some("remote") => params["device"]
                .as_str()
                .filter(|d| !d.is_empty())
                .map(Via::Remote)
                .ok_or_else(|| NotAllowed("web.call via remote needs the device's id".into())),
            _ => Err(NotAllowed(
                "web.call needs via: \"local\" (a page) or \"remote\" (a device)".into(),
            )),
        }
    }
}

/// `web.call {op, args, via, page | device}`: one request, from a local
/// page with that id or from a paired device (the plugin's word, never
/// the browser's or the device's). A device is a surface of its own,
/// known by its id (never by a page id passed along), and is refused an
/// op that isn't `remote`.
pub fn call(d: &Daemon, params: &Value) -> Result<Value> {
    let op = params["op"].as_str().unwrap_or("");
    let Some(found) = OPS.iter().find(|o| o.op == op) else {
        return Err(NotAllowed::op(op).into());
    };
    let via = Via::of(params)?;
    if FORBIDDEN.iter().any(|f| found.method.starts_with(f)) {
        return Err(NotAllowed::op(op).into());
    }
    let page = match via {
        Via::Local(page) => {
            if !PASSIVE.contains(&op) {
                d.web_ui().used(page);
            }
            // Anything from the page a command was for: it is alive and
            // has it (the page says so after each command), so it isn't
            // handed on.
            arrived(d, page);
            page
        }
        Via::Remote(_) if !found.remote => {
            return Err(NotAllowed(format!("{op}: not available to a remote device")).into());
        }
        Via::Remote(device) => {
            if !PASSIVE.contains(&op) {
                d.web_ui().device_used(device);
            }
            ""
        }
    };
    let args = filtered(found, &params["args"]);
    match (op, via) {
        ("page.report", Via::Remote(device)) => d.web_ui().report_device(device, &args),
        ("page.pending", Via::Remote(device)) => Ok(d.web_ui().take_parked(device)),
        ("device.leave", Via::Remote(device)) => Ok(json!({"left": device})),
        ("device.leave", Via::Local(_)) => Err(BadRequest(
            "device.leave is a paired device's own: a page unpairs one with device.revoke".into(),
        )
        .into()),
        ("pane.open", Via::Remote(device)) => {
            control::open(d, control::who(d, Surface::Device(device.into())), &args)
        }
        ("pane.open", Via::Local(_)) => Err(BadRequest(
            "pane.open is a paired device's: a page opens a pane on its pane socket".into(),
        )
        .into()),
        ("pane.take", Via::Remote(device)) => {
            control::take(d, control::who(d, Surface::Device(device.into())), &args)
        }
        ("pane.take", Via::Local(page)) if !page.is_empty() => {
            control::take(d, control::who(d, Surface::Page(page.into())), &args)
        }
        ("pane.take", Via::Local(_)) => {
            Err(BadRequest("pane.take needs the page that asks".into()).into())
        }
        _ => call_op(d, found, args, via, page),
    }
}

/// `call`, for the ops answered the same way whoever asked.
fn call_op(d: &Daemon, found: &Op, args: Value, via: Via, page: &str) -> Result<Value> {
    match found.op {
        "snapshot" => snapshot(d, &args, via),
        "session.messages" => session_messages(d, &args),
        "device.list" => crate::relay::status(d),
        "device.pair" => crate::relay::pair(d),
        "device.pair_cancel" => crate::relay::pair_cancel(d),
        "device.confirm" => crate::relay::confirm(d),
        "device.reject" => crate::relay::reject(d),
        "device.revoke" => crate::relay::revoke(d, &args),
        "device.input" => crate::relay::input(d, &args),
        "session.resume" => resume(d, &args),
        "page.report" => d.web_ui().report(page, &args),
        "page.pending" => Ok(pending(d)),
        "view.default" => crate::ui::default_view(d),
        "form.setup" => Ok(json!({
            "status": crate::setup::status(d)?,
            "suggest": crate::setup::suggest(d, &json!({}))?,
        })),
        "form.setup_save" => crate::setup::apply(d, &args),
        "overlord.new_session" => d.rpc(found.method, folder_request(args)),
        "session.new" => launch(d, args),
        // A session the list doesn't show: labelled as rows are.
        "session.get" => d.rpc(found.method, args).map(|mut s| {
            label(&mut s);
            s
        }),
        _ => d.rpc(found.method, args),
    }
}

/// Op `session.resume {ref}`: make sure the session runs (a stopped one is
/// restarted on its conversation; the overlord is ensured), as Enter in
/// the list does, without switching any terminal to it. Its row as it is
/// now.
fn resume(d: &Daemon, args: &Value) -> Result<Value> {
    // What `call` does before `ui.goto`, which resumes the same way:
    // where each agent is (may ask the login shell), and Codex's shared
    // server for a session that runs on it, before any lock.
    rlord_core::agents::install::warm();
    if d.starts_on_codex_daemon("ui.goto", args) {
        crate::codex_daemon::ensure_running();
    }
    let s = crate::ui::ensure_running(d, methods::p_str(args, "ref")?)?;
    crate::codex_daemon::sync_gate(&d.db());
    Ok(serde_json::to_value(s)?)
}

/// Only the arguments `op` passes on (any others are dropped).
fn filtered(op: &Op, given: &Value) -> Value {
    let mut args = serde_json::Map::new();
    if let Some(given) = given.as_object() {
        for k in op.args {
            if let Some(v) = given.get(*k) {
                args.insert((*k).to_string(), v.clone());
            }
        }
    }
    Value::Object(args)
}

/// Op `overlord.new_session`: a plain folder (`project`) goes as the TUI's
/// form sends one: `repo: "<path> (plain folder, not a repo)"`, no
/// worktree.
fn folder_request(mut args: Value) -> Value {
    let folder = args
        .as_object_mut()
        .and_then(|o| o.remove("project"))
        .and_then(|p| p.as_str().map(str::to_string))
        .filter(|p| !p.trim().is_empty());
    if let Some(f) = folder.filter(|_| args["repo"].as_str().is_none_or(str::is_empty)) {
        args["repo"] = json!(format!("{} (plain folder, not a repo)", f.trim()));
        args["workspace"] = json!(false);
    }
    args
}

/// Op `session.new`: the manual form's launch. With `proposal` (a session
/// the overlord proposed), the overlord's background and related sessions
/// come from the daemon's copy, and the session is told the overlord
/// launched it.
fn launch(d: &Daemon, args: Value) -> Result<Value> {
    d.rpc("session.new", launch_params(d, args)?)
}

/// `launch`'s parameters for `session.new`.
fn launch_params(d: &Daemon, mut args: Value) -> Result<Value> {
    let id = args
        .as_object_mut()
        .and_then(|o| o.remove("proposal"))
        .and_then(|p| p.as_str().map(str::to_string))
        .filter(|p| !p.is_empty());
    if let Some(id) = id {
        let kept = crate::ui::proposal(d, &id)?;
        args["context"] = kept["context"].clone();
        args["related"] = kept["related"].clone();
        args["by_overlord"] = json!(true);
    }
    Ok(args)
}

/// Op `snapshot {view, rev}`: everything the list draws, in one answer.
/// `view` is a `session.list` query, whose `query` (or `search`) is the
/// filter box's text, with the TUI's tokens (`search_tokens`), and whose
/// `usage` picks the usage measure; a `rev` that is still current gets
/// `{"unchanged": true}`. Grouping and labels are worked out here: the
/// page only draws. A local page's also has `relay` (remote access, for
/// its header); a device's doesn't.
pub fn snapshot(d: &Daemon, args: &Value, via: Via) -> Result<Value> {
    // Read first: a change while this is built moves it again, and the
    // page then asks once more.
    let rev = d.rev();
    if args["rev"].as_str() == Some(rev.as_str()) {
        return Ok(json!({"unchanged": true, "rev": rev}));
    }
    let mut view = match &args["view"] {
        Value::Null => json!({}),
        v @ Value::Object(_) => v.clone(),
        _ => bail!("view must be a session.list query (an object)"),
    };
    // The filter text: `query` (the TUI's view shape, as `set_list_view`
    // sends it), else `search`.
    let text = ["query", "search"]
        .iter()
        .find_map(|k| view.get(*k).and_then(Value::as_str).map(str::to_string));
    if let Some(o) = view.as_object_mut() {
        o.remove("query");
        // No measure: the user's sticky one (or config `usage`).
        if o.get("usage").is_some_and(Value::is_null) {
            o.remove("usage");
        }
    }
    if let Some(text) = text {
        search_tokens(&mut view, &text);
    }
    // (The overlord is `overlord`, never one of the rows.)
    view["include_overlord"] = json!(false);
    let mut rows = d.rpc("session.list", view)?;
    for row in rows.as_array_mut().into_iter().flatten() {
        label(row);
    }
    let order: Vec<String> = d.rpc("tag.order", json!({}))?["order"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let groups: Vec<Value> =
        groups::group_list(rows.as_array().map(Vec::as_slice).unwrap_or(&[]), &order)
            .into_iter()
            .map(|(tag, count)| {
                json!({
                    "tag": tag,
                    "label": groups::group_label(&tag),
                    "count": count,
                    "favourite": order.contains(&tag),
                })
            })
            .collect();
    let mut o = d.rpc("overlord.get", json!({}))?;
    if !o.is_null() {
        label(&mut o);
        for r in o["overlords"].as_array_mut().into_iter().flatten() {
            label(r);
        }
    }
    let statuses: Vec<Value> = [
        Status::Starting,
        Status::Working,
        Status::WaitingInput,
        Status::Idle,
        Status::Stopped,
        Status::Error,
    ]
    .into_iter()
    .map(|s| json!({"status": s, "label": s.label()}))
    .collect();
    let mut out = json!({
        "rev": rev,
        "rows": rows,
        "groups": groups,
        "overlord": o,
        "statuses": statuses,
    });
    if let Via::Local(_) = via {
        out["relay"] = crate::relay::summary(d);
    }
    Ok(out)
}

/// At most this many messages per `session.messages`, each at most this
/// many bytes.
const MESSAGES_MAX: usize = 50;
const MESSAGE_BYTES: usize = 4096;

/// Op `session.messages {ref, before, limit}`: any session's current
/// conversation as text, oldest first (at most 50 messages, each at most
/// 4 KiB; `before`: a message's `line`), read by plain code from its
/// transcript (`history::read`, as `recall` reads one). The transcript
/// beside a session's or the overlord's pane. No conversation yet: no
/// messages.
pub fn session_messages(d: &Daemon, args: &Value) -> Result<Value> {
    let r = methods::p_str(args, "ref")?;
    let limit = args["limit"]
        .as_u64()
        .map_or(MESSAGES_MAX, |l| (l as usize).clamp(1, MESSAGES_MAX));
    let before = args["before"].as_u64().filter(|b| *b > 0);
    let (s, path) = {
        let db = d.db();
        let s = db.resolve_ref(r)?;
        let path = db.transcript_source(&s.id)?;
        (s, path)
    };
    let head = json!({"id": s.id, "name": s.name, "agent": s.agent,
                      "conversation": s.agent_session});
    let (messages, more) = messages_of(path, before, limit, "the session's")?;
    Ok(json!({"session": head, "messages": messages, "more": more}))
}

/// The last `limit` messages of the transcript at `path` (those before
/// line `before`, if given), each clipped, and whether earlier ones are
/// left. No transcript: none.
fn messages_of(
    path: Option<String>,
    before: Option<u64>,
    limit: usize,
    whose: &str,
) -> Result<(Vec<Value>, bool)> {
    let path = path
        .map(PathBuf::from)
        .filter(|p| rlord_core::agents::transcript::exists(p));
    let Some(path) = path else {
        return Ok((vec![], false));
    };
    // One more than asked: whether there are earlier ones.
    let mut msgs =
        rlord_core::history::read(&path, before, limit + 1, 0, (limit + 1) * 4 * MESSAGE_BYTES)
            .with_context(|| format!("reading {whose} transcript ({})", path.display()))?;
    let more = msgs.len() > limit;
    if more {
        msgs.drain(..msgs.len() - limit);
    }
    let messages = msgs
        .into_iter()
        .map(|m| {
            json!({
                "line": m.line,
                "who": m.who,
                "at": m.at,
                "text": clip_bytes(&m.text, MESSAGE_BYTES),
            })
        })
        .collect();
    Ok((messages, more))
}

/// `s` cut to at most `n` bytes, on a character boundary.
fn clip_bytes(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// The filter box's text, read as the TUI reads its filter
/// (`SessionQuery::apply_search_text`): `tag:`, `status:`, `agent:`,
/// `proj:`, `group:` and `sort:` tokens set those filters of the view,
/// the other words are its free-text `search`. So the page and the TUI
/// take the same filter, and the page doesn't parse it.
fn search_tokens(view: &mut Value, text: &str) {
    let mut q = SessionQuery::default();
    if let Some(s) = view["sort"].as_str().and_then(SortMode::parse) {
        q.sort = s;
    }
    let sort = q.sort;
    q.apply_search_text(text);
    view["search"] = json!(q.search);
    if q.sort != sort {
        view["sort"] = json!(q.sort.label());
    }
    if !q.status.is_empty() {
        view["status"] = json!(q.status);
    }
    for (k, v) in [
        ("tag", q.tag),
        ("agent", q.agent),
        ("project", q.project),
        ("group", q.group),
    ] {
        if let Some(v) = v {
            view[k] = json!(v);
        }
    }
}

/// Add `status_label` (as people see it) beside a row's `status`.
fn label(row: &mut Value) {
    let l = row["status"]
        .as_str()
        .map(Status::label_of)
        .map(str::to_string);
    row["status_label"] = json!(l);
}

/// `web.pane_open {target, surface, cols, rows}`: a local page's pane
/// socket opens a pane (`surface`: `page:<id>`). `target` is `overlord`
/// (the active one) or a session reference. Answered as a device's
/// `pane.open` is (`control::open`): the session, its tmux session,
/// whether it runs, whether this page holds it and the size to attach at.
/// Starts nothing: a session that isn't running says so, and the page
/// offers Resume.
pub fn pane_open(d: &Daemon, params: &Value) -> Result<Value> {
    let s = methods::p_str(params, "surface")?;
    let surface = Surface::parse(s).ok_or_else(|| BadRequest(format!("unknown surface {s:?}")))?;
    control::open(d, control::who(d, surface), params)
}

// ---- the surfaces besides the terminal: web pages and paired devices ----

/// At most this many pages are kept (the plugin's control-socket limit),
/// and at most this many devices.
const MAX_PAGES: usize = 16;
const MAX_DEVICES: usize = 16;

/// How long a UI command waits for a device that had no live session
/// (`page.pending` from it, at its next hello).
const DEVICE_PENDING_FOR: Duration = Duration::from_secs(600);

/// The open web pages and the paired devices that used rlord, as the
/// plugin and they report them: the side of "which screen is the user on"
/// (`ui::target`, `ui.state`) that isn't the terminal.
#[derive(Default)]
pub struct WebUi {
    pages: Vec<Page>,
    /// Paired devices (the phone), by device id: added by their first op
    /// or report, dropped when they are no longer paired (`keep_devices`)
    /// or the plugin goes.
    devices: Vec<Page>,
    /// Order of arrival, shared by pages and devices.
    seq: u64,
    /// Numbers the commands parked for devices (`Parked::n`).
    parked_seq: u64,
}

/// One open page (a browser tab's control socket), or one paired device.
#[derive(Clone, Debug, Default)]
pub struct Page {
    /// The plugin's id for it (`p<n>-<8 hex>`), or the device's id.
    pub id: String,
    /// `{kind: "list" | "overlord" | "session", id?}`.
    pub route: Value,
    /// Its list view, in the TUI's shape (`ui::normalize_view`); null
    /// before it reports one.
    pub view: Value,
    pub visible: bool,
    /// The form open on it, if any.
    pub form: Option<String>,
    /// When the user last did something on it (the daemon's clock, ms).
    pub last_input: Option<i64>,
    /// Order of arrival: among surfaces never used, the newest wins.
    pub seq: u64,
    /// A device's last UI command, until its live session had it
    /// (`web.ui_delivered`) or the device takes it (`page.pending`).
    pub parked: Option<Parked>,
}

/// A UI command kept for a device.
#[derive(Clone, Debug)]
pub struct Parked {
    pub at: Instant,
    /// Its number, on the `ui` line and in `web.ui_delivered`.
    pub n: u64,
    pub cmd: Value,
}

/// The forms a page may say it has open.
const FORMS: &[&str] = &[
    "describe", "confirm", "manual", "fork", "setup", "keys", "devices",
];

impl WebUi {
    /// `web.pages`: the pages open now. New ones are added, missing ones
    /// dropped; the others keep what they reported.
    pub fn set_pages(&mut self, ids: &[String]) {
        self.pages.retain(|p| ids.contains(&p.id));
        for id in ids {
            if self.pages.len() >= MAX_PAGES {
                break;
            }
            if !id.is_empty() && !self.pages.iter().any(|p| &p.id == id) {
                self.seq += 1;
                self.pages.push(Page {
                    id: clip(id, 40),
                    route: json!({"kind": "list"}),
                    seq: self.seq,
                    ..Default::default()
                });
            }
        }
    }

    /// The plugin is gone: so are its pages and its devices' records.
    pub fn clear(&mut self) {
        self.pages.clear();
        self.devices.clear();
    }

    /// Page `id` was used now. (An unknown id changes nothing.)
    pub fn used(&mut self, id: &str) {
        if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
            p.last_input = Some(rlord_core::types::now_ms());
        }
    }

    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    pub fn page(&self, id: &str) -> Option<&Page> {
        self.pages.iter().find(|p| p.id == id)
    }

    pub fn page_mut(&mut self, id: &str) -> Option<&mut Page> {
        self.pages.iter_mut().find(|p| p.id == id)
    }

    pub fn devices(&self) -> &[Page] {
        &self.devices
    }

    pub fn device(&self, id: &str) -> Option<&Page> {
        self.devices.iter().find(|p| p.id == id)
    }

    /// Device `id`'s record, made on its first op or report. Past the
    /// limit, the one used longest ago makes room.
    pub fn device_mut(&mut self, id: &str) -> &mut Page {
        let id = clip(id, 40);
        let i = match self.devices.iter().position(|p| p.id == id) {
            Some(i) => i,
            None => {
                if self.devices.len() >= MAX_DEVICES {
                    let oldest = self
                        .devices
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, p)| (p.last_input, p.seq))
                        .map(|(i, _)| i);
                    if let Some(i) = oldest {
                        self.devices.remove(i);
                    }
                }
                self.seq += 1;
                self.devices.push(Page {
                    id,
                    route: json!({"kind": "list"}),
                    seq: self.seq,
                    ..Default::default()
                });
                self.devices.len() - 1
            }
        };
        &mut self.devices[i]
    }

    /// Device `id` was used now (the daemon's clock, as for a page).
    pub fn device_used(&mut self, id: &str) {
        self.device_mut(id).last_input = Some(rlord_core::types::now_ms());
    }

    /// Only the devices still paired keep their records.
    pub fn keep_devices(&mut self, paired: &[String]) {
        self.devices.retain(|p| paired.contains(&p.id));
    }

    /// Park `cmd` in device `id`'s one slot (replacing what was there):
    /// its number.
    pub fn park(&mut self, id: &str, cmd: Value) -> u64 {
        self.parked_seq += 1;
        let n = self.parked_seq;
        self.device_mut(id).parked = Some(Parked {
            at: Instant::now(),
            n,
            cmd,
        });
        n
    }

    /// Command `n` reached device `id`'s live session.
    fn delivered(&mut self, id: &str, n: u64) {
        if let Some(p) = self.devices.iter_mut().find(|p| p.id == id) {
            if p.parked.as_ref().is_some_and(|k| k.n == n) {
                p.parked = None;
            }
        }
    }

    /// Op `page.pending` from device `id`: its parked command, once, if
    /// under 10 minutes old. Null otherwise.
    pub(crate) fn take_parked(&mut self, id: &str) -> Value {
        let Some(p) = self.devices.iter_mut().find(|p| p.id == id) else {
            return Value::Null;
        };
        match p.parked.take() {
            Some(k) if k.at.elapsed() < DEVICE_PENDING_FOR => k.cmd,
            _ => Value::Null,
        }
    }

    /// Op `page.report {route, view, visible, form, input}` from page
    /// `id`: what it shows now. Only the fields given change; `input:
    /// true` stamps its last use with the daemon's clock.
    fn report(&mut self, id: &str, args: &Value) -> Result<Value> {
        let Some(p) = self.pages.iter_mut().find(|p| p.id == id) else {
            bail!("this page isn't open (reload it)");
        };
        report_fields(p, args)?;
        Ok(json!({"ok": true}))
    }

    /// `page.report` from device `id`, as from a page (the id is the
    /// plugin's word, never the device's).
    pub(crate) fn report_device(&mut self, id: &str, args: &Value) -> Result<Value> {
        report_fields(self.device_mut(id), args)?;
        Ok(json!({"ok": true}))
    }
}

/// A `page.report`'s fields into `p`: all checked, then set.
fn report_fields(p: &mut Page, args: &Value) -> Result<()> {
    let route = match args.get("route").filter(|r| !r.is_null()) {
        None => None,
        Some(r) => {
            let kind = r["kind"].as_str().unwrap_or("");
            if !matches!(kind, "list" | "overlord" | "session") {
                bail!("route kind must be list, overlord or session");
            }
            let mut route = json!({"kind": kind});
            if let Some(i) = r["id"].as_str().filter(|_| kind == "session") {
                route["id"] = json!(clip(i, 80));
            }
            Some(route)
        }
    };
    let form = match args.get("form") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(f) => {
            let f = f.as_str().unwrap_or("");
            if !FORMS.contains(&f) {
                bail!("form must be one of {} or null", FORMS.join(", "));
            }
            Some(Some(f.to_string()))
        }
    };
    if let Some(r) = route {
        p.route = r;
    }
    if let Some(f) = form {
        p.form = f;
    }
    if let Some(v) = args.get("view").filter(|v| v.is_object()) {
        p.view = crate::ui::normalize_view(v);
    }
    if let Some(v) = args.get("visible").and_then(Value::as_bool) {
        p.visible = v;
    }
    if args["input"] == true {
        p.last_input = Some(rlord_core::types::now_ms());
    }
    Ok(())
}

/// `web.ui_delivered {device, n}`: the plugin sent device `device` its
/// command `n` on a live session, so `page.pending` doesn't hand it over
/// again.
pub fn ui_delivered(d: &Daemon, params: &Value) -> Result<Value> {
    let device = methods::p_str(params, "device")?;
    let n = params["n"].as_u64().context("ui_delivered needs n")?;
    d.web_ui().delivered(device, n);
    Ok(json!({"ok": true}))
}

/// How long a UI command waits for a page (`page.pending`).
const PENDING_FOR: Duration = Duration::from_secs(30);

/// A call from `page`: the parked command for it, if any, has arrived.
fn arrived(d: &Daemon, page: &str) {
    let mut slot = d.pending_web.lock().expect("ui mutex");
    if slot.as_ref().is_some_and(|(_, p, _)| p == page) {
        *slot = None;
    }
}

/// Op `page.pending`: the last UI command for a web page, once, if it is
/// under 30 s old, its page never said anything after it, and that page
/// has closed since (so it may never have arrived: a reload, a reconnect).
/// Null otherwise.
fn pending(d: &Daemon) -> Value {
    let mut slot = d.pending_web.lock().expect("ui mutex");
    let lost = slot
        .as_ref()
        .is_some_and(|(at, page, _)| at.elapsed() < PENDING_FOR && d.web_ui().page(page).is_none());
    if !lost {
        return Value::Null;
    }
    slot.take().map(|(_, _, cmd)| cmd).unwrap_or(Value::Null)
}

/// `s` cut to at most `n` characters.
fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `web.pages {pages: [ids]}`: the plugin's open pages, the whole list
/// each time (so a lost call heals on the next).
pub fn pages(d: &Daemon, params: &Value) -> Result<Value> {
    let ids: Vec<String> = params["pages"]
        .as_array()
        .context("pages must be a list of page ids")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let mut w = d.web_ui();
    w.set_pages(&ids);
    Ok(json!({"pages": w.pages().len()}))
}

// ---- running the plugin ----

/// How long `web.start` waits for the plugin's hello.
const HELLO_WAIT: Duration = Duration::from_secs(5);
/// How long a stopped plugin gets between SIGTERM and SIGKILL.
const TERM_WAIT: Duration = Duration::from_secs(2);
/// How long a stop waits, after SIGKILL, for the plugin to be reaped.
const KILL_WAIT: Duration = Duration::from_millis(500);
/// How long a stop waits, once the plugin is gone, for the daemon to let
/// go of its `web.subscribe` stream (a plugin this daemon didn't start
/// may hold one open: not waited for longer).
const STREAM_WAIT: Duration = Duration::from_secs(1);
/// Restarts after an unexpected exit: from 1 s, doubling, at most 60 s;
/// none after five failures in a row.
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const MAX_FAILURES: u32 = 5;
/// A run at least this long resets the failure count.
const STABLE_RUN: Duration = Duration::from_secs(60);

/// The plugin as the daemon runs it.
pub struct Web {
    sup: Mutex<Sup>,
    /// Signalled on a hello, on every exit, and when a stream ends.
    moved: Condvar,
    /// The SHA-256 the plugin must have: the compiled-in one (tests set
    /// another, `set_expected_hash`).
    expected: Mutex<Option<String>>,
    backoff_base: Mutex<Duration>,
}

#[derive(Default)]
struct Sup {
    /// Keep it running (a start was asked for, no stop since).
    want: bool,
    /// Bumped on every spawn: which child an exit or a timer is about.
    generation: u64,
    pid: Option<u32>,
    /// Its port, once it said hello.
    port: Option<u16>,
    /// The generation that said hello.
    hello: Option<u64>,
    started_at: Option<Instant>,
    state: State,
    /// Why it isn't running (not installed, a hash mismatch, port in use,
    /// incompatible, too many failures).
    why: Option<String>,
    /// The last line it wrote to `web.log` before its last exit.
    last_error: Option<String>,
    failures: u32,
    /// The executable last started, and whether it was checked.
    path: Option<PathBuf>,
    verified: bool,
    /// Open `web.subscribe` streams (`Daemon::push_loop`): the plugin's.
    /// A restarted plugin's may overlap the old one's briefly.
    streams: usize,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    #[default]
    Stopped,
    Starting,
    Running,
    /// Died; restarting after a pause.
    Backoff,
    /// Not restarted: `why` says why.
    Failed,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Stopped => "stopped",
            State::Starting => "starting",
            State::Running => "running",
            State::Backoff => "restarting",
            State::Failed => "failed",
        }
    }
}

impl Default for Web {
    fn default() -> Self {
        Web {
            sup: Mutex::default(),
            moved: Condvar::new(),
            expected: Mutex::new(plugin::COMPILED_SHA256.map(str::to_string)),
            backoff_base: Mutex::new(BACKOFF_BASE),
        }
    }
}

impl Web {
    fn lock(&self) -> MutexGuard<'_, Sup> {
        self.sup.lock().expect("web mutex")
    }

    /// Tests: the hash the plugin must have (None: a checkout's build).
    #[doc(hidden)]
    pub fn set_expected_hash(&self, h: Option<String>) {
        *self.expected.lock().expect("web mutex") = h;
    }

    /// Tests: the first restart's pause.
    #[doc(hidden)]
    pub fn set_backoff_base(&self, d: Duration) {
        *self.backoff_base.lock().expect("web mutex") = d;
    }

    fn locate(&self) -> std::result::Result<plugin::Plugin, plugin::PluginError> {
        plugin::locate_with(self.expected.lock().expect("web mutex").as_deref())
    }

    /// `web.status`: installed or not, running or not, its port, why not.
    pub fn status(&self, enabled: bool) -> Value {
        let found = self.locate();
        let sup = self.lock();
        let (path, verified) = match &found {
            Ok(p) => (Some(p.path.display().to_string()), p.verified),
            Err(_) => (sup.path.as_ref().map(|p| p.display().to_string()), false),
        };
        json!({
            "installed": found.is_ok(),
            "problem": found.err().map(|e| e.to_string()),
            "path": path,
            "verified": verified,
            "enabled": enabled,
            "state": sup.state.name(),
            "pid": sup.pid,
            "port": if sup.state == State::Running { sup.port } else { None },
            "why": sup.why,
            "last_error": sup.last_error,
        })
    }

    /// `web.start`: start it (if it isn't running) and wait for its hello.
    /// Errors when it can't start or doesn't come up.
    pub fn start(self: &Arc<Self>, enabled: bool) -> Result<Value> {
        let mut sup = self.lock();
        sup.want = true;
        if sup.pid.is_none() {
            // An explicit start: a fresh count.
            sup.failures = 0;
            self.spawn(&mut sup)?;
        }
        let generation = sup.generation;
        let deadline = Instant::now() + HELLO_WAIT;
        loop {
            if sup.hello == Some(sup.generation) && sup.pid.is_some() {
                break;
            }
            if sup.generation != generation || sup.pid.is_none() {
                let why = sup
                    .why
                    .clone()
                    .or_else(|| sup.last_error.clone())
                    .unwrap_or_else(|| "it exited".into());
                bail!(
                    "rlord-web didn't start: {why} (log: {})",
                    plugin::log_path().display()
                );
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "rlord-web didn't answer within {} s (log: {})",
                    HELLO_WAIT.as_secs(),
                    plugin::log_path().display()
                );
            }
            sup = self.moved.wait_timeout(sup, left).expect("web mutex").0;
        }
        drop(sup);
        Ok(self.status(enabled))
    }

    /// Spawn `<plugin> serve`, output appended to `web.log`; a thread
    /// waits for it to exit. Under the lock.
    fn spawn(self: &Arc<Self>, sup: &mut Sup) -> Result<()> {
        let p = match self.locate() {
            Ok(p) => p,
            Err(e) => {
                sup.state = State::Failed;
                sup.why = Some(e.to_string());
                return Err(e.into());
            }
        };
        let log = plugin::log_path();
        if let Some(dir) = log.parent() {
            rlord_core::private::dir(dir)?;
        }
        let out = rlord_core::private::append(&log)?;
        let err = out.try_clone()?;
        let mut child = std::process::Command::new(&p.path)
            .arg("serve")
            .stdin(std::process::Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .with_context(|| format!("starting {}", p.path.display()))?;
        sup.generation += 1;
        sup.pid = Some(child.id());
        sup.port = None;
        sup.started_at = Some(Instant::now());
        sup.state = State::Starting;
        sup.why = None;
        sup.path = Some(p.path.clone());
        sup.verified = p.verified;
        if !p.verified {
            tracing::info!(path = %p.path.display(), "starting rlord-web (unverified: a build without a plugin hash)");
        }
        let generation = sup.generation;
        let web = Arc::clone(self);
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code());
            web.exited(generation, code);
        });
        Ok(())
    }

    /// The plugin of `generation` exited (`code` None: by a signal).
    fn exited(self: &Arc<Self>, generation: u64, code: Option<i32>) {
        let mut sup = self.lock();
        if sup.generation != generation {
            return;
        }
        let ran = sup.started_at.map(|t| t.elapsed()).unwrap_or_default();
        sup.pid = None;
        sup.port = None;
        sup.last_error = last_line(&plugin::log_path());
        if !sup.want {
            sup.state = State::Stopped;
        } else if code == Some(plugin::EXIT_INCOMPATIBLE) {
            sup.state = State::Failed;
            sup.why = Some(
                "the plugin and this rlord disagree on the web protocol: run `rlord web install`"
                    .into(),
            );
        } else if code == Some(plugin::EXIT_PORT_IN_USE) {
            sup.state = State::Failed;
            sup.why = Some(
                "its port is in use (another program, or another rlord): change [web] port".into(),
            );
        } else {
            if ran >= STABLE_RUN {
                sup.failures = 0;
            }
            sup.failures += 1;
            if sup.failures >= MAX_FAILURES {
                sup.state = State::Failed;
                sup.why = Some(format!(
                    "it failed {MAX_FAILURES} times in a row (exit {})",
                    code.map_or("by a signal".to_string(), |c| c.to_string())
                ));
            } else {
                sup.state = State::Backoff;
                let base = *self.backoff_base.lock().expect("web mutex");
                let pause = base
                    .saturating_mul(1 << (sup.failures - 1).min(16))
                    .min(BACKOFF_MAX);
                tracing::warn!(code = ?code, failures = sup.failures, pause_ms = pause.as_millis() as u64,
                    "rlord-web exited; restarting");
                let web = Arc::clone(self);
                std::thread::spawn(move || {
                    std::thread::sleep(pause);
                    let mut sup = web.lock();
                    // Not if it was stopped, or started again, meanwhile.
                    if sup.generation == generation && sup.want && sup.pid.is_none() {
                        if let Err(e) = web.spawn(&mut sup) {
                            tracing::warn!(error = %e, "restarting rlord-web");
                        }
                        web.moved.notify_all();
                    }
                });
            }
        }
        if sup.state == State::Failed {
            tracing::warn!(why = ?sup.why, "rlord-web is not running");
        }
        self.moved.notify_all();
    }

    /// A hello from process `pid`: if it is the plugin this daemon
    /// started, it is up, on `port`.
    pub fn hello(&self, pid: u32, port: u16) {
        let mut sup = self.lock();
        if sup.pid != Some(pid) {
            tracing::info!(pid, "web.hello from a plugin this daemon didn't start");
            return;
        }
        sup.port = Some(port);
        sup.hello = Some(sup.generation);
        sup.state = State::Running;
        sup.why = None;
        self.moved.notify_all();
    }

    /// `web.stop`: SIGTERM, SIGKILL after 2 s; not restarted. Returns once
    /// the plugin is reaped and the daemon has let go of its stream (its
    /// pages and relay state gone with it): a relay command after it, or
    /// after a `start` right after it, never counts the stopped plugin's
    /// stream as one that will answer.
    pub fn stop(&self) {
        let mut sup = self.lock();
        sup.want = false;
        let Some(pid) = sup.pid else {
            if sup.state != State::Failed {
                sup.state = State::Stopped;
            }
            return;
        };
        let generation = sup.generation;
        signal(pid, libc::SIGTERM);
        let mut deadline = Instant::now() + TERM_WAIT;
        let mut killed = false;
        while sup.generation == generation && sup.pid.is_some() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                if killed {
                    // (Reaped by its thread later.)
                    return;
                }
                signal(pid, libc::SIGKILL);
                killed = true;
                deadline = Instant::now() + KILL_WAIT;
                continue;
            }
            sup = self.moved.wait_timeout(sup, left).expect("web mutex").0;
        }
        let deadline = Instant::now() + STREAM_WAIT;
        while sup.streams > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                tracing::info!(
                    streams = sup.streams,
                    "a web.subscribe stream outlived the plugin"
                );
                return;
            }
            sup = self.moved.wait_timeout(sup, left).expect("web mutex").0;
        }
    }

    /// A `web.subscribe` stream opened (`Daemon::push_loop`).
    pub(crate) fn stream_opened(&self) {
        self.lock().streams += 1;
    }

    /// A `web.subscribe` stream ended; `gone` runs when it was the last
    /// one, under the supervisor's lock, so a `stop` waiting for it
    /// returns after `gone`.
    pub(crate) fn stream_closed(&self, gone: impl FnOnce()) {
        let mut sup = self.lock();
        sup.streams = sup.streams.saturating_sub(1);
        if sup.streams == 0 {
            gone();
        }
        self.moved.notify_all();
    }

    /// How many `web.subscribe` streams are open (the plugin's).
    pub(crate) fn streams(&self) -> usize {
        self.lock().streams
    }
}

/// Send `sig` to `pid`.
fn signal(pid: u32, sig: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // Safety: a plain kill(2) on the child this daemon started (still
        // unreaped, so its pid can't have been reused).
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// The last non-empty line of the file at `path` (its last 4 KiB read).
fn last_line(path: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(4096))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    String::from_utf8_lossy(&buf)
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// `web.status`.
pub fn status(d: &Daemon) -> Result<Value> {
    Ok(d.web().status(d.config().web.enabled))
}

/// `web.start`.
pub fn start(d: &Daemon) -> Result<Value> {
    d.web().start(d.config().web.enabled)
}

/// `web.stop`.
pub fn stop(d: &Daemon) -> Result<Value> {
    d.web().stop();
    status(d)
}

/// `web.rotate`: a new token. The token file goes and the plugin restarts
/// (making a new one), which closes every open web connection.
pub fn rotate(d: &Daemon) -> Result<Value> {
    d.web().stop();
    match std::fs::remove_file(plugin::token_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).context("removing the web token");
        }
        _ => {}
    }
    start(d)
}

/// At daemon start: start the plugin when `[web] enabled` or `[remote]
/// enabled` (in the background; a failure is logged and never stops the
/// daemon).
pub fn start_if_enabled(d: &Arc<Daemon>) {
    let cfg = d.config();
    if !cfg.web.enabled && !cfg.remote.enabled {
        return;
    }
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        if let Err(e) = start(&d) {
            tracing::warn!(error = %e, "[web] or [remote] enabled, but rlord-web didn't start");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_op_reaches_a_forbidden_family() {
        for o in OPS {
            for f in FORBIDDEN {
                assert!(!o.method.starts_with(f), "{} -> {}", o.op, o.method);
                assert!(!o.op.starts_with(f), "{}", o.op);
            }
        }
    }

    fn op_named(name: &str) -> &'static Op {
        OPS.iter().find(|o| o.op == name).unwrap()
    }

    /// Only look-ups (and a page's report of what it shows, which sets the
    /// same thing again) are idempotent: anything that changes something
    /// runs at most once per request id from a device, so a resend after a
    /// lost link can't repeat it. `page.pending` hands its command over
    /// once, so it isn't. `web.hello` tells the plugin which are.
    #[test]
    fn only_look_ups_are_idempotent() {
        let idem: Vec<&str> = OPS.iter().filter(|o| o.idempotent).map(|o| o.op).collect();
        assert_eq!(
            idem,
            [
                "snapshot",
                "session.get",
                "overlord.ensure",
                "page.report",
                "view.default",
                "keys.list",
                "form.defaults",
                "form.repos",
                "form.setup",
                "device.list",
                "session.messages",
                "pane.open",
                "pane.take",
            ]
        );
        for o in [
            "overlord.ask",
            "overlord.new_session",
            "session.new",
            "session.stop",
            "page.pending",
            "device.leave",
        ] {
            assert!(!op_named(o).idempotent, "{o}");
        }
        let d = Daemon::new(
            rlord_core::db::Db::open_in_memory().unwrap(),
            Default::default(),
        );
        let h = hello(
            &d,
            &json!({"protocol": plugin::WEB_PROTOCOL, "version": "x"}),
        )
        .unwrap();
        let told: Vec<&str> = h["idempotent"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // Remote ones only: a device never sends the others.
        assert!(
            told.contains(&"snapshot") && !told.contains(&"keys.list"),
            "{told:?}"
        );
    }

    /// A browser can't set what only trusted callers set: the overlord's
    /// background, its role, extra agent arguments; those come from the
    /// daemon's copy of the proposal.
    #[test]
    fn a_launch_takes_only_the_forms_fields() {
        let d = Daemon::new(
            rlord_core::db::Db::open_in_memory().unwrap(),
            Default::default(),
        );
        let got = filtered(
            op_named("session.new"),
            &json!({"repo": "/r", "name": "n", "extra_args": ["--danger"], "role": "overlord",
                    "by_overlord": true, "context": "mine", "related": ["x"],
                    "tmux_name": "t", "hooks_work": true, "remote_control": true}),
        );
        assert_eq!(got, json!({"repo": "/r", "name": "n"}));
        assert_eq!(launch_params(&d, got.clone()).unwrap(), got);
        // A proposal's background is the daemon's.
        d.proposals.lock().unwrap().push_back((
            "p1".into(),
            json!({"context": "the overlord's", "related": ["a1"]}),
        ));
        let p = launch_params(&d, json!({"repo": "/r", "proposal": "p1"})).unwrap();
        assert_eq!(
            p,
            json!({"repo": "/r", "context": "the overlord's", "related": ["a1"], "by_overlord": true})
        );
        assert!(launch_params(&d, json!({"proposal": "p9"})).is_err());
        let f = filtered(
            op_named("session.fork"),
            &json!({"ref": "a", "prompt": "p", "context": "c", "related": ["b"], "model": "m",
                    "branch": "b", "description": "d", "workspace": true}),
        );
        assert_eq!(f, json!({"ref": "a", "prompt": "p", "workspace": true}));
        let s = filtered(
            op_named("form.setup_save"),
            &json!({"code_dir": "/c", "skip": true, "roots": "follow"}),
        );
        assert_eq!(s, json!({"code_dir": "/c", "roots": "follow"}));
        let r = filtered(
            op_named("overlord.new_session"),
            &json!({"description": "d", "request_id": "r1"}),
        );
        assert_eq!(r, json!({"description": "d"}));
    }

    /// A plain folder goes to the overlord as the TUI's form sends one.
    #[test]
    fn a_folder_request_has_the_forms_wire_shape() {
        assert_eq!(
            folder_request(json!({"description": "d", "project": "/tmp/x", "workspace": true})),
            json!({"description": "d", "repo": "/tmp/x (plain folder, not a repo)", "workspace": false})
        );
        assert_eq!(
            folder_request(json!({"repo": "/r", "workspace": true})),
            json!({"repo": "/r", "workspace": true})
        );
    }

    #[test]
    fn ops_are_listed_once() {
        let mut names: Vec<&str> = OPS.iter().map(|o| o.op).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), OPS.len());
    }
}
