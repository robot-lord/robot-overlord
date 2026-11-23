//! `rlord-device bridge` as the page sees it (no relay or host needed):
//! the shell's own messages are answered by the bridge, the rest reach
//! the engine, the engine's messages come back, one page at a time, and
//! a page from an origin not given is refused.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use rlord_e2e_device::bridge::{Bridge, Options};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("rlord-bridge-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

async fn start(name: &str) -> Bridge {
    Bridge::start(Options {
        dir: scratch(name),
        name: "test".into(),
        origins: vec!["http://127.0.0.1:1".into()],
        ..Options::default()
    })
    .await
    .unwrap()
}

/// The next message that `pick` accepts.
async fn next(ws: &mut Ws, pick: impl Fn(&Value) -> bool) -> Value {
    let wait = async {
        while let Some(Ok(m)) = ws.next().await {
            if let Message::Text(t) = m {
                let v: Value = serde_json::from_str(&t).unwrap();
                if pick(&v) {
                    return v;
                }
            }
        }
        panic!("the bridge closed");
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .expect("in time")
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::text(v.to_string())).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn plays_the_shell_and_carries_the_engine() {
    let mut b = start("shell").await;
    assert!(b.url().starts_with("ws://127.0.0.1:"));
    let (mut ws, _) = tokio_tungstenite::connect_async(b.url()).await.unwrap();
    // The shell's settings, from the bridge itself.
    send(&mut ws, json!({"app": "settings"})).await;
    let s = next(&mut ws, |v| v["event"] == "settings").await;
    assert_eq!(s["platform"], "bridge");
    assert_eq!(s["app_lock"], false);
    send(&mut ws, json!({"app": "settings", "app_lock": true})).await;
    let s = next(&mut ws, |v| v["event"] == "settings").await;
    assert_eq!(s["app_lock"], true);
    // The engine's: a fresh one is unpaired.
    send(&mut ws, json!({"app": "ready"})).await;
    let c = next(&mut ws, |v| v["event"] == "conn").await;
    assert_eq!(c["state"], "unpaired", "{c}");
    send(&mut ws, json!({"app": "status"})).await;
    let st = next(&mut ws, |v| v["event"] == "status").await;
    assert!(st["version"].is_string(), "{st}");
    // A request with no host is answered at once.
    send(&mut ws, json!({"id": 7, "op": "snapshot", "args": {}})).await;
    let r = next(&mut ws, |v| v["id"] == 7).await;
    assert!(r["error"].is_object(), "{r}");
    // Scan: the bridge asks whoever runs it.
    send(&mut ws, json!({"app": "scan"})).await;
    let ev = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), b.event())
            .await
            .unwrap()
            .unwrap();
        if ev["event"] == "scan" {
            break ev;
        }
    };
    assert_eq!(ev, json!({"event": "scan"}));
    assert!(b.connected());

    // A second page replaces the first, which is closed.
    let (mut ws2, _) = tokio_tungstenite::connect_async(b.url()).await.unwrap();
    send(&mut ws2, json!({"app": "ready"})).await;
    next(&mut ws2, |v| v["event"] == "conn").await;
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                _ => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the first page is closed");
    drop(ws2);
    let t = std::time::Instant::now();
    while b.connected() {
        assert!(t.elapsed() < Duration::from_secs(5), "the page left");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    b.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_from_another_origin_is_refused() {
    let b = start("origin").await;
    let mut req = b.url().into_client_request().unwrap();
    req.headers_mut()
        .insert("origin", "https://example.com".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
    let mut req = b.url().into_client_request().unwrap();
    req.headers_mut()
        .insert("origin", "http://127.0.0.1:1".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_ok());
    b.shutdown();
}
