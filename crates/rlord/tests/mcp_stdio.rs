//! `rlord mcp` over stdio, against a fake daemon socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

/// A slow tool must not hold up the call sent after it: each `tools/call`
/// answers on its own, so the fast one's reply comes first.
#[test]
fn slow_tool_call_does_not_block_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { break };
            std::thread::spawn(move || {
                let mut line = String::new();
                BufReader::new(&conn).read_line(&mut line).unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                let name = req["params"]["name"].as_str().unwrap_or("").to_string();
                if name == "slow" {
                    std::thread::sleep(Duration::from_secs(2));
                }
                let resp = json!({"id": req["id"], "result": {"tool": name}});
                let _ = writeln!(conn, "{resp}");
            });
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_rlord"))
        .args(["mcp", "--socket"])
        .arg(&sock)
        .env_remove("RLORD_SESSION_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for (id, name) in [(1, "slow"), (2, "fast")] {
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                         "params": {"name": name, "arguments": {}}});
        writeln!(stdin, "{msg}").unwrap();
    }
    // stdin closes first: the slow call must still answer (a client that
    // half-closes reads on).
    drop(stdin);
    // Read on a thread, so a lost reply fails the test instead of hanging it.
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines() {
            let id = serde_json::from_str::<Value>(&l.unwrap()).unwrap()["id"].as_i64();
            let _ = tx.send(id.unwrap());
        }
    });
    let mut ids = Vec::new();
    for _ in 0..2 {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(id) => ids.push(id),
            Err(_) => {
                let _ = child.kill();
                panic!("no reply within 20s; got {ids:?}");
            }
        }
    }
    child.wait().unwrap();
    assert_eq!(ids, vec![2, 1]);
}
