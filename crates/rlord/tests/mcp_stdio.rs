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
    // A lost reply must fail the test, not hang it.
    let pid = child.id();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(20));
        let _ = Command::new("kill").arg(pid.to_string()).status();
    });
    let mut stdin = child.stdin.take().unwrap();
    for (id, name) in [(1, "slow"), (2, "fast")] {
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                         "params": {"name": name, "arguments": {}}});
        writeln!(stdin, "{msg}").unwrap();
    }
    // stdin closes first: the slow call must still answer (a client that
    // half-closes reads on).
    drop(stdin);
    let ids: Vec<i64> = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map(|l| {
            serde_json::from_str::<Value>(&l.unwrap()).unwrap()["id"]
                .as_i64()
                .unwrap()
        })
        .collect();
    child.wait().unwrap();
    assert_eq!(ids, vec![2, 1]);
}
