//! The daemon's web API (`web.rs`) over a real socket, against an
//! in-memory database: what the `rlord-web` plugin calls. No tmux (PATH
//! is emptied, and the tmux socket named is a scratch one), no agents.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use rlord_core::config::Config;
use rlord_core::db::{Db, NewSession};
use rlord_core::plugin;
use rlord_daemon::Daemon;

/// The page's copy of a snapshot (`crates/web/ui`): its hand-written types
/// are checked against it, and this file checks it against the daemon.
/// `RLORD_UPDATE_FIXTURES=1` rewrites it from a real snapshot.
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../web/ui/test/fixtures/snapshot.json"
);

/// Once per test binary: no tmux, no agents, no login shell; rlord's
/// folders in a scratch place.
fn env() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("web-api");
        let _ = std::fs::remove_dir_all(&dir);
        let empty = dir.join("empty-path");
        std::fs::create_dir_all(&empty).unwrap();
        // Safety: set once, before any test thread reads them.
        std::env::set_var("PATH", &empty);
        std::env::set_var("RLORD_TMUX_SOCKET", "rlord-test-web-api");
        std::env::set_var("RLORD_NO_LOGIN_SHELL", "1");
        std::env::set_var("XDG_DATA_HOME", dir.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
        std::env::set_var("XDG_RUNTIME_DIR", dir.join("run"));
        // (The repo finder and the code-folder suggestion look in $HOME.)
        std::env::set_var("HOME", dir.join("home"));
        std::fs::create_dir_all(dir.join("home")).unwrap();
        // No web plugin to find (not even one built beside the tests).
        std::env::set_var("RLORD_WEB_BIN", dir.join("no-plugin"));
        dir
    })
}

struct Running {
    daemon: Arc<Daemon>,
    sock: PathBuf,
    _dir: tempfile::TempDir,
}

fn add(db: &Db, id: &str, role: &str, tmux: Option<&str>) {
    db.insert_session(&NewSession {
        id,
        name: id,
        agent: "claude",
        agent_session: Some(id),
        project_path: "/tmp",
        tmux_session: tmux,
        launch_spec: "{}",
        created_at: 1,
        repo_path: None,
        workspace_path: None,
        branch: Some("main"),
        role,
    })
    .unwrap();
}

/// Tests that write config.toml (one scratch file for them all) take
/// turns.
static CONFIG: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A daemon with an overlord and three sessions (two tagged `web`, one
/// also `wip`), serving on a temp socket.
fn start() -> Running {
    start_with(|_| {})
}

/// `start`, with `more` done to the database first.
fn start_with(more: impl FnOnce(&Db)) -> Running {
    env();
    let db = Db::open_in_memory().unwrap();
    add(&db, "overlord-1", "overlord", Some("overlord-rlord"));
    for id in ["alpha", "beta", "gamma"] {
        add(&db, id, "", None);
    }
    db.add_tag("alpha", "web").unwrap();
    db.add_tag("beta", "web").unwrap();
    db.add_tag("beta", "wip").unwrap();
    db.set_group_order(&["wip".to_string()]).unwrap();
    more(&db);
    let daemon = Daemon::new(db, Config::default());
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let (tx, rx) = std::sync::mpsc::channel();
    let (d2, s2) = (daemon.clone(), sock.clone());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = Daemon::bind(&s2).unwrap();
            tx.send(()).unwrap();
            let _ = d2.serve(listener).await;
        });
    });
    rx.recv().unwrap();
    Running {
        daemon,
        sock,
        _dir: dir,
    }
}

/// One request; the whole response (`result` or `error`).
fn raw(sock: &Path, method: &str, params: Value) -> Value {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    writeln!(
        s,
        "{}",
        json!({"id": 7, "method": method, "params": params})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn ok(sock: &Path, method: &str, params: Value) -> Value {
    let r = raw(sock, method, params);
    assert!(r.get("error").is_none(), "{method}: {r}");
    r["result"].clone()
}

/// A local page's request (no page id).
fn op(sock: &Path, op: &str, args: Value) -> Value {
    raw(
        sock,
        "web.call",
        json!({"op": op, "args": args, "via": "local"}),
    )
}

#[test]
fn web_call_reaches_its_ops_and_nothing_else() {
    let r = start();
    // An op on the list reaches the real method.
    let got = op(&r.sock, "session.get", json!({"ref": "alpha"}));
    assert_eq!(got["result"]["id"], "alpha", "{got}");
    // ... labelled as a row is, with the status the list shows.
    assert_eq!(got["result"]["status_label"], "starting", "{got}");
    assert_eq!(got["result"]["shown_status"], "starting", "{got}");
    // A method error comes back as one.
    let got = op(&r.sock, "session.get", json!({"ref": "nosuch"}));
    assert!(got["error"]["message"].is_string(), "{got}");
    // Every forbidden family, the plugin's own lifecycle methods, and
    // methods simply not on the list: refused, with the typed code.
    for o in [
        "admin.reset",
        "mcp.call",
        "mcp.tools",
        "hook.emit",
        "web.hello",
        "web.call",
        "web.status",
        "web.start",
        "web.stop",
        "web.rotate",
        "web.pane_target",
        "config.reload",
        "ui.goto",
        "ui.state",
        "tmux.reload",
        "setup.apply",
        "session.send",
        "session.context_add",
        "session.group",
        "tag.rename",
        "tag.order",
        "overlord.set_model",
        "limits.resume",
        "remote.set",
        "",
    ] {
        let got = op(&r.sock, o, json!({}));
        assert_eq!(
            got["error"]["code"],
            plugin::RPC_ERR_NOT_ALLOWED,
            "{o}: {got}"
        );
    }
    // Nothing changed on the way.
    assert_eq!(
        ok(&r.sock, "session.get", json!({"ref": "alpha"}))["status"],
        "starting"
    );
}

/// A method's own error (not the allow-list's refusal).
fn method_error(got: &Value) -> String {
    assert!(got["error"].is_object(), "{got}");
    assert_ne!(got["error"]["code"], plugin::RPC_ERR_NOT_ALLOWED, "{got}");
    got["error"]["message"].as_str().unwrap_or("").to_string()
}

#[test]
fn actions_reach_their_methods() {
    let r = start();
    let get = |id: &str| ok(&r.sock, "session.get", json!({"ref": id}));
    let done = |o: &str, args: Value| {
        let got = op(&r.sock, o, args);
        assert!(got.get("error").is_none(), "{o}: {got}");
        got["result"].clone()
    };
    // Rename, describe, notes, tags, pin.
    done(
        "session.rename",
        json!({"ref": "alpha", "name": "alpha-two"}),
    );
    assert_eq!(get("alpha")["name"], "alpha-two");
    done(
        "session.describe",
        json!({"ref": "alpha", "description": " the web UI "}),
    );
    assert_eq!(get("alpha")["description"], "the web UI");
    done(
        "session.notes",
        json!({"ref": "alpha", "notes": "line one\nline two\n"}),
    );
    assert_eq!(get("alpha")["notes"], "line one\nline two");
    done(
        "session.tag",
        json!({"ref": "alpha", "add": ["ui"], "remove": ["web"]}),
    );
    assert_eq!(get("alpha")["tags"], json!(["ui"]));
    done("session.pin", json!({"ref": "alpha", "pinned": true}));
    assert_eq!(get("alpha")["pinned"], true);
    done("session.pin", json!({"ref": "alpha", "pinned": false}));
    assert_eq!(get("alpha")["pinned"], false);
    // The overlord's row stays fixed, as everywhere else.
    let got = op(
        &r.sock,
        "session.rename",
        json!({"ref": "overlord-1", "name": "x"}),
    );
    assert!(method_error(&got).contains("overlord"), "{got}");
    // Stop; an argument that isn't the op's own is dropped (the daemon
    // sets `interrupt` itself).
    let stopped = done(
        "session.stop",
        json!({"ref": "beta", "interrupt": "still_running"}),
    );
    assert_eq!(stopped["stopped"], "beta", "{stopped}");
    assert!(stopped.get("note").is_none(), "{stopped}");
    assert_eq!(get("beta")["status"], "stopped");
    // Archive and unarchive.
    done("session.archive", json!({"ref": "gamma"}));
    assert!(get("gamma")["archived_at"].is_number());
    done("session.unarchive", json!({"ref": "gamma"}));
    assert!(get("gamma")["archived_at"].is_null());
    // These start agents: here (no tmux, no agent) each reaches its method
    // and fails there.
    for (o, args) in [
        ("session.resume", json!({"ref": "beta"})),
        ("session.resume", json!({"ref": "nosuch"})),
        ("session.restart", json!({"ref": "beta"})),
        ("overlord.switch", json!({"agent": "nosuch"})),
    ] {
        method_error(&op(&r.sock, o, args));
    }
    // An archived session isn't resumed.
    done("session.archive", json!({"ref": "gamma"}));
    let got = op(&r.sock, "session.resume", json!({"ref": "gamma"}));
    assert!(method_error(&got).contains("unarchive"), "{got}");
    // Telling the overlord needs something to say (with text it would
    // start the overlord).
    let got = op(&r.sock, "overlord.ask", json!({"text": "  "}));
    assert!(method_error(&got).contains("nothing to send"), "{got}");
}

/// The page's list of ops (`crates/web/ui/src/proto.ts`, `OPS`) is the
/// daemon's: a button can't send an op the daemon refuses, and an op added
/// here is typed there.
#[test]
fn the_pages_ops_are_the_daemons() {
    let proto = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../web/ui/src/proto.ts"
    ))
    .unwrap();
    let start = proto.find("export const OPS = [").expect("OPS in proto.ts");
    let body = &proto[start..];
    let body = &body[..body.find("] as const").expect("end of OPS")];
    let page: Vec<&str> = body.split('"').skip(1).step_by(2).collect();
    let daemon: Vec<&str> = rlord_daemon::web::OPS.iter().map(|o| o.op).collect();
    assert_eq!(page, daemon);
}

