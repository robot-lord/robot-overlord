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
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sock = std::env::temp_dir().join(format!("rlord-mcp-{}-{nanos}.sock", std::process::id()));
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
    // Closing stdin must still let the slow call answer before exit.
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
    let _ = std::fs::remove_file(&sock);
    assert_eq!(ids, vec![2, 1]);
}
