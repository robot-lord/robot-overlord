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

/// A daemon with an overlord and three sessions (two tagged `web`, one
/// also `wip`), serving on a temp socket.
fn start() -> Running {
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

fn op(sock: &Path, op: &str, args: Value) -> Value {
    raw(sock, "web.call", json!({"op": op, "args": args}))
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
        json!({"op": op, "args": args, "page": page}),
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