#[test]
fn hello_refuses_another_protocol() {
    let r = start();
    let hello = |p: u32| {
        raw(
            &r.sock,
            "web.hello",
            json!({"protocol": p, "version": "x", "pid": 1, "port": 0}),
        )
    };
    let got = hello(plugin::WEB_PROTOCOL);
    assert_eq!(got["result"]["protocol"], plugin::WEB_PROTOCOL, "{got}");
    assert_eq!(got["result"]["version"], env!("CARGO_PKG_VERSION"));
    // The remote API level, which the plugin puts in each device's hello.
    assert_eq!(got["result"]["api"], rlord_daemon::web::API, "{got}");
    let got = hello(plugin::WEB_PROTOCOL + 1);
    assert_eq!(got["error"]["code"], plugin::RPC_ERR_INCOMPATIBLE, "{got}");
    assert!(got["error"]["message"]
        .as_str()
        .unwrap()
        .contains("rlord web install"));
}

/// The fields of `a` and `b` agree: objects have the same keys, arrays'
/// first items agree, scalars have the same type. A null matches anything
/// (a field that may be empty). Returns where they differ.
fn shape_diff(a: &Value, b: &Value, at: &str) -> Vec<String> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => vec![],
        (Value::Object(x), Value::Object(y)) => {
            let mut d = vec![];
            for k in x.keys().filter(|k| !y.contains_key(*k)) {
                d.push(format!("{at}.{k}: only in the daemon's"));
            }
            for k in y.keys().filter(|k| !x.contains_key(*k)) {
                d.push(format!("{at}.{k}: only in the fixture"));
            }
            for (k, v) in x {
                if let Some(w) = y.get(k) {
                    d.extend(shape_diff(v, w, &format!("{at}.{k}")));
                }
            }
            d
        }
        (Value::Array(x), Value::Array(y)) => match (x.first(), y.first()) {
            (Some(v), Some(w)) => shape_diff(v, w, &format!("{at}[0]")),
            _ => vec![],
        },
        (Value::Bool(_), Value::Bool(_))
        | (Value::Number(_), Value::Number(_))
        | (Value::String(_), Value::String(_)) => vec![],
        _ => vec![format!("{at}: {a} vs {b}")],
    }
}

