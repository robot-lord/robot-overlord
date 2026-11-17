//! The daemon's side of the web UI. The web server itself is an opt-in
//! plugin, `rlord-web` (`crates/web`), and every network socket is there;
//! this module has no HTTP types. The plugin reaches the daemon over the
//! Unix socket with these methods:
//!
//! - `web.hello {protocol, version, pid, port}`: its first call. Another
//!   protocol is refused with `plugin::RPC_ERR_INCOMPATIBLE`.
//! - `web.call {op, args}`: the only way a browser request reaches the
//!   daemon. `op` comes from a fixed list (`OPS`), not a pass-through: the
//!   list lives here, on the always-installed side, so a parsing bug in
//!   the plugin can't reach a method the web was never meant to have.
//! - `web.subscribe`: a push stream of `{"notify":"changed","rev"}`, at
//!   most one per 100 ms (`Daemon::push_loop`).
//! - `web.pane_target {target}`: which tmux session a pane shows. Starts
//!   nothing.
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
use rlord_core::{groups, plugin, tmux};

use crate::{methods, overlord, Daemon};

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

/// A browser op that isn't on `OPS`. Answered with RPC code
/// `plugin::RPC_ERR_NOT_ALLOWED`.
#[derive(Debug)]
pub struct NotAllowed(pub String);

impl fmt::Display for NotAllowed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: not available to the web UI", self.0)
    }
}

impl std::error::Error for NotAllowed {}

/// One browser op: the daemon method it calls ("" when it is answered
/// here) and the only arguments passed on (any others are dropped, so a
/// browser can't set what a method takes from its trusted callers, such
/// as `session.stop`'s `interrupt` or `session.restart`'s `hooks_work`).
pub struct Op {
    pub op: &'static str,
    pub method: &'static str,
    pub args: &'static [&'static str],
}

const fn op(op: &'static str, method: &'static str, args: &'static [&'static str]) -> Op {
    Op { op, method, args }
}

/// The browser's ops. Each new op comes with a test
/// (`crates/daemon/tests/web_api.rs`).
pub const OPS: &[Op] = &[
    op("snapshot", "", &["view", "rev"]),
    op("session.get", "session.get", &["ref"]),
    // Opening the overlord's pane when the user asks (never by itself).
    op("overlord.ensure", "overlord.ensure", &[]),
    // A stopped session's Resume: started on its conversation (an
    // overlord is ensured), as Enter in the list does.
    op("session.resume", "", &["ref"]),
    op("session.restart", "session.restart", &["ref"]),
    op("session.stop", "session.stop", &["ref"]),
    // The "tell the overlord" box: typed into the overlord's pane.
    op("overlord.ask", "overlord.ask", &["text"]),
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
];

/// Method families no op may name, whatever `OPS` says: a second check.
const FORBIDDEN: &[&str] = &[
    "admin.", "mcp.", "hook.", "web.", "config.", "ui.", "tmux.", "setup.",
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
    Ok(json!({"protocol": plugin::WEB_PROTOCOL, "version": env!("CARGO_PKG_VERSION")}))
}

/// `web.call {op, args}`: one browser request.
pub fn call(d: &Daemon, params: &Value) -> Result<Value> {
    let op = params["op"].as_str().unwrap_or("");
    let Some(found) = OPS.iter().find(|o| o.op == op) else {
        return Err(NotAllowed(op.to_string()).into());
    };
    if FORBIDDEN.iter().any(|f| found.method.starts_with(f)) {
        return Err(NotAllowed(op.to_string()).into());
    }
    let mut args = serde_json::Map::new();
    if let Some(given) = params["args"].as_object() {
        for k in found.args {
            if let Some(v) = given.get(*k) {
                args.insert((*k).to_string(), v.clone());
            }
        }
    }
    let args = Value::Object(args);
    match op {
        "snapshot" => snapshot(d, &args),
        "session.resume" => resume(d, &args),
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

/// Op `snapshot {view, rev}`: everything the list draws, in one answer.
/// `view` is a `session.list` query, whose `search` may hold the TUI's
/// filter tokens (`search_tokens`); a `rev` that is still current gets
/// `{"unchanged": true}`. Grouping and labels are worked out here: the
/// page only draws.
pub fn snapshot(d: &Daemon, args: &Value) -> Result<Value> {
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
    if let Some(text) = view
        .get("search")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
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
    Ok(json!({
        "rev": rev,
        "rows": rows,
        "groups": groups,
        "overlord": o,
        "statuses": statuses,
    }))
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

/// `web.pane_target {target}`: the tmux session a pane shows. `overlord`
/// is the active overlord; anything else a session reference. Starts
/// nothing: a session that isn't running says so, and the page offers
/// Resume.
pub fn pane_target(d: &Daemon, params: &Value) -> Result<Value> {
    let target = methods::p_str(params, "target")?;
    let s = {
        let db = d.db();
        if target == "overlord" {
            overlord::session(&db)?.context("no overlord is set up yet")?
        } else {
            db.resolve_ref(target)?
        }
    };
    // (tmux, without the DB guard.)
    let running =
        s.status != Status::Stopped && s.tmux_session.as_deref().is_some_and(tmux::has_session);
    Ok(json!({
        "id": s.id,
        "name": s.name,
        "overlord": overlord::is_overlord_role(&s.role),
        "tmux_session": s.tmux_session,
        "running": running,
    }))
}

// ---- running the plugin ----

/// How long `web.start` waits for the plugin's hello.
const HELLO_WAIT: Duration = Duration::from_secs(5);
/// How long a stopped plugin gets between SIGTERM and SIGKILL.
const TERM_WAIT: Duration = Duration::from_secs(2);
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
    /// Signalled on a hello and on every exit.
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

    /// `web.stop`: SIGTERM, SIGKILL after 2 s; not restarted.
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
        let deadline = Instant::now() + TERM_WAIT;
        while sup.generation == generation && sup.pid.is_some() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                signal(pid, libc::SIGKILL);
                // (Reaped by its thread; wait for that, briefly.)
                let _ = self
                    .moved
                    .wait_timeout(sup, Duration::from_millis(500))
                    .expect("web mutex");
                return;
            }
            sup = self.moved.wait_timeout(sup, left).expect("web mutex").0;
        }
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

/// At daemon start: start the plugin when `[web] enabled` (in the
/// background; a failure is logged and never stops the daemon).
pub fn start_if_enabled(d: &Arc<Daemon>) {
    if !d.config().web.enabled {
        return;
    }
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        if let Err(e) = start(&d) {
            tracing::warn!(error = %e, "[web] enabled, but rlord-web didn't start");
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

    #[test]
    fn ops_are_listed_once() {
        let mut names: Vec<&str> = OPS.iter().map(|o| o.op).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), OPS.len());
    }
}
