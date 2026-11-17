//! `rlord-web`, the opt-in web UI plugin. The daemon starts it
//! (`rlord-web serve`) and supervises it; nobody runs it by hand.
//!
//! Exit codes: 0 when the daemon goes away, 1 on an error, 2 on a wrong
//! command line, 3 when the daemon refuses its protocol
//! (`plugin::EXIT_INCOMPATIBLE`), 4 when its port is taken
//! (`plugin::EXIT_PORT_IN_USE`). The daemon retries none of 3 and 4.

use std::io::{BufRead, BufReader, Write};
use std::process::ExitCode;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use rlord_core::{paths, plugin};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "usage: rlord-web serve | --version
  (started by the rlord daemon: run `rlord web`)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["--version" | "-V"] => {
            println!("{} {VERSION}", plugin::BIN_NAME);
            ExitCode::SUCCESS
        }
        ["serve"] => match serve() {
            Ok(code) => code,
            Err(e) => {
                eprintln!("rlord-web: {e:#}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Say hello to the daemon, then stay until it goes away.
fn serve() -> Result<ExitCode> {
    let socket = paths::socket_path();
    let stream = std::os::unix::net::UnixStream::connect(&socket)
        .with_context(|| format!("connecting to the daemon at {}", socket.display()))?;
    let mut w = stream.try_clone()?;
    let hello = json!({"id": 1, "method": "web.hello", "params": {
        "protocol": plugin::WEB_PROTOCOL,
        "version": VERSION,
        "pid": std::process::id(),
        "port": 0,
    }});
    writeln!(w, "{hello}")?;
    let mut lines = BufReader::new(stream).lines();
    let reply: Value = serde_json::from_str(&lines.next().context("the daemon hung up")??)?;
    if let Some(e) = reply.get("error") {
        eprintln!("rlord-web: the daemon refused hello: {}", e["message"]);
        if e["code"] == plugin::RPC_ERR_INCOMPATIBLE {
            return Ok(ExitCode::from(plugin::EXIT_INCOMPATIBLE as u8));
        }
        return Ok(ExitCode::FAILURE);
    }
    // Nothing else to do yet: wait for the daemon to go.
    for line in lines {
        if line.is_err() {
            break;
        }
    }
    Ok(ExitCode::SUCCESS)
}