#[test]
fn snapshot_has_the_pages_shape_and_says_unchanged() {
    let r = start();
    let snap = op(&r.sock, "snapshot", json!({"view": {"archived": "live"}}))["result"].clone();
    if std::env::var("RLORD_UPDATE_FIXTURES").as_deref() == Ok("1") {
        std::fs::create_dir_all(Path::new(FIXTURE).parent().unwrap()).unwrap();
        let text = serde_json::to_string_pretty(&snap).unwrap() + "\n";
        std::fs::write(FIXTURE, text).unwrap();
    }
    let fixture: Value =
        serde_json::from_str(&std::fs::read_to_string(FIXTURE).expect("the snapshot fixture"))
            .unwrap();
    let diff = shape_diff(&snap, &fixture, "snapshot");
    assert!(
        diff.is_empty(),
        "the snapshot changed shape: update crates/web/ui/src/proto.ts, then \
         RLORD_UPDATE_FIXTURES=1 cargo test -p robot-overlord-daemon --test web_api\n{}",
        diff.join("\n")
    );
    // What it holds: the rows (no overlord), labels, groups in their
    // order with counts and favourites, the overlord with its roster.
    let ids: Vec<&str> = snap["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 3, "{ids:?}");
    assert!(!ids.contains(&"overlord-1"));
    assert_eq!(snap["rows"][0]["status_label"], "starting");
    let groups: Vec<(String, u64, bool)> = snap["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["label"].as_str().unwrap().to_string(),
                g["count"].as_u64().unwrap(),
                g["favourite"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        [
            ("#wip".to_string(), 1, true),
            ("#web".to_string(), 2, false),
            ("untagged".to_string(), 1, false),
        ]
    );
    assert_eq!(snap["overlord"]["id"], "overlord-1");
    assert!(snap["overlord"]["overlords"].as_array().unwrap().len() >= 2);
    assert!(snap["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["status"] == "waiting_input" && s["label"] == "needs you"));
    // The same rev again: nothing to send.
    let rev = snap["rev"].as_str().unwrap();
    let again = op(&r.sock, "snapshot", json!({"view": {}, "rev": rev}))["result"].clone();
    assert_eq!(again, json!({"unchanged": true, "rev": rev}));
    // A view is a session.list query.
    let tagged = op(&r.sock, "snapshot", json!({"view": {"tag": "wip"}}))["result"].clone();
    assert_eq!(tagged["rows"].as_array().unwrap().len(), 1);
    // The filter box's text takes the TUI's tokens; the rest is free text.
    let ids = |search: &str| -> Vec<String> {
        let s = op(&r.sock, "snapshot", json!({"view": {"search": search}}))["result"].clone();
        let mut v: Vec<String> = s["rows"]
            .as_array()
            .unwrap_or_else(|| panic!("{search}: {s}"))
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    assert_eq!(ids("tag:wip"), ["beta"]);
    assert_eq!(ids("tag:web alpha"), ["alpha"]);
    assert_eq!(ids("status:starting"), ["alpha", "beta", "gamma"]);
    assert_eq!(ids("status:turn"), Vec::<String>::new());
    assert_eq!(ids("status:bg,limited"), Vec::<String>::new());
    assert_eq!(ids("gamma"), ["gamma"]);
    let bad = op(&r.sock, "snapshot", json!({"view": "everything"}));
    assert!(bad["error"].is_object(), "{bad}");
}

#[test]
fn rev_moves_on_a_change_and_never_repeats_across_a_restart() {
    let r = start();
    let rev0 = r.daemon.rev();
    ok(
        &r.sock,
        "session.describe",
        json!({"ref": "alpha", "description": "first"}),
    );
    let rev1 = r.daemon.rev();
    assert_ne!(rev0, rev1);
    let snap = op(&r.sock, "snapshot", json!({"rev": rev0}))["result"].clone();
    assert_eq!(snap["rev"], rev1.as_str());
    assert!(snap.get("unchanged").is_none());
    // A read changes nothing.
    ok(&r.sock, "session.list", json!({}));
    assert_eq!(r.daemon.rev(), rev1);
    // Another daemon (a restart): same count, another rev.
    let other = start();
    assert_ne!(other.daemon.rev(), rev0);
    assert_ne!(other.daemon.rev().split('-').next(), rev0.split('-').next());
}

#[test]
fn web_subscribe_coalesces_changes_and_carries_the_rev() {
    let r = start();
    let s = UnixStream::connect(&r.sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut w = s.try_clone().unwrap();
    writeln!(w, "{}", json!({"id": 1, "method": "web.subscribe"})).unwrap();
    let mut lines = BufReader::new(s).lines();
    let ack: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(ack["result"]["subscribed"], true);
    assert_eq!(ack["result"]["rev"], r.daemon.rev().as_str());
    // A burst of changes (more than the channel holds, so the subscriber
    // lags too).
    let began = Instant::now();
    for i in 0..100 {
        ok(
            &r.sock,
            "session.describe",
            json!({"ref": "alpha", "description": format!("d{i}")}),
        );
    }
    let last = r.daemon.rev();
    let mut got: Vec<Value> = vec![];
    while got.last().map(|v| v["rev"] != last.as_str()) != Some(false) {
        let line = lines
            .next()
            .expect("stream open")
            .expect("a notify in time");
        got.push(serde_json::from_str(&line).unwrap());
    }
    let took = began.elapsed();
    // Folded: at most one notice per 100 ms (plus the first, sent at
    // once), each `changed` with a rev, the last with the final rev.
    let most = took.as_millis() / 100 + 2;
    assert!(
        got.len() as u128 <= most,
        "{} notices in {took:?}",
        got.len()
    );
    for v in &got {
        assert_eq!(v["notify"], "changed", "{v}");
        assert!(v["rev"].is_string(), "{v}");
    }
}

/// The plugin's own relay status moves the rev, marked `relay` so the
/// plugin wakes no device for it; any other change folded in with it
/// unmarks it.
#[test]
fn web_subscribe_marks_a_relay_status_change() {
    let r = start();
    let s = UnixStream::connect(&r.sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut w = s.try_clone().unwrap();
    writeln!(w, "{}", json!({"id": 1, "method": "web.subscribe"})).unwrap();
    let mut lines = BufReader::new(s).lines();
    lines.next().unwrap().unwrap();
    let mut until_rev = |rev: String| -> Value {
        loop {
            let v: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            if v["rev"] == rev.as_str() {
                return v;
            }
        }
    };
    let state =
        |connected: bool| json!({"relay": "https://relay.example.com", "connected": connected});
    ok(&r.sock, "web.remote_state", state(true));
    let v = until_rev(r.daemon.rev());
    assert_eq!(v["relay"], true, "{v}");
    // The same state again: nothing moved.
    let rev = r.daemon.rev();
    ok(&r.sock, "web.remote_state", state(true));
    assert_eq!(r.daemon.rev(), rev);
    ok(
        &r.sock,
        "session.describe",
        json!({"ref": "alpha", "description": "d"}),
    );
    let v = until_rev(r.daemon.rev());
    assert!(v.get("relay").is_none(), "{v}");
    // Both, folded or not: the last notice isn't marked.
    ok(&r.sock, "web.remote_state", state(false));
    ok(
        &r.sock,
        "session.describe",
        json!({"ref": "alpha", "description": "e"}),
    );
    let v = until_rev(r.daemon.rev());
    assert!(v.get("relay").is_none(), "{v}");
}

#[test]
fn pane_target_finds_the_pane_and_starts_nothing() {
    let r = start();
    let o = ok(&r.sock, "web.pane_target", json!({"target": "overlord"}));
    assert_eq!(o["id"], "overlord-1");
    assert_eq!(o["overlord"], true);
    assert_eq!(o["tmux_session"], "overlord-rlord");
    // (No tmux server here: not running, and nothing was started.)
    assert_eq!(o["running"], false);
    let a = ok(&r.sock, "web.pane_target", json!({"target": "alpha"}));
    assert_eq!(
        (a["id"].as_str(), a["running"].as_bool()),
        (Some("alpha"), Some(false))
    );
    assert!(a["tmux_session"].is_null());
    let missing = raw(&r.sock, "web.pane_target", json!({"target": "nosuch"}));
    assert!(missing["error"].is_object(), "{missing}");
    let rev = r.daemon.rev();
    ok(&r.sock, "web.pane_target", json!({"target": "beta"}));
    assert_eq!(r.daemon.rev(), rev, "a look-up changed something");
    assert_eq!(
        ok(&r.sock, "session.get", json!({"ref": "overlord-1"}))["status"],
        "starting"
    );
}

/// A browser request from page `page` (the plugin stamps the id).
fn op_from(sock: &Path, page: &str, op: &str, args: Value) -> Value {
    raw(
        sock,
        "web.call",
        json!({"op": op, "args": args, "via": "local", "page": page}),
    )
}

fn ui_state(sock: &Path) -> Value {
    ok(sock, "ui.state", json!({}))
}

/// Slice 2: the plugin names the open pages, each reports what it shows,
/// and with no terminal (no tmux here) the page is where UI commands go.
#[test]
fn pages_register_report_and_drop() {
    let r = start();
    // No page, no terminal.
    let s = ui_state(&r.sock);
    assert_eq!((&s["target"], &s["web"]), (&json!("none"), &Value::Null));
    ok(
        &r.sock,
        "web.pages",
        json!({"pages": ["p1-aaaa", "p2-bbbb"]}),
    );
    let s = ui_state(&r.sock);
    assert_eq!(s["target"], "web");
    // Neither used yet: the newest.
    assert_eq!(s["web"]["pages"], 2);
    assert_eq!(s["web"]["showing"]["kind"], "session_list");
    // p1 reports a session, its view and a form, with input: it is the
    // most recent now.
    let rep = op_from(
        &r.sock,
        "p1-aaaa",
        "page.report",
        json!({"route": {"kind": "session", "id": "alpha"},
               "view": {"query": "tag:web", "sort": "name"},
               "visible": true, "form": "manual", "input": true,
               // Not the browser's to set: dropped.
               "page": "p2-bbbb", "last_input": 1}),
    );
    assert!(rep["error"].is_null(), "{rep}");
    let s = ui_state(&r.sock);
    assert_eq!(
        s["web"]["showing"],
        json!({"kind": "session", "id": "alpha", "name": "alpha"})
    );
    assert_eq!(
        (&s["web"]["visible"], &s["web"]["form"]),
        (&json!(true), &json!("manual"))
    );
    assert_eq!(s["current_view"]["query"], "tag:web");
    assert_eq!(s["current_view"]["sort"], "name");
    assert_eq!(s["current_view"]["tab"], "sessions");
    // A bad route or form is refused; a page that isn't open can't report.
    let bad = op_from(
        &r.sock,
        "p1-aaaa",
        "page.report",
        json!({"route": {"kind": "admin"}}),
    );
    assert!(bad["error"].is_object(), "{bad}");
    let bad = op_from(
        &r.sock,
        "p1-aaaa",
        "page.report",
        json!({"form": "anything"}),
    );
    assert!(bad["error"].is_object(), "{bad}");
    let bad = op_from(&r.sock, "p9-zzzz", "page.report", json!({"visible": true}));
    assert!(bad["error"].is_object(), "{bad}");
    // A user-driven op from p2 makes it the most recent (a second later:
    // whole seconds); a look-up doesn't.
    std::thread::sleep(Duration::from_millis(1100));
    op_from(&r.sock, "p2-bbbb", "snapshot", json!({}));
    assert_eq!(ui_state(&r.sock)["web"]["form"], "manual");
    op_from(
        &r.sock,
        "p2-bbbb",
        "session.pin",
        json!({"ref": "gamma", "pinned": true}),
    );
    let s = ui_state(&r.sock);
    assert_eq!(
        (&s["web"]["showing"]["kind"], &s["web"]["form"]),
        (&json!("session_list"), &Value::Null)
    );
    // p2 closes: p1 again. Then both.
    ok(&r.sock, "web.pages", json!({"pages": ["p1-aaaa"]}));
    assert_eq!(ui_state(&r.sock)["web"]["form"], "manual");
    ok(&r.sock, "web.pages", json!({"pages": []}));
    assert_eq!(ui_state(&r.sock)["target"], "none");
}

/// The pages go when the plugin's `web.subscribe` stream does.
#[test]
fn pages_go_with_the_plugin() {
    let r = start();
    let s = UnixStream::connect(&r.sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut w = s.try_clone().unwrap();
    writeln!(w, "{}", json!({"id": 1, "method": "web.subscribe"})).unwrap();
    let mut lines = BufReader::new(s).lines();
    lines.next().unwrap().unwrap();
    ok(&r.sock, "web.pages", json!({"pages": ["p1-aaaa"]}));
    assert_eq!(ui_state(&r.sock)["target"], "web");
    drop(lines);
    drop(w);
    // (The daemon finds the stream gone on its next write.)
    let start = Instant::now();
    loop {
        ok(
            &r.sock,
            "session.describe",
            json!({"ref": "alpha", "description": "x"}),
        );
        if ui_state(&r.sock)["target"] == "none" {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "pages kept");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The overlord's tool call, as `rlord mcp --overlord` makes it.
fn tool(sock: &Path, name: &str, args: Value) -> Value {
    raw(
        sock,
        "mcp.call",
        json!({"name": name, "arguments": args, "overlord": true}),
    )
}

/// The plugin's `web.subscribe` stream: its UI lines, read as they come.
struct WebStream {
    lines: std::io::Lines<BufReader<UnixStream>>,
    _w: UnixStream,
}

impl WebStream {
    fn open(sock: &Path) -> WebStream {
        let s = UnixStream::connect(sock).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut w = s.try_clone().unwrap();
        writeln!(w, "{}", json!({"id": 1, "method": "web.subscribe"})).unwrap();
        let mut lines = BufReader::new(s).lines();
        lines.next().unwrap().unwrap();
        WebStream { lines, _w: w }
    }

    /// The next UI command (changes skipped).
    fn next_ui(&mut self) -> Value {
        loop {
            let l = self.lines.next().expect("open").expect("a line in time");
            let v: Value = serde_json::from_str(&l).unwrap();
            if v["notify"] == "ui" {
                return v;
            }
        }
    }
}

/// With only a web page open (no tmux here), the overlord's UI tools land
/// on it: each puts its command, addressed to the page, on the plugin's
/// stream and says `on: "web"`. With no page either, they fail naming
/// both ways in.
#[test]
fn the_overlords_ui_tools_land_on_a_page() {
    let r = start();
    for (name, args) in [
        ("show_list", json!({})),
        ("set_list_view", json!({"query": "tag:web"})),
        ("propose_session", json!({"name": "x", "prompt": "p"})),
        ("goto_session", json!({"session": "alpha"})),
        (
            "confirm_new_session",
            json!({"description": "d", "changed": [], "note": "n"}),
        ),
        ("confirm_setup", json!({"code_dir": "/tmp"})),
        ("open_keys", json!({})),
    ] {
        let got = tool(&r.sock, name, args);
        let e = method_error(&got);
        assert!(
            e.contains("rlord web") && e.contains("terminal"),
            "{name}: {e}"
        );
    }
    let mut stream = WebStream::open(&r.sock);
    ok(&r.sock, "web.pages", json!({"pages": ["p1-aaaa"]}));
    let on_web = |got: &Value, name: &str| {
        assert!(got["error"].is_null(), "{name}: {got}");
        let t = got["result"].clone();
        assert_eq!(t["on"], "web", "{name}: {t}");
        t
    };
    on_web(&tool(&r.sock, "show_list", json!({})), "show_list");
    let c = stream.next_ui();
    assert_eq!(
        (&c["page"], &c["cmd"]),
        (&json!("p1-aaaa"), &json!({"cmd": "show_list"}))
    );

    // set_list_view starts from the page's view and leaves the TUI's alone.
    op_from(
        &r.sock,
        "p1-aaaa",
        "page.report",
        json!({"view": {"sort": "name", "tab": "groups"}}),
    );
    let t = on_web(
        &tool(
            &r.sock,
            "set_list_view",
            json!({"query": "tag:web", "select": "beta"}),
        ),
        "set_list_view",
    );
    assert_eq!(
        (&t["view"]["sort"], &t["view"]["tab"]),
        (&json!("name"), &json!("groups"))
    );
    let c = stream.next_ui();
    assert_eq!(c["cmd"]["cmd"], "set_view");
    assert_eq!(c["cmd"]["view"]["query"], "tag:web");
    assert_eq!(c["cmd"]["select"], "beta");
    assert_eq!(stream.next_ui()["cmd"]["cmd"], "show_list");
    let s = ui_state(&r.sock);
    assert_eq!(s["current_view"]["query"], "tag:web");
    // ... show: false sends the view only.
    tool(
        &r.sock,
        "set_list_view",
        json!({"sort": "state", "show": false}),
    );
    let c = stream.next_ui();
    assert_eq!(c["cmd"]["view"]["sort"], "state");
    tool(&r.sock, "show_list", json!({}));
    assert_eq!(stream.next_ui()["cmd"]["cmd"], "show_list");

    // propose_session: the form, with the background kept back by id.
    let t = on_web(
        &tool(
            &r.sock,
            "propose_session",
            json!({"name": "x", "prompt": "do it", "context": "the overlord's note", "related": ["alpha"]}),
        ),
        "propose_session",
    );
    let c = stream.next_ui();
    assert_eq!(c["cmd"]["cmd"], "new_session");
    assert_eq!(c["cmd"]["prefill"]["prompt"], "do it");
    assert_eq!(c["cmd"]["prefill"]["related_names"], json!(["alpha"]));
    let id = c["cmd"]["prefill"]["proposal"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(t["proposed"]["proposal"], id.as_str());

    let t = on_web(
        &tool(
            &r.sock,
            "confirm_new_session",
            json!({"description": "a web thing", "agent": "codex", "changed": ["agent"],
                   "note": "codex, as you said"}),
        ),
        "confirm_new_session",
    );
    let c = stream.next_ui();
    assert_eq!(c["cmd"]["cmd"], "confirm_new_session");
    assert_eq!(c["cmd"]["prefill"], t["shown"]);
    assert_eq!(c["cmd"]["prefill"]["resolved"], json!(["agent"]));

    on_web(
        &tool(
            &r.sock,
            "confirm_setup",
            json!({"code_dir": "/tmp", "note": "n"}),
        ),
        "confirm_setup",
    );
    let c = stream.next_ui();
    assert_eq!(c["cmd"]["cmd"], "setup");
    assert!(
        c["cmd"]["prefill"]["code_dir"]
            .as_str()
            .unwrap()
            .ends_with("tmp"),
        "{c}"
    );
    assert!(c["cmd"]["status"].is_object());

    // The key screen: read-only in the page, and the answer says so.
    let t = on_web(
        &tool(&r.sock, "open_keys", json!({"key": "keys.quit"})),
        "open_keys",
    );
    assert!(t["note"].as_str().unwrap().contains("read-only"), "{t}");
    let c = stream.next_ui();
    assert_eq!(c["cmd"], json!({"cmd": "keys", "row": "keys.quit"}));
    // A page can read the keys.
    let k = op_from(&r.sock, "p1-aaaa", "keys.list", json!({}));
    assert!(
        k["result"]["keys"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "{k}"
    );

    // get_ui_state names the screen.
    let t = tool(&r.sock, "get_ui_state", json!({}))["result"].clone();
    assert_eq!(t["target"], "web");
    assert_eq!(t["web"]["open"], true);
}

/// The filter box's text, sent as `query` (the TUI's view shape), filters
/// as `list_sessions` does; the page's own default view comes from the
/// daemon.
#[test]
fn snapshot_takes_query_and_the_default_view() {
    let r = start();
    let ids = |view: Value| -> Vec<String> {
        let s = op(&r.sock, "snapshot", json!({"view": view}))["result"].clone();
        let mut v: Vec<String> = s["rows"]
            .as_array()
            .unwrap_or_else(|| panic!("{s}"))
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    let listed = |q: &str| -> Vec<String> {
        let got = tool(&r.sock, "list_sessions", json!({"query": q}))["result"].clone();
        let rows = got
            .as_array()
            .cloned()
            .or_else(|| got["sessions"].as_array().cloned())
            .unwrap_or_else(|| panic!("{got}"));
        let mut v: Vec<String> = rows
            .iter()
            .filter_map(|r| r["id"].as_str().map(str::to_string))
            .collect();
        v.sort();
        v
    };
    for q in [
        "tag:web status:starting",
        "tag:wip",
        "tag:web status:turn",
        "gamma",
    ] {
        assert_eq!(ids(json!({"query": q, "usage": null})), listed(q), "{q}");
    }
    assert_eq!(ids(json!({"query": "tag:wip"})), ["beta"]);
    let v = op(&r.sock, "view.default", json!({}))["result"].clone();
    assert_eq!(
        (&v["sort"], &v["tab"]),
        (&json!("updated"), &json!("sessions"))
    );
}

/// `crates/web/ui/test/fixtures/<name>.json`: written from `got` with
/// `RLORD_UPDATE_FIXTURES=1`, else `got` must have its shape. The page's
/// types are checked against these files (`fixture.test.ts`).
fn fixture(name: &str, got: &Value) {
    let path = PathBuf::from(FIXTURE).with_file_name(format!("{name}.json"));
    if std::env::var("RLORD_UPDATE_FIXTURES").as_deref() == Ok("1") {
        // (This machine's scratch folder: not in a committed file.)
        let text = serde_json::to_string_pretty(got)
            .unwrap()
            .replace(env().to_str().unwrap(), "/scratch");
        std::fs::write(&path, text + "\n").unwrap();
    }
    let want: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("fixture {}", path.display())),
    )
    .unwrap();
    let diff = shape_diff(got, &want, name);
    assert!(
        diff.is_empty(),
        "{name} changed shape: update crates/web/ui/src/proto.ts, then \
         RLORD_UPDATE_FIXTURES=1 cargo test -p robot-overlord-daemon --test web_api\n{}",
        diff.join("\n")
    );
}

/// The forms' ops: each reaches its function with only its arguments,
/// and their answers (and one of each UI command) are the page's
/// fixtures.
#[test]
fn the_forms_ops_and_fixtures() {
    let r = start();
    let res = |o: &str, args: Value| {
        let got = op(&r.sock, o, args);
        assert!(got["error"].is_null(), "{o}: {got}");
        got["result"].clone()
    };
    let defaults = res("form.defaults", json!({"no_listing": false}));
    assert!(
        defaults["agents"].is_object() || defaults["agents"].is_array(),
        "{defaults}"
    );
    fixture("form_defaults", &defaults);
    // A repo for the finder to find (a folder with `.git`: no git here).
    std::fs::create_dir_all(env().join("home/src/demo/.git")).unwrap();
    let mut repos = res("form.repos", json!({}));
    let began = Instant::now();
    while repos["repos"].as_array().is_none_or(Vec::is_empty)
        && began.elapsed() < Duration::from_secs(5)
    {
        std::thread::sleep(Duration::from_millis(50));
        repos = res("form.repos", json!({}));
    }
    assert!(repos["repos"][0]["path"].is_string(), "{repos}");
    fixture("form_repos", &repos);
    let setup = res("form.setup", json!({"launch_dir": "/etc"}));
    assert!(setup["status"]["needed"].is_boolean(), "{setup}");
    assert!(setup["suggest"].is_object(), "{setup}");
    fixture("form_setup", &setup);

    // form.setup_save: `skip` isn't the browser's (dropped, so this asks
    // for a folder); a folder is saved to config.toml (a scratch one).
    let config = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    let e = method_error(&op(&r.sock, "form.setup_save", json!({"skip": true})));
    assert!(e.contains("code_dir"), "{e}");
    let code = env().join("home").join("code");
    let e = method_error(&op(
        &r.sock,
        "form.setup_save",
        json!({"code_dir": code.to_str().unwrap()}),
    ));
    assert!(e.contains("create it?"), "{e}");
    let saved = res(
        "form.setup_save",
        json!({"code_dir": code.to_str().unwrap(), "roots": "follow", "create": true}),
    );
    assert_eq!(saved["created"], true, "{saved}");
    assert!(code.is_dir());
    let cfg = std::fs::read_to_string(saved["config"].as_str().unwrap()).unwrap();
    assert!(cfg.contains("code_dir"), "{cfg}");
    drop(config);

    // The launches and the requests reach their methods (no tmux or
    // overlord here: they fail there, not at the allow-list).
    for (o, args) in [
        (
            "session.new",
            json!({"project": "/tmp", "name": "n", "agent": "claude"}),
        ),
        ("session.fork", json!({"ref": "alpha"})),
        (
            "overlord.new_session",
            json!({"description": "a thing", "project": "/tmp"}),
        ),
        (
            "overlord.setup_request",
            json!({"description": "my code is in ~/src"}),
        ),
    ] {
        method_error(&op(&r.sock, o, args));
    }
    // A proposal the daemon doesn't have is refused before anything starts.
    let e = method_error(&op(
        &r.sock,
        "session.new",
        json!({"project": "/tmp", "proposal": "p99"}),
    ));
    assert!(e.contains("too old"), "{e}");

    // One of each UI command, as a page gets them.
    let mut stream = WebStream::open(&r.sock);
    ok(&r.sock, "web.pages", json!({"pages": ["p1-aaaa"]}));
    let mut cmds = serde_json::Map::new();
    let mut take = |stream: &mut WebStream, want: &str| {
        let c = stream.next_ui();
        assert_eq!(c["cmd"]["cmd"], want, "{c}");
        cmds.insert(want.to_string(), c["cmd"].clone());
    };
    tool(&r.sock, "show_list", json!({}));
    take(&mut stream, "show_list");
    tool(
        &r.sock,
        "set_list_view",
        json!({"query": "tag:web", "select": "alpha", "show": false}),
    );
    take(&mut stream, "set_view");
    tool(
        &r.sock,
        "propose_session",
        json!({"project": "/tmp", "name": "n", "agent": "claude", "model": "opus", "branch": "b",
               "prompt": "p", "context": "c", "related": ["alpha"], "workspace": false,
               "base": "main", "prefer": "local"}),
    );
    take(&mut stream, "new_session");
    tool(
        &r.sock,
        "confirm_new_session",
        json!({"description": "d", "project": "/tmp", "agent": "codex", "model": "", "workspace": false,
               "tags": ["web"], "base": "main", "prefer": "local", "changed": ["agent"], "note": "n"}),
    );
    take(&mut stream, "confirm_new_session");
    tool(
        &r.sock,
        "confirm_setup",
        json!({"code_dir": "/tmp", "repo_roots": ["/tmp"], "description": "d", "changed": ["code_dir"], "note": "n"}),
    );
    take(&mut stream, "setup");
    tool(&r.sock, "open_keys", json!({"key": "keys.quit"}));
    take(&mut stream, "keys");
    // goto (the session isn't running and can't be started here, so the
    // command is written as goto_session sends it) and the notice.
    cmds.insert(
        "goto".into(),
        json!({"cmd": "goto", "id": "alpha", "overlord": false}),
    );
    cmds.insert(
        "notice".into(),
        json!({"cmd": "notice", "text": "the overlord opened a new-session form in the terminal"}),
    );
    fixture("ui_cmds", &Value::Object(cmds));
}

// ---- slice 3: remote access ----

/// A paired device's request, as the plugin passes it on.
fn op_remote(sock: &Path, device: &str, op: &str, args: Value) -> Value {
    raw(
        sock,
        "web.call",
        json!({"op": op, "args": args, "via": "remote", "device": device}),
    )
}

fn not_allowed(got: &Value) -> String {
    assert_eq!(got["error"]["code"], plugin::RPC_ERR_NOT_ALLOWED, "{got}");
    got["error"]["message"].as_str().unwrap().to_string()
}

/// Every `web.call` says who asked; a device reaches only the ops marked
/// `remote`, with only their arguments, and never as a page.
#[test]
fn a_device_reaches_only_the_remote_ops() {
    let r = start();
    // No `via`, another one, or a device without its id: refused.
    for params in [
        json!({"op": "session.get", "args": {"ref": "alpha"}}),
        json!({"op": "session.get", "args": {"ref": "alpha"}, "via": "phone"}),
        json!({"op": "session.get", "args": {"ref": "alpha"}, "via": "remote"}),
        json!({"op": "session.get", "args": {"ref": "alpha"}, "via": "remote", "device": ""}),
    ] {
        let m = not_allowed(&raw(&r.sock, "web.call", params.clone()));
        assert!(m.contains("via"), "{params}: {m}");
    }
    // Each local-only op: refused from a device, before anything runs.
    let local: Vec<&str> = rlord_daemon::web::OPS
        .iter()
        .filter(|o| !o.remote)
        .map(|o| o.op)
        .collect();
    assert_eq!(
        local,
        [
            "keys.list",
            "form.setup",
            "form.setup_save",
            "overlord.setup_request",
            "device.list",
            "device.pair",
            "device.pair_cancel",
            "device.confirm",
            "device.reject",
            "device.revoke",
        ]
    );
    for o in &local {
        let m = not_allowed(&op_remote(&r.sock, "ab12cd34", o, json!({})));
        assert!(m.contains("remote device"), "{o}: {m}");
    }
    // Each remote op reaches its method (here, with nothing to run, many
    // fail there: never at the allow-list).
    for o in rlord_daemon::web::OPS.iter().filter(|o| o.remote) {
        let got = op_remote(&r.sock, "ab12cd34", o.op, json!({}));
        assert_ne!(
            got["error"]["code"],
            plugin::RPC_ERR_NOT_ALLOWED,
            "{}: {got}",
            o.op
        );
    }
    let got = op_remote(&r.sock, "ab12cd34", "session.get", json!({"ref": "alpha"}));
    assert_eq!(got["result"]["id"], "alpha", "{got}");
    let got = op_remote(
        &r.sock,
        "ab12cd34",
        "session.describe",
        json!({"ref": "alpha", "description": "from the phone"}),
    );
    assert!(got["error"].is_null(), "{got}");
    assert_eq!(
        ok(&r.sock, "session.get", json!({"ref": "alpha"}))["description"],
        "from the phone"
    );
    // Only its arguments: the daemon's own `interrupt` is dropped.
    let stopped = op_remote(
        &r.sock,
        "ab12cd34",
        "session.stop",
        json!({"ref": "beta", "interrupt": "still_running"}),
    );
    assert_eq!(stopped["result"]["stopped"], "beta", "{stopped}");
    assert!(stopped["result"].get("note").is_none(), "{stopped}");
    // A device's snapshot has no relay state; a page's has.
    let snap = op_remote(&r.sock, "ab12cd34", "snapshot", json!({}))["result"].clone();
    assert!(
        snap["rows"].is_array() && snap.get("relay").is_none(),
        "{snap}"
    );
    let snap = op(&r.sock, "snapshot", json!({}))["result"].clone();
    assert_eq!(snap["relay"]["devices"], 0, "{snap}");

    // A device is never a page: a page id passed along is ignored, and
    // its ops don't count as use of that page (a device not paired, as
    // here, isn't a surface at all).
    ok(
        &r.sock,
        "web.pages",
        json!({"pages": ["p1-aaaa", "p2-bbbb"]}),
    );
    op_from(
        &r.sock,
        "p1-aaaa",
        "page.report",
        json!({"form": "manual", "input": true}),
    );
    std::thread::sleep(Duration::from_millis(1100));
    let got = raw(
        &r.sock,
        "web.call",
        json!({"op": "session.pin", "args": {"ref": "gamma", "pinned": true},
               "via": "remote", "device": "ab12cd34", "page": "p2-bbbb"}),
    );
    assert!(got["error"].is_null(), "{got}");
    assert_eq!(ui_state(&r.sock)["web"]["form"], "manual");
}

/// The user's relay commands are the socket's alone: no op names them, no
/// MCP tool reaches them, and `relay.` is a forbidden family.
#[test]
fn relay_commands_are_the_users_only() {
    let r = start();
    for m in [
        "relay.status",
        "relay.setup",
        "relay.pair",
        "relay.pair_cancel",
        "relay.confirm",
        "relay.reject",
        "relay.devices",
        "relay.revoke",
        "relay.disable",
    ] {
        not_allowed(&op(&r.sock, m, json!({})));
        not_allowed(&op_remote(&r.sock, "ab12cd34", m, json!({})));
        let got = tool(&r.sock, m, json!({}));
        assert!(got["error"].is_object(), "{m}: {got}");
    }
    // No tool's code names one (the tools reach methods through
    // `Daemon::rpc`, which has none of them either: a unit test).
    let mcp = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/mcp.rs")).unwrap();
    assert!(!mcp.contains("\"relay."), "mcp.rs names a relay.* method");
    let tools = ok(&r.sock, "mcp.tools", json!({"overlord": true}));
    assert!(!tools.to_string().contains("relay."), "{tools}");
    // The socket answers them. With no plugin: what to install.
    let st = ok(&r.sock, "relay.status", json!({}));
    assert_eq!(
        (&st["enabled"], &st["reported"], &st["plugin"]["installed"]),
        (&json!(false), &json!(false), &json!(false)),
        "{st}"
    );
    let e = method_error(&raw(&r.sock, "relay.confirm", json!({})));
    assert!(e.contains("rlord web install"), "{e}");
    let e = method_error(&raw(&r.sock, "relay.pair", json!({})));
    assert!(e.contains("rlord relay setup"), "{e}");
}

/// A stand-in for the plugin: subscribes, answers each relay command it
/// is sent with `answer`, and records them.
struct FakePlugin {
    seen: std::sync::mpsc::Receiver<Value>,
    _thread: std::thread::JoinHandle<()>,
}

impl FakePlugin {
    fn start(sock: &Path, answer: fn(&Value) -> Value) -> FakePlugin {
        let s = UnixStream::connect(sock).unwrap();
        let mut w = s.try_clone().unwrap();
        writeln!(w, "{}", json!({"id": 1, "method": "web.subscribe"})).unwrap();
        let mut lines = BufReader::new(s).lines();
        lines.next().unwrap().unwrap();
        let (tx, seen) = std::sync::mpsc::channel();
        let sock = sock.to_path_buf();
        let thread = std::thread::spawn(move || {
            let _w = w;
            for l in lines {
                let Ok(l) = l else { return };
                let v: Value = serde_json::from_str(&l).unwrap();
                if v["notify"] != "remote" {
                    continue;
                }
                let mut reply = answer(&v["req"]);
                reply["id"] = v["id"].clone();
                let got = raw(&sock, "web.remote_reply", reply);
                assert!(got["error"].is_null(), "{got}");
                if tx.send(v).is_err() {
                    return;
                }
            }
        });
        FakePlugin {
            seen,
            _thread: thread,
        }
    }

    /// The next command it was sent (`{notify, id, req}`).
    fn next(&self) -> Value {
        self.seen.recv_timeout(Duration::from_secs(5)).unwrap()
    }
}

/// How the stand-in answers: as the protocol says (docs/working/
/// e2e-protocol.md, "Host and daemon").
fn plugin_answer(req: &Value) -> Value {
    match req["cmd"].as_str().unwrap() {
        "setup" => json!({"ok": {"origin": "https://relay.example.com",
                                 "fingerprint": "q83vEjRWeJq83vEjRWeJq83vEjRWeJq83vEjRWeJq80"}}),
        "pair" => json!({"ok": {"qr": "RLORD1:AQIDBAUGBwgJCgsMDQ4PEA", "expires": 1_900_000_000}}),
        "confirm" => json!({"ok": {"device": {"id": "ab12cd34ef560789", "name": "phone"}}}),
        "revoke" if req["device"] == "nosuch" => json!({"error": "no device nosuch"}),
        _ => json!({"ok": {}}),
    }
}

/// The user's commands go to the plugin on its stream and its answers
/// come back; setup and disable write `[remote]`; the page's `device.*`
/// ops take the same path.
#[test]
fn relay_commands_go_to_the_plugin_and_back() {
    let r = start();
    let _config = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    let plugin = FakePlugin::start(&r.sock, plugin_answer);
    // A TUI's stream never sees them (a setup token travels in one).
    let events = UnixStream::connect(&r.sock).unwrap();
    events
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut ew = events.try_clone().unwrap();
    writeln!(
        ew,
        "{}",
        json!({"id": 1, "method": "events.subscribe", "params": {"client": "tui"}})
    )
    .unwrap();
    let mut event_lines = BufReader::new(events).lines();
    event_lines.next().unwrap().unwrap();

    // Setup: forwarded as asked; the answer is written to config.toml.
    let got = ok(
        &r.sock,
        "relay.setup",
        json!({"address": " https://relay.example.com#k=q83v ", "token": "t0k"}),
    );
    assert_eq!(got["relay"], "https://relay.example.com", "{got}");
    let sent = plugin.next();
    assert!(sent["id"].is_string(), "{sent}");
    assert_eq!(
        sent["req"],
        json!({"cmd": "setup", "address": "https://relay.example.com#k=q83v", "token": "t0k",
               "reset": false})
    );
    let cfg = Config::load().unwrap();
    assert!(cfg.remote.enabled);
    assert_eq!(cfg.remote.relay, "https://relay.example.com");
    assert_eq!(
        cfg.remote.relay_key,
        "q83vEjRWeJq83vEjRWeJq83vEjRWeJq83vEjRWeJq80"
    );
    let st = ok(&r.sock, "relay.status", json!({}));
    assert_eq!(st["enabled"], true, "{st}");
    let e = method_error(&raw(&r.sock, "relay.setup", json!({})));
    assert!(e.contains("address"), "{e}");

    // Pair: the offer, with its modules for a page to draw.
    let got = ok(&r.sock, "relay.pair", json!({}));
    assert_eq!(plugin.next()["req"], json!({"cmd": "pair"}));
    assert_eq!(got["qr"], "RLORD1:AQIDBAUGBwgJCgsMDQ4PEA");
    assert_eq!(got["expires"], 1_900_000_000);
    let rows = got["qr_rows"].as_array().unwrap();
    assert!(rows.len() >= 21, "{got}");
    assert!(rows.iter().all(|r| r.as_str().unwrap().len() == rows.len()));
    // ... the same from the page's Devices view; never from a device.
    let page = op(&r.sock, "device.pair", json!({}))["result"].clone();
    assert_eq!(plugin.next()["req"], json!({"cmd": "pair"}));
    assert_eq!(page["qr_rows"], got["qr_rows"]);
    fixture("device_pair", &page);
    not_allowed(&op_remote(&r.sock, "ab12cd34", "device.pair", json!({})));

    // The rest, each as the protocol names it.
    for (method, params, req) in [
        (
            "relay.pair_cancel",
            json!({}),
            json!({"cmd": "pair_cancel"}),
        ),
        ("relay.confirm", json!({}), json!({"cmd": "confirm"})),
        ("relay.reject", json!({}), json!({"cmd": "reject"})),
        (
            "relay.revoke",
            json!({"device": "ab12cd34"}),
            json!({"cmd": "revoke", "device": "ab12cd34"}),
        ),
    ] {
        ok(&r.sock, method, params);
        assert_eq!(plugin.next()["req"], req, "{method}");
    }
    let got = ok(&r.sock, "relay.confirm", json!({}));
    plugin.next();
    assert_eq!(got["device"]["name"], "phone");
    for (o, args, req) in [
        (
            "device.pair_cancel",
            json!({}),
            json!({"cmd": "pair_cancel"}),
        ),
        ("device.confirm", json!({}), json!({"cmd": "confirm"})),
        ("device.reject", json!({}), json!({"cmd": "reject"})),
        (
            "device.revoke",
            json!({"device": "ab12cd34", "id": "other"}),
            json!({"cmd": "revoke", "device": "ab12cd34"}),
        ),
    ] {
        let got = op(&r.sock, o, args);
        assert!(got["error"].is_null(), "{o}: {got}");
        assert_eq!(plugin.next()["req"], req, "{o}");
    }
    // The plugin's error is the command's.
    let e = method_error(&raw(&r.sock, "relay.revoke", json!({"device": "nosuch"})));
    assert!(e.contains("no device nosuch"), "{e}");
    plugin.next();
    let e = method_error(&raw(&r.sock, "relay.revoke", json!({})));
    assert!(e.contains("device id"), "{e}");
    // A late or unknown answer is refused.
    let got = raw(&r.sock, "web.remote_reply", json!({"id": "r999", "ok": {}}));
    assert!(got["error"].is_object(), "{got}");

    // Disable: the plugin closes its connection; enabled = false.
    ok(&r.sock, "relay.disable", json!({}));
    assert_eq!(plugin.next()["req"], json!({"cmd": "disable"}));
    assert!(!Config::load().unwrap().remote.enabled);
    let e = method_error(&raw(&r.sock, "relay.pair", json!({})));
    assert!(e.contains("off"), "{e}");

    for l in event_lines.by_ref() {
        let Ok(l) = l else { break };
        assert!(!l.contains("remote"), "a TUI got a relay command: {l}");
    }
}

/// The plugin's mirror: kept, answered from, a change of `rev`, and a
/// notice on every surface when a device starts waiting for the user.
#[test]
fn the_mirror_and_the_confirmation_notice() {
    let r = start();
    let mut stream = WebStream::open(&r.sock);
    ok(&r.sock, "web.pages", json!({"pages": ["p1-aaaa"]}));
    // A TUI too: the terminal's surface.
    let tui = UnixStream::connect(&r.sock).unwrap();
    tui.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut tw = tui.try_clone().unwrap();
    writeln!(
        tw,
        "{}",
        json!({"id": 1, "method": "events.subscribe", "params": {"client": "tui"}})
    )
    .unwrap();
    let mut tui_lines = BufReader::new(tui).lines();
    tui_lines.next().unwrap().unwrap();
    let state = |pending: Value| {
        json!({
            "relay": "https://relay.example.com", "connected": true, "error": null,
            "devices": [{"id": "ab12cd34ef560789", "name": "phone", "paired_at": 1_800_000_000,
                         "last_seen": 1_800_000_100, "linked": true}],
            "pairing": {"expires": 1_800_000_300},
            "pending": pending,
        })
    };
    let rev = r.daemon.rev();
    ok(
        &r.sock,
        "web.remote_state",
        state(json!({"name": "tablet", "code": "123456", "expires": 1_800_000_120})),
    );
    assert_ne!(r.daemon.rev(), rev);
    let n = stream.next_ui();
    assert_eq!(n["page"], "p1-aaaa");
    assert_eq!(n["cmd"]["cmd"], "notice");
    let text = n["cmd"]["text"].as_str().unwrap();
    assert!(
        text.contains("tablet") && text.contains("123456") && text.contains("Devices"),
        "{text}"
    );
    // The terminal's own words, on its status line.
    let n = loop {
        let v: Value = serde_json::from_str(&tui_lines.next().unwrap().unwrap()).unwrap();
        if v["notify"] == "ui" {
            break v;
        }
    };
    assert_eq!(
        (&n["to"], &n["cmd"]["cmd"]),
        (&json!("terminal"), &json!("notice"))
    );
    let text = n["cmd"]["text"].as_str().unwrap();
    assert!(text.contains("tablet") && text.contains(":relay"), "{text}");
    // The same state again: no second notice; another device: its own.
    ok(
        &r.sock,
        "web.remote_state",
        state(json!({"name": "tablet", "code": "123456", "expires": 1_800_000_120})),
    );
    ok(
        &r.sock,
        "web.remote_state",
        state(json!({"name": "laptop", "code": "654321", "expires": 1_800_000_200})),
    );
    let n = stream.next_ui();
    assert!(n["cmd"]["text"].as_str().unwrap().contains("laptop"), "{n}");
    // Answered from the mirror.
    let st = ok(&r.sock, "relay.status", json!({}));
    assert_eq!(st["reported"], true, "{st}");
    assert_eq!(st["connected"], true);
    assert_eq!(st["devices"][0]["name"], "phone");
    assert_eq!(st["pending"]["code"], "654321");
    let devices = ok(&r.sock, "relay.devices", json!({}));
    assert_eq!(devices["devices"][0]["id"], "ab12cd34ef560789");
    let snap = op(&r.sock, "snapshot", json!({}))["result"].clone();
    assert_eq!(
        snap["relay"],
        json!({"enabled": false, "connected": true, "devices": 1,
               "pending": {"name": "laptop", "code": "654321", "expires": 1_800_000_200}})
    );
    let list = op(&r.sock, "device.list", json!({}))["result"].clone();
    assert_eq!(list["devices"], st["devices"]);
    fixture("device_list", &list);
    // Not an object: refused.
    let bad = raw(&r.sock, "web.remote_state", json!(["x"]));
    assert!(bad["error"].is_object(), "{bad}");

    // The plugin goes: so does what it said.
    drop(stream);
    let start = Instant::now();
    loop {
        ok(
            &r.sock,
            "session.describe",
            json!({"ref": "alpha", "description": "x"}),
        );
        if ok(&r.sock, "relay.status", json!({}))["reported"] == false {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "mirror kept");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A Claude Code transcript line.
fn claude_line(kind: &str, text: &str) -> String {
    let content = if kind == "user" {
        json!(text)
    } else {
        json!([{"type": "text", "text": text}])
    };
    json!({"type": kind, "timestamp": "2026-11-17T10:00:00.000Z",
           "message": {"role": kind, "content": content}})
    .to_string()
}

/// The overlord's conversation as text: from its transcript, at most 50
/// messages a call, each at most 4 KiB, paged back by `before`.
#[test]
fn overlord_messages_are_bounded() {
    let dir = env().join("messages");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("0b1d7a52-0000-4000-8000-000000000001.jsonl");
    let mut lines = Vec::new();
    for i in 0..60 {
        lines.push(claude_line("user", &format!("ask {i}")));
        // Long, in three-byte characters: past 4 KiB until clipped.
        let reply = if i % 10 == 0 {
            "€".repeat(3000)
        } else {
            format!("answer {i}")
        };
        lines.push(claude_line("assistant", &reply));
    }
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    let p = path.to_str().unwrap().to_string();
    let r = start_with(|db| db.upsert_transcript_source("overlord-1", &p).unwrap());

    let got = op(&r.sock, "overlord.messages", json!({}))["result"].clone();
    assert_eq!(got["overlord"]["id"], "overlord-1", "{got}");
    let msgs = got["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 50);
    assert_eq!(got["more"], true);
    assert_eq!(msgs.last().unwrap()["text"], "answer 59");
    assert_eq!(msgs.last().unwrap()["who"], "agent");
    assert!(msgs
        .iter()
        .all(|m| m["text"].as_str().unwrap().len() <= 4096));
    assert!(msgs
        .iter()
        .any(|m| m["text"].as_str().unwrap().starts_with('€')));
    // Paged back from the oldest shown.
    let first = msgs[0]["line"].as_u64().unwrap();
    let back = op(
        &r.sock,
        "overlord.messages",
        json!({"before": first, "limit": 5}),
    )["result"]
        .clone();
    // (Five short messages: the phone page's chat test reads them.)
    fixture("overlord_messages", &back);
    let back = back["messages"].as_array().unwrap().clone();
    assert_eq!(back.len(), 5);
    assert!(back.iter().all(|m| m["line"].as_u64().unwrap() < first));
    // A big limit is 50; from a device too.
    let big = op_remote(
        &r.sock,
        "ab12cd34",
        "overlord.messages",
        json!({"limit": 500}),
    )["result"]
        .clone();
    assert_eq!(big["messages"].as_array().unwrap().len(), 50);
    // The start: nothing earlier.
    let all = op(&r.sock, "overlord.messages", json!({"before": 3}))["result"].clone();
    assert_eq!(all["messages"].as_array().unwrap().len(), 2);
    assert_eq!(all["more"], false);
    // A look-up: no change.
    let rev = r.daemon.rev();
    op(&r.sock, "overlord.messages", json!({}));
    assert_eq!(r.daemon.rev(), rev);
    // No transcript: no messages.
    let none = start();
    let got = op(&none.sock, "overlord.messages", json!({}))["result"].clone();
    assert_eq!(got["messages"], json!([]), "{got}");
}

// ---- slice 4: the phone ----

/// The phone in these tests, and the plugin's report that it is paired
/// (`linked`: it has a live session now).
const PHONE: &str = "ab12cd34";

fn paired(sock: &Path, devices: Value) {
    ok(sock, "web.remote_state", json!({"devices": devices}));
}

/// Any session's conversation as text, as the overlord's is read: at most
/// 50 messages of 4 KiB, paged back by `before`, from a page or a device.
#[test]
fn session_messages_are_bounded() {
    let dir = env().join("session-messages");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("0b1d7a52-0000-4000-8000-000000000002.jsonl");
    let mut lines = Vec::new();
    for i in 0..60 {
        lines.push(claude_line("user", &format!("ask {i}")));
        let reply = if i % 10 == 0 {
            "€".repeat(3000)
        } else {
            format!("answer {i}")
        };
        lines.push(claude_line("assistant", &reply));
    }
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    let p = path.to_str().unwrap().to_string();
    let r = start_with(|db| db.upsert_transcript_source("beta", &p).unwrap());

    let got =
        op_remote(&r.sock, PHONE, "session.messages", json!({"ref": "beta"}))["result"].clone();
    assert_eq!(got["session"]["id"], "beta", "{got}");
    assert_eq!(got["session"]["name"], "beta", "{got}");
    let msgs = got["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 50);
    assert_eq!(got["more"], true);
    assert_eq!(msgs.last().unwrap()["text"], "answer 59");
    assert!(msgs
        .iter()
        .all(|m| m["text"].as_str().unwrap().len() <= 4096));
    let first = msgs[0]["line"].as_u64().unwrap();
    let back = op(
        &r.sock,
        "session.messages",
        json!({"ref": "beta", "before": first, "limit": 500}),
    )["result"]
        .clone();
    let back = back["messages"].as_array().unwrap().clone();
    assert_eq!(back.len(), 50);
    assert!(back.iter().all(|m| m["line"].as_u64().unwrap() < first));
    // A session with no transcript: none. An unknown one: an error.
    let none = op(&r.sock, "session.messages", json!({"ref": "gamma"}))["result"].clone();
    assert_eq!(
        (&none["messages"], &none["more"]),
        (&json!([]), &json!(false))
    );
    assert!(op(&r.sock, "session.messages", json!({"ref": "nope"}))["error"].is_object());
    // A look-up: no change, and no use of a screen.
    let rev = r.daemon.rev();
    op_remote(&r.sock, PHONE, "session.messages", json!({"ref": "beta"}));
    assert_eq!(r.daemon.rev(), rev);
}

/// A paired device reports what it shows and is picked as a page is: the
/// surface the user did something on last. UI commands for it carry its
/// id and a number, and are kept for its next `page.pending` until the
/// plugin says one went out live.
#[test]
fn a_paired_phone_is_a_surface() {
    let r = start();
    let mut stream = WebStream::open(&r.sock);
    // Not listed as paired by the plugin: no surface.
    op_remote(
        &r.sock,
        PHONE,
        "session.pin",
        json!({"ref": "gamma", "pinned": true}),
    );
    assert_eq!(ui_state(&r.sock)["target"], "none");
    paired(
        &r.sock,
        json!([{"id": PHONE, "name": "pocket", "linked": true}]),
    );
    let s = ui_state(&r.sock);
    assert_eq!(s["target"], "phone", "{s}");
    assert_eq!(s["phone"]["open"], true);
    assert_eq!(s["phone"]["name"], "pocket");
    assert_eq!(s["phone"]["showing"]["kind"], "session_list");
    assert!(s["web"].is_null());

    // Its reports: as a page's, under its id (the plugin's word).
    let rep = op_remote(
        &r.sock,
        PHONE,
        "page.report",
        json!({"route": {"kind": "session", "id": "alpha"},
               "view": {"sort": "name"}, "visible": true, "form": "describe",
               "page": "p9-zzzz"}),
    );
    assert!(rep["error"].is_null(), "{rep}");
    let s = ui_state(&r.sock);
    assert_eq!(
        s["phone"]["showing"],
        json!({"kind": "session", "id": "alpha", "name": "alpha"})
    );
    assert_eq!(s["phone"]["form"], "describe");
    assert_eq!(s["current_view"]["sort"], "name");
    let bad = op_remote(
        &r.sock,
        PHONE,
        "page.report",
        json!({"route": {"kind": "admin"}}),
    );
    assert!(bad["error"].is_object(), "{bad}");

    // The overlord's tools land on it: `on: "phone"`, lines for it.
    let on_phone = |got: &Value, name: &str| {
        assert!(got["error"].is_null(), "{name}: {got}");
        assert_eq!(got["result"]["on"], "phone", "{name}: {got}");
        got["result"].clone()
    };
    on_phone(&tool(&r.sock, "show_list", json!({})), "show_list");
    let c = stream.next_ui();
    assert_eq!(c["device"], PHONE, "{c}");
    assert_eq!(c["cmd"], json!({"cmd": "show_list"}));
    assert!(c["n"].is_u64() && c.get("page").is_none(), "{c}");
    let t = on_phone(
        &tool(&r.sock, "set_list_view", json!({"query": "tag:web"})),
        "set_list_view",
    );
    // From the phone's own view.
    assert_eq!(t["view"]["sort"], "name");
    assert_eq!(stream.next_ui()["cmd"]["cmd"], "set_view");
    assert_eq!(stream.next_ui()["cmd"]["cmd"], "show_list");
    assert_eq!(ui_state(&r.sock)["current_view"]["query"], "tag:web");
    on_phone(
        &tool(
            &r.sock,
            "propose_session",
            json!({"name": "x", "prompt": "p"}),
        ),
        "propose_session",
    );
    assert_eq!(stream.next_ui()["cmd"]["cmd"], "new_session");
    on_phone(
        &tool(
            &r.sock,
            "confirm_new_session",
            json!({"description": "from the phone", "changed": [], "note": "n"}),
        ),
        "confirm_new_session",
    );
    let c = stream.next_ui();
    assert_eq!(c["cmd"]["cmd"], "confirm_new_session");
    let t = tool(&r.sock, "get_ui_state", json!({}))["result"].clone();
    assert_eq!(t["target"], "phone");
    assert_eq!(t["phone"]["open"], true);

    // Parked: the last command, handed to the phone's next `page.pending`
    // once (its live session never had it).
    let got = op_remote(&r.sock, PHONE, "page.pending", json!({}));
    assert_eq!(got["result"]["cmd"], "confirm_new_session", "{got}");
    assert_eq!(
        op_remote(&r.sock, PHONE, "page.pending", json!({}))["result"],
        Value::Null
    );
    // One that went out live isn't handed over again; a newer one than
    // the one delivered still is.
    tool(&r.sock, "show_list", json!({}));
    let n1 = stream.next_ui()["n"].as_u64().unwrap();
    ok(
        &r.sock,
        "web.ui_delivered",
        json!({"device": PHONE, "n": n1}),
    );
    assert_eq!(
        op_remote(&r.sock, PHONE, "page.pending", json!({}))["result"],
        Value::Null
    );
    tool(&r.sock, "show_list", json!({}));
    let n2 = stream.next_ui()["n"].as_u64().unwrap();
    tool(&r.sock, "show_list", json!({}));
    let n3 = stream.next_ui()["n"].as_u64().unwrap();
    assert!(n1 < n2 && n2 < n3);
    ok(
        &r.sock,
        "web.ui_delivered",
        json!({"device": PHONE, "n": n2}),
    );
    assert_eq!(
        op_remote(&r.sock, PHONE, "page.pending", json!({}))["result"],
        json!({"cmd": "show_list"})
    );
    // A local page's slot is its own.
    assert_eq!(
        op(&r.sock, "page.pending", json!({}))["result"],
        Value::Null
    );

    // A page used later wins; then the user does something on the phone,
    // and it is the target again (a look-up from it doesn't count).
    ok(&r.sock, "web.pages", json!({"pages": ["p1-aaaa"]}));
    std::thread::sleep(Duration::from_millis(1100));
    op_from(
        &r.sock,
        "p1-aaaa",
        "session.pin",
        json!({"ref": "gamma", "pinned": false}),
    );
    assert_eq!(ui_state(&r.sock)["target"], "web");
    assert_eq!(tool(&r.sock, "show_list", json!({}))["result"]["on"], "web");
    std::thread::sleep(Duration::from_millis(1100));
    op_remote(&r.sock, PHONE, "snapshot", json!({}));
    op_remote(&r.sock, PHONE, "session.messages", json!({"ref": "alpha"}));
    assert_eq!(ui_state(&r.sock)["target"], "web");
    op_remote(
        &r.sock,
        PHONE,
        "session.describe",
        json!({"ref": "alpha", "description": "from the phone"}),
    );
    let s = ui_state(&r.sock);
    assert_eq!(s["target"], "phone", "{s}");
    // The page is still described (the most recent page).
    assert_eq!(s["web"]["open"], true);

    // Unpaired (revoked, or it left): no surface any more.
    paired(&r.sock, json!([]));
    let s = ui_state(&r.sock);
    assert_eq!((&s["target"], &s["phone"]), (&json!("web"), &Value::Null));
}

/// What needs a screen on this computer (the code-folder form, the key
/// screen) never goes to a phone: it takes a terminal or a page, and with
/// only a phone it fails naming the ways in.
#[test]
fn screens_on_this_computer_never_go_to_a_phone() {
    let r = start();
    let mut stream = WebStream::open(&r.sock);
    paired(&r.sock, json!([{"id": PHONE, "name": "p", "linked": true}]));
    op_remote(&r.sock, PHONE, "page.report", json!({"input": true}));
    assert_eq!(ui_state(&r.sock)["target"], "phone");
    for (name, args) in [
        ("confirm_setup", json!({"code_dir": "/tmp"})),
        ("open_keys", json!({})),
    ] {
        let e = method_error(&tool(&r.sock, name, args));
        assert!(
            e.contains("phone") && e.contains("rlord web") && e.contains("terminal"),
            "{name}: {e}"
        );
    }
    // With a page open (used before the phone): the page.
    ok(&r.sock, "web.pages", json!({"pages": ["p1-aaaa"]}));
    std::thread::sleep(Duration::from_millis(1100));
    op_remote(&r.sock, PHONE, "page.report", json!({"input": true}));
    assert_eq!(ui_state(&r.sock)["target"], "phone");
    let t = tool(&r.sock, "confirm_setup", json!({"code_dir": "/tmp"}));
    assert_eq!(t["result"]["on"], "web", "{t}");
    let c = stream.next_ui();
    assert_eq!(
        (&c["page"], &c["cmd"]["cmd"]),
        (&json!("p1-aaaa"), &json!("setup"))
    );
    let t = tool(&r.sock, "open_keys", json!({}));
    assert_eq!(t["result"]["on"], "web", "{t}");
    assert_eq!(stream.next_ui()["cmd"]["cmd"], "keys");
    // Nothing was parked for the phone.
    assert_eq!(
        op_remote(&r.sock, PHONE, "page.pending", json!({}))["result"],
        Value::Null
    );
}

/// `device.leave` is a device's own: from a device it names only the
/// caller (the plugin then removes that one); from a local page it is a
/// bad request.
#[test]
fn device_leave_is_the_callers_own() {
    let r = start();
    let got = op_remote(
        &r.sock,
        PHONE,
        "device.leave",
        // Not the device's to choose: dropped.
        json!({"device": "ffffffff"}),
    );
    assert_eq!(got["result"], json!({"left": PHONE}), "{got}");
    let got = op(&r.sock, "device.leave", json!({}));
    assert_eq!(got["error"]["code"], plugin::RPC_ERR_BAD_REQUEST, "{got}");
    assert!(got["error"]["message"]
        .as_str()
        .unwrap()
        .contains("device.revoke"));
}

/// The remote API, level by level: within a level, the ops a device may
/// call and the keys of its snapshot only grow (`web::API`; the phone app
/// is built against one). The fixture is written with
/// `RLORD_UPDATE_FIXTURES=1`, which refuses to drop anything: dropping or
/// renaming is a new level.
#[test]
fn the_remote_api_only_grows_within_a_level() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/remote_api.json");
    let r = start();
    let snap = op_remote(&r.sock, PHONE, "snapshot", json!({}))["result"].clone();
    assert!(snap.get("relay").is_none(), "{snap}");
    let mut keys = std::collections::BTreeSet::new();
    key_paths(&snap, "", &mut keys);
    let ops: std::collections::BTreeSet<String> = rlord_daemon::web::OPS
        .iter()
        .filter(|o| o.remote)
        .map(|o| o.op.to_string())
        .collect();
    let now = json!({"api": rlord_daemon::web::API, "ops": ops, "snapshot": keys});
    let old: Option<Value> = std::fs::read_to_string(&path)
        .ok()
        .map(|t| serde_json::from_str(&t).unwrap());
    let missing = |old: &Value, k: &str, now: &std::collections::BTreeSet<String>| -> Vec<String> {
        old[k]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|x| !now.contains(*x))
            .map(str::to_string)
            .collect()
    };
    if let Some(old) = &old {
        assert_eq!(
            old["api"],
            rlord_daemon::web::API,
            "web::API moved: rewrite {} for the new level",
            path.display()
        );
        let gone: Vec<String> = missing(old, "ops", &ops)
            .into_iter()
            .chain(missing(old, "snapshot", &keys))
            .collect();
        assert!(
            gone.is_empty(),
            "the remote API lost {gone:?} within level {}: put them back, or bump web::API",
            rlord_daemon::web::API
        );
    }
    if std::env::var("RLORD_UPDATE_FIXTURES").as_deref() == Ok("1") || old.is_none() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&now).unwrap() + "\n").unwrap();
    }
    assert!(old.is_some(), "wrote {}: commit it", path.display());
}

/// Every key path in `v` (`rows[].id`, `overlord.overlords[].agent`, …),
/// arrays by their first item.
fn key_paths(v: &Value, at: &str, out: &mut std::collections::BTreeSet<String>) {
    match v {
        Value::Object(o) => {
            for (k, v) in o {
                let p = if at.is_empty() {
                    k.clone()
                } else {
                    format!("{at}.{k}")
                };
                out.insert(p.clone());
                key_paths(v, &p, out);
            }
        }
        Value::Array(a) => {
            if let Some(first) = a.first() {
                key_paths(first, &format!("{at}[]"), out);
            }
        }
        _ => {}
    }
}
