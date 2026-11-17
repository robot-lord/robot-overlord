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
        "session.stop",
        "session.new",
        "overlord.ask",
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
