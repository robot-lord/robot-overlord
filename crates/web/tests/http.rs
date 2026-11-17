//! The plugin end to end, without tmux (`common::Rig`). Checks the Host
//! and Origin checks, first-frame auth, the control socket's requests and
//! events, and that the plugin goes with the daemon.

mod common;

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

use rlord_core::paths;

use common::{alive, close_code, next_json, Rig, LOCK};

fn start() -> Rig {
    common::env_once("web-http", "rlord-test-web-http");
    Rig::start(|db| common::add(db, "alpha", None))
}

#[tokio::test(flavor = "multi_thread")]
async fn host_check_and_headers() {
    let _g = LOCK.lock().await;
    let r = tokio::task::spawn_blocking(start).await.unwrap();
    let host = format!("127.0.0.1:{}", r.port);
    let (status, text) = r.get("/", &host).await;
    assert_eq!(status, 200, "{text}");
    let lower = text.to_lowercase();
    assert!(lower.contains("content-type: text/html"), "{text}");
    for h in [
        "content-security-policy: default-src 'none'; script-src 'self';",
        "x-content-type-options: nosniff",
        "referrer-policy: no-referrer",
        "cross-origin-opener-policy: same-origin",
        "cross-origin-resource-policy: same-origin",
        "cache-control: no-store",
    ] {
        assert!(lower.contains(h), "missing {h}: {text}");
    }
    assert!(lower.contains(&format!("connect-src 'self' ws://127.0.0.1:{}", r.port)));
    assert!(!lower.contains("set-cookie"));
    // DNS rebinding: another name for this address is refused, headers
    // and all.
    for bad in [
        format!("localhost:{}", r.port),
        "evil.example".to_string(),
        format!("127.0.0.1:{}", r.port + 1),
    ] {
        let (status, text) = r.get("/", &bad).await;
        assert_eq!(status, 404, "{bad}: {text}");
        assert!(text.to_lowercase().contains("content-security-policy"));
    }
    assert_eq!(r.get("/no-such-file.js", &host).await.0, 404);
    drop(r);
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrades_need_the_exact_origin_and_the_token() {
    let _g = LOCK.lock().await;
    let r = tokio::task::spawn_blocking(start).await.unwrap();
    assert_eq!(r.ws("/ws", None).await.err(), Some(403));
    for bad in [
        "http://localhost:".to_string() + &r.port.to_string(),
        "http://evil.example".to_string(),
        "null".to_string(),
    ] {
        assert_eq!(r.ws("/ws", Some(&bad)).await.err(), Some(403), "{bad}");
    }
    // A wrong token, or something else first: closed with 4401.
    for first in [
        json!({"auth": "0".repeat(64)}).to_string(),
        json!({"auth": ""}).to_string(),
        json!({"id": 1, "op": "snapshot"}).to_string(),
        "hello".to_string(),
    ] {
        let mut ws = r.ws("/ws", Some(&r.origin())).await.unwrap();
        ws.send(Message::text(first.clone())).await.unwrap();
        assert_eq!(close_code(&mut ws).await, Some(4401), "{first}");
    }
    // The right one: hello.
    let (_ws, hello) = r.control().await;
    assert_eq!(hello["event"], "hello", "{hello}");
    assert_eq!(hello["version"], env!("CARGO_PKG_VERSION"));
    assert!(hello["build"]
        .as_str()
        .unwrap()
        .starts_with(env!("CARGO_PKG_VERSION")));
    assert_eq!(hello["rev"], r.daemon.rev().as_str());
    drop(r);
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_and_events_on_the_control_socket() {
    let _g = LOCK.lock().await;
    let r = tokio::task::spawn_blocking(start).await.unwrap();
    let (mut ws, _) = r.control().await;
    // A snapshot.
    ws.send(Message::text(
        json!({"id": 1, "op": "snapshot", "args": {"view": {}}}).to_string(),
    ))
    .await
    .unwrap();
    let a = next_json(&mut ws).await;
    assert_eq!(a["id"], 1, "{a}");
    assert_eq!(a["ok"]["rows"][0]["id"], "alpha", "{a}");
    let rev = a["ok"]["rev"].as_str().unwrap().to_string();
    // A change in the daemon: an event with the new rev.
    let sock = paths::socket_path();
    tokio::task::spawn_blocking(move || {
        rlord_daemon::client_call(
            &sock,
            "session.describe",
            json!({"ref": "alpha", "description": "from a test"}),
        )
        .unwrap()
    })
    .await
    .unwrap();
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["event"], "changed", "{ev}");
    assert_ne!(ev["rev"], rev.as_str());
    assert_eq!(ev["rev"], r.daemon.rev().as_str());
    // Off the list; not JSON; no op.
    ws.send(Message::text(
        json!({"id": "x", "op": "admin.reset", "args": {}}).to_string(),
    ))
    .await
    .unwrap();
    let a = next_json(&mut ws).await;
    assert_eq!(a["id"], "x");
    assert_eq!(a["error"]["code"], "not_allowed", "{a}");
    ws.send(Message::text("{nope")).await.unwrap();
    assert_eq!(next_json(&mut ws).await["error"]["code"], "bad_request");
    ws.send(Message::text(json!({"id": 3}).to_string()))
        .await
        .unwrap();
    let a = next_json(&mut ws).await;
    assert_eq!(
        (a["id"].clone(), a["error"]["code"].clone()),
        (json!(3), json!("bad_request"))
    );
    // A method error comes back as one.
    ws.send(Message::text(
        json!({"id": 4, "op": "session.get", "args": {"ref": "nosuch"}}).to_string(),
    ))
    .await
    .unwrap();
    let a = next_json(&mut ws).await;
    assert_eq!(a["error"]["code"], "failed", "{a}");
    // A frame over the 1 MiB limit ends the socket, unanswered.
    let big = format!(
        "{{\"id\":5,\"op\":\"snapshot\",\"args\":{{\"x\":\"{}\"}}}}",
        "a".repeat(1 << 20)
    );
    let _ = ws.send(Message::text(big)).await;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => panic!("answered a frame over the limit: {t}"),
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => break,
            Ok(Some(Ok(_))) => continue,
            Err(_) => panic!("the socket stayed open"),
        }
    }
    drop(r);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plugin_goes_with_the_daemon() {
    let _g = LOCK.lock().await;
    let mut r = tokio::task::spawn_blocking(start).await.unwrap();
    assert!(alive(r.pid));
    drop(r.stop.take());
    let t = Instant::now();
    while alive(r.pid) {
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "rlord-web outlived its daemon"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let log = r.log();
    assert!(log.contains("the daemon went away"), "{log}");
    drop(r);
}
