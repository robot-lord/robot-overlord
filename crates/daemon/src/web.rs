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
//! See docs/working/web-ui-remote.md, "Daemon RPC additions".

use std::fmt;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

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

/// The browser's ops and the daemon methods they call. `snapshot` is
/// answered here. Each new op comes with a test.
pub const OPS: &[(&str, &str)] = &[
    ("snapshot", ""),
    ("session.get", "session.get"),
    // Opening the overlord's pane when the user asks (never by itself).
    ("overlord.ensure", "overlord.ensure"),
];

/// Method families no op may name, whatever `OPS` says: a second check.
const FORBIDDEN: &[&str] = &[
    "admin.", "mcp.", "hook.", "web.", "config.", "ui.", "tmux.", "setup.",
];

/// `web.hello`: the plugin's first call.
pub fn hello(_d: &Daemon, params: &Value) -> Result<Value> {
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
    Ok(json!({"protocol": plugin::WEB_PROTOCOL, "version": env!("CARGO_PKG_VERSION")}))
}

/// `web.call {op, args}`: one browser request.
pub fn call(d: &Daemon, params: &Value) -> Result<Value> {
    let op = params["op"].as_str().unwrap_or("");
    let args = match &params["args"] {
        Value::Null => json!({}),
        a => a.clone(),
    };
    let Some((_, method)) = OPS.iter().find(|(o, _)| *o == op) else {
        return Err(NotAllowed(op.to_string()).into());
    };
    if FORBIDDEN.iter().any(|f| method.starts_with(f)) {
        return Err(NotAllowed(op.to_string()).into());
    }
    match op {
        "snapshot" => snapshot(d, &args),
        _ => d.rpc(method, args),
    }
}

/// Op `snapshot {view, rev}`: everything the list draws, in one answer.
/// `view` is a `session.list` query; a `rev` that is still current gets
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_op_reaches_a_forbidden_family() {
        for (op, method) in OPS {
            for f in FORBIDDEN {
                assert!(!method.starts_with(f), "{op} -> {method}");
                assert!(!op.starts_with(f), "{op}");
            }
        }
    }
}
