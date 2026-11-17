//! A pane through the plugin's bridge, end to end, on a scratch tmux
//! server (socket `rlord-test-web`). Gated: `RLORD_TMUX_TESTS=1 cargo
//! test`. Never touches `tmux -L rlord` or the default server; the "agent"
//! is `tee` into a log file.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use rlord_core::tmux;

use common::{next_json, Rig, Ws, LOCK};

const SOCKET: &str = "rlord-test-web";
const TARGET: &str = "rlord_webpane";

fn gated() -> bool {
    std::env::var("RLORD_TMUX_TESTS").as_deref() == Ok("1")
}

/// A tmux command on the scratch server; stdout, trimmed.
fn tx(args: &[&str]) -> String {
    let out = tmux::command().args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn views() -> Vec<String> {
    tx(&["list-sessions", "-F", "#{session_name}"])
        .lines()
        .filter(|s| tmux::is_view(s))
        .map(str::to_string)
        .collect()
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn read_log(log: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(log).unwrap_or_default()).into_owned()
}

/// Pane output read so far, and the control messages seen.
#[derive(Default)]
struct Seen {
    out: Vec<u8>,
    control: Vec<Value>,
    closed: bool,
}

impl Seen {
    fn screen(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }
}

/// Read from the pane socket until `done` (5 s at most).
async fn read_until(ws: &mut Ws, seen: &mut Seen, what: &str, done: impl Fn(&Seen) -> bool) {
    let start = Instant::now();
    while !done(seen) {
        let left = Duration::from_secs(5).saturating_sub(start.elapsed());
        assert!(!left.is_zero(), "timed out waiting for {what}");
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Binary(b)))) => seen.out.extend_from_slice(&b),
            Ok(Some(Ok(Message::Text(t)))) => seen.control.push(serde_json::from_str(&t).unwrap()),
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => {
                seen.closed = true;
                assert!(done(seen), "closed before {what}");
                return;
            }
            Ok(Some(Ok(_))) => {}
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

async fn open(r: &Rig, target: &str, cols: u16, rows: u16) -> Ws {
    let mut ws = r.authed("/pane").await;
    ws.send(Message::text(
        json!({"type": "open", "target": target, "cols": cols, "rows": rows}).to_string(),
    ))
    .await
    .unwrap();
    ws
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pane_through_the_bridge() {
    if !gated() {
        return;
    }
    let _g = LOCK.lock().await;
    let dir = common::env_once("web-pane", SOCKET);
    tx(&["kill-server"]);
    let log = dir.join("probe.log");
    let _ = std::fs::remove_file(&log);
    // The "agent": raw mode, logs and shows what it is given.
    let probe = format!(
        "stty raw -echo; printf 'PROBE-READY'; exec tee {}",
        tmux::shell_quote(&log.to_string_lossy())
    );
    tmux::new_session(TARGET, "/tmp", &probe).unwrap();
    // The target has a status row of its own (as the overlord does).
    tx(&["set-option", "-t", &format!("={TARGET}:"), "status", "on"]);
    // A root binding the browser must never reach.
    tx(&[
        "bind-key",
        "-n",
        "F12",
        "set-option",
        "-g",
        "@web_fired",
        "yes",
    ]);
    // A view a dead bridge left behind: swept when the plugin starts.
    tx(&[
        "new-session",
        "-d",
        "-t",
        &format!("={TARGET}"),
        "-s",
        "webview-leftover",
    ]);
    assert!(tmux::has_session("webview-leftover"));

    let r = tokio::task::spawn_blocking(|| {
        Rig::start(|db| {
            common::add(db, "alpha", Some(TARGET));
            common::add(db, "beta", None);
        })
    })
    .await
    .unwrap();
    assert!(!tmux::has_session("webview-leftover"), "not swept");
    assert!(
        r.log().contains("removed 1 leftover view session"),
        "{}",
        r.log()
    );

    // Not running: closed, nothing started.
    let mut ws = open(&r, "beta", 80, 24).await;
    let m = next_json(&mut ws).await;
    assert_eq!(
        (m["type"].as_str(), m["reason"].as_str()),
        (Some("closed"), Some("not_running")),
        "{m}"
    );
    let mut ws = open(&r, "nosuch", 80, 24).await;
    assert_eq!(next_json(&mut ws).await["reason"], "not_found");
    assert!(views().is_empty());

    // Open: the pane's screen, in a view session of its own.
    let mut ws = open(&r, "alpha", 80, 24).await;
    let opened = next_json(&mut ws).await;
    assert_eq!(opened["type"], "opened", "{opened}");
    assert_eq!(
        (opened["id"].as_str(), opened["name"].as_str()),
        (Some("alpha"), Some("alpha"))
    );
    let mut seen = Seen::default();
    read_until(&mut ws, &mut seen, "the target's screen", |s| {
        s.screen().contains("PROBE-READY")
    })
    .await;
    let v = views();
    assert_eq!(v.len(), 1, "{v:?}");
    let view = format!("={}:", v[0]);
    assert!(tmux::view_shows(&v[0], TARGET));

    // No status row: the view's is off, the window takes all 24 rows; the
    // target keeps its own.
    assert_eq!(tx(&["show-options", "-v", "-t", &view, "status"]), "off");
    assert_eq!(
        tx(&["show-options", "-v", "-t", &format!("={TARGET}:"), "status"]),
        "on"
    );
    let window = tmux::current_window_id(TARGET).unwrap();
    let dim = |f: &str| tx(&["display-message", "-p", "-t", &window, f]);
    wait_for("24 rows", || dim("#{window_height}") == "24").await;

    // Typing reaches the pane, and its output comes back.
    ws.send(Message::binary(b"hello-web".to_vec()))
        .await
        .unwrap();
    wait_for("the keys in the pane", || {
        read_log(&log).contains("hello-web")
    })
    .await;
    read_until(&mut ws, &mut seen, "the echo", |s| {
        s.screen().contains("hello-web")
    })
    .await;

    // A root binding doesn't fire: the key reaches the pane instead.
    ws.send(Message::binary(b"\x1b[24~".to_vec()))
        .await
        .unwrap();
    wait_for("F12 in the pane", || read_log(&log).contains("\x1b[24~")).await;
    assert_eq!(
        tx(&["show-options", "-gqv", "@web_fired"]),
        "",
        "root binding fired"
    );

    // The browser is never the user's terminal.
    assert_eq!(
        tx(&["list-clients", "-F", "#{session_name}"])
            .lines()
            .count(),
        1
    );
    assert!(tmux::list_clients().is_empty());

    // Resize: the window follows.
    ws.send(Message::text(
        json!({"type": "resize", "cols": 100, "rows": 30}).to_string(),
    ))
    .await
    .unwrap();
    wait_for("100 columns", || dim("#{window_width}") == "100").await;
    wait_for("30 rows", || dim("#{window_height}") == "30").await;

    // A second view at once: its own view session.
    let mut ws2 = open(&r, "alpha", 80, 24).await;
    assert_eq!(next_json(&mut ws2).await["type"], "opened");
    wait_for("two views", || views().len() == 2).await;
    // Closing it: its view goes, the other stays.
    ws2.send(Message::text(json!({"type": "close"}).to_string()))
        .await
        .unwrap();
    let mut seen2 = Seen::default();
    read_until(&mut ws2, &mut seen2, "the second's close", |s| {
        s.closed || s.control.iter().any(|m| m["type"] == "closed")
    })
    .await;
    wait_for("one view", || views().len() == 1).await;

    // Killing the target closes the socket and leaves no view session.
    tmux::kill_session(TARGET).unwrap();
    read_until(&mut ws, &mut seen, "the close", |s| {
        s.closed || s.control.iter().any(|m| m["type"] == "closed")
    })
    .await;
    assert!(
        seen.control
            .iter()
            .any(|m| m["type"] == "closed" && m["reason"] == "ended"),
        "{:?}",
        seen.control
    );
    wait_for("no view sessions", || views().is_empty()).await;

    drop(r);
    tx(&["kill-server"]);
}
