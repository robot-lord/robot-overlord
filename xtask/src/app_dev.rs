//! `cargo xtask app dev` and `app test-e2e`: the phone page without a
//! phone. Both start the loopback host (`app::Stack`: an in-memory daemon
//! with fixture sessions and a fake overlord, the real `rlord-web`, the
//! real relay, all on 127.0.0.1) and `rlord-device bridge` (the app's
//! engine, native, its message bridge offered on a loopback websocket),
//! handed the host's pairing code as if scanned. The host confirms a
//! pairing by itself.
//!
//! - `dev` builds the dev page (`--mode app-dev` into `dist-app-dev/`: the
//!   phone page plus the dev bridge, its `connect-src` naming the bridge
//!   only), serves it with `vite preview` on 127.0.0.1, rebuilds it on
//!   change, and takes the host's commands on stdin. The page's Scan button
//!   gets a fresh pairing code from the host.
//! - `test-e2e` runs `vitest.app-e2e.config.ts` (the page's own store and
//!   `RemoteTransport` in Node over the bridge), with the bridge's URL and
//!   the host's control folder in `RLORD_BRIDGE_URL` and
//!   `RLORD_STACK_CONTROL`.
//!
//! No Xcode, no simulator, no real agent; nothing leaves loopback.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::app::Stack;
use crate::cmd::{self, run};
use crate::{web, web_app};

const UI: &str = "crates/web/ui";
const DEV_DIST: &str = "dist-app-dev";
const DEV_PAGE: &str = "app-dev.html";
const E2E_CONFIG: &str = "vitest.app-e2e.config.ts";

const DEV_HELP: &str = "\
commands: pair (a new pairing code, as if scanned), restart-relay, stop-plugin,
          start-plugin, status, network up|down (the phone's network), quit";

/// `app dev [--port N]`.
pub fn dev(root: &Path, args: &[String]) -> Result<()> {
    let port = dev_port(args)?;
    let ui = root.join(UI);
    web::ensure_installed(&ui)?;
    build_device()?;
    let mut stack = Stack::start(root, true)?;
    if !stack.overlord {
        eprintln!("note: no fake overlord (tmux isn't installed): asks will fail");
    }
    let qr = stack.pairing_code()?;
    let page_port = match port {
        Some(p) => p,
        None => free_port()?,
    };
    let page = page_url(page_port);
    let state = cmd::TempDir::new_in(&root.join("target"), "app-device")?;
    let mut bridge = DeviceBridge::start(root, state.path(), &qr, Some(&page_origin(page_port)))?;

    // The page, built once (and checked), then rebuilt on change.
    let port_env = bridge.port.to_string();
    run(web::tool(&ui, "vite")?
        .args(["build", "--mode", "app-dev"])
        .env("VITE_RLORD_BRIDGE_PORT", &port_env))?;
    web_app::check_dev(&ui.join(DEV_DIST), bridge.port)?;
    let _watch = Reaped(
        web::tool(&ui, "vite")?
            .args(["build", "--mode", "app-dev", "--watch"])
            .env("VITE_RLORD_BRIDGE_PORT", &port_env)
            .stdout(Stdio::null())
            .spawn()
            .context("starting vite build --watch")?,
    );
    let _serve = Reaped(
        web::tool(&ui, "vite")?
            .args(preview_args(page_port))
            .stdout(Stdio::null())
            .spawn()
            .context("starting vite preview")?,
    );

    println!(
        "\nthe phone page: {page}{DEV_PAGE}\n(host relay {}, bridge {})\n{DEV_HELP}",
        stack.relay, bridge.url
    );
    let (tx, rx) = mpsc::channel();
    let stdin_tx = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if stdin_tx.send(Input::Line(line)).is_err() {
                return;
            }
        }
        let _ = stdin_tx.send(Input::Line("quit".into()));
    });
    let events = std::mem::replace(&mut bridge.events, mpsc::channel().1);
    std::thread::spawn(move || {
        for ev in events {
            if tx.send(Input::Bridge(ev)).is_err() {
                return;
            }
        }
    });
    for input in rx {
        let r = match input {
            Input::Line(line) => match dev_command(&line) {
                Ok(DevCommand::Quit) => break,
                Ok(c) => dev_run(c, &mut stack, &mut bridge),
                Err(e) => Err(e),
            },
            Input::Bridge(ev) if ev["event"] == "scan" => {
                println!("the page asked to scan: a new pairing code");
                stack
                    .pairing_code()
                    .and_then(|qr| bridge.send(&format!("qr {qr}")))
            }
            Input::Bridge(ev) if ev["event"] == "page" => {
                let on = ev["connected"] == true;
                println!("page {}", if on { "connected" } else { "gone" });
                Ok(())
            }
            Input::Bridge(_) => Ok(()),
        };
        if let Err(e) = r {
            eprintln!("{e:#}");
        }
    }
    bridge.quit();
    stack.quit();
    Ok(())
}

enum Input {
    Line(String),
    Bridge(Value),
}

/// A command typed to `app dev`.
#[derive(Debug, PartialEq, Eq)]
enum DevCommand {
    Pair,
    /// For the host stack, with the event it answers with.
    Stack(&'static str, &'static str),
    /// For the bridge, as is.
    Bridge(String),
    Nothing,
    Quit,
}

fn dev_command(line: &str) -> Result<DevCommand> {
    Ok(match line.trim() {
        "" => DevCommand::Nothing,
        "pair" => DevCommand::Pair,
        "restart-relay" => DevCommand::Stack("restart-relay", "relay"),
        "stop-plugin" => DevCommand::Stack("stop-plugin", "plugin"),
        "start-plugin" => DevCommand::Stack("start-plugin", "plugin"),
        "status" => DevCommand::Stack("status", "status"),
        c @ ("network up" | "network down") => DevCommand::Bridge(c.to_string()),
        "quit" | "exit" => DevCommand::Quit,
        other => bail!("unknown command {other:?}\n{DEV_HELP}"),
    })
}

fn dev_run(c: DevCommand, stack: &mut Stack, bridge: &mut DeviceBridge) -> Result<()> {
    match c {
        DevCommand::Pair => {
            let qr = stack.pairing_code()?;
            bridge.send(&format!("qr {qr}"))?;
            println!("scanned a new pairing code");
        }
        DevCommand::Stack(command, answer) => {
            stack.send(command)?;
            println!("{}", stack.wait(answer)?);
        }
        DevCommand::Bridge(line) => bridge.send(&line)?,
        DevCommand::Nothing | DevCommand::Quit => {}
    }
    Ok(())
}

fn dev_port(args: &[String]) -> Result<Option<u16>> {
    match args {
        [] => Ok(None),
        [flag, n] if flag == "--port" => Ok(Some(n.parse().context("--port N")?)),
        _ => cmd::usage("usage: cargo xtask app dev [--port N]"),
    }
}

/// `app test-e2e`.
pub fn test_e2e(root: &Path, args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return cmd::usage("usage: cargo xtask app test-e2e");
    }
    let ui = root.join(UI);
    web::ensure_installed(&ui)?;
    build_device()?;
    let mut stack = Stack::start(root, true)?;
    if !stack.overlord {
        bail!("the host stack has no fake overlord: test-e2e needs tmux");
    }
    let qr = stack.pairing_code()?;
    let state = cmd::TempDir::new_in(&root.join("target"), "app-device")?;
    let mut bridge = DeviceBridge::start(root, state.path(), &qr, None)?;
    let mut c = web::tool(&ui, "vitest")?;
    c.args(["run", "--config", E2E_CONFIG]);
    for (k, v) in e2e_env(&bridge.url, &stack.control) {
        c.env(k, v);
    }
    let result = run(&mut c);
    bridge.quit();
    stack.quit();
    result
}

/// What the end-to-end tests are told.
fn e2e_env(bridge_url: &str, control: &str) -> [(&'static str, String); 2] {
    [
        ("RLORD_BRIDGE_URL", bridge_url.to_string()),
        ("RLORD_STACK_CONTROL", control.to_string()),
    ]
}

fn build_device() -> Result<()> {
    run(cmd::cargo().args([
        "build",
        "-p",
        "robot-overlord-e2e-device",
        "--bin",
        "rlord-device",
    ]))
}

/// `rlord-device bridge`, run for a task: its events (one JSON line each
/// on its stdout) and its stdin.
struct DeviceBridge {
    child: Child,
    stdin: ChildStdin,
    events: mpsc::Receiver<Value>,
    url: String,
    port: u16,
}

const BRIDGE_WAIT: Duration = Duration::from_secs(30);

impl DeviceBridge {
    fn start(root: &Path, state: &Path, qr: &str, origin: Option<&str>) -> Result<DeviceBridge> {
        let bin = root.join("target/debug/rlord-device");
        let mut child = Command::new(&bin)
            .args(bridge_args(state, qr, origin))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;
        let stdin = child.stdin.take().context("bridge stdin")?;
        let stdout = child.stdout.take().context("bridge stdout")?;
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                match serde_json::from_str::<Value>(&line) {
                    Ok(v) if v["event"].is_string() => {
                        if tx.send(v).is_err() {
                            break;
                        }
                    }
                    _ => eprintln!("bridge: {line}"),
                }
            }
        });
        let mut b = DeviceBridge {
            child,
            stdin,
            events,
            url: String::new(),
            port: 0,
        };
        let until = Instant::now() + BRIDGE_WAIT;
        let ready = loop {
            let left = until.saturating_duration_since(Instant::now());
            let v = b
                .events
                .recv_timeout(left)
                .context("rlord-device bridge never said it was ready")?;
            if v["event"] == "ready" {
                break v;
            }
        };
        (b.url, b.port) = ready_url(&ready).context("the bridge's ready line")?;
        println!("device bridge ready: {}", b.url);
        Ok(b)
    }

    fn send(&mut self, line: &str) -> Result<()> {
        writeln!(self.stdin, "{line}")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn quit(&mut self) {
        let _ = self.send("quit");
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for DeviceBridge {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A child process killed when dropped (the dev page's servers).
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bridge_args(state: &Path, qr: &str, origin: Option<&str>) -> Vec<String> {
    let mut a = vec![
        "bridge".to_string(),
        "--state".into(),
        state.to_string_lossy().into_owned(),
        "--port".into(),
        "0".into(),
        "--qr".into(),
        qr.to_string(),
    ];
    if let Some(o) = origin {
        a.extend(["--origin".into(), o.to_string()]);
    }
    a
}

/// The bridge's `ready` line: its URL and port.
fn ready_url(v: &Value) -> Option<(String, u16)> {
    let url = v["url"].as_str()?.to_string();
    let port = u16::try_from(v["port"].as_u64()?).ok()?;
    url.starts_with("ws://127.0.0.1:").then_some((url, port))
}

fn page_origin(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

fn page_url(port: u16) -> String {
    format!("{}/", page_origin(port))
}

fn preview_args(port: u16) -> Vec<String> {
    [
        "preview",
        "--mode",
        "app-dev",
        "--outDir",
        DEV_DIST,
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--strictPort",
    ]
    .map(String::from)
    .to_vec()
}

/// A free port on 127.0.0.1 (for the page's server).
fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).context("finding a free port")?;
    Ok(l.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_bridge_is_started_with_the_code_and_the_page_origin() {
        let a = bridge_args(Path::new("/s"), "RLORD1:q", Some("http://127.0.0.1:5"));
        assert_eq!(
            a,
            [
                "bridge",
                "--state",
                "/s",
                "--port",
                "0",
                "--qr",
                "RLORD1:q",
                "--origin",
                "http://127.0.0.1:5"
            ]
        );
        assert!(!bridge_args(Path::new("/s"), "q", None).contains(&"--origin".to_string()));
    }

    #[test]
    fn the_bridges_ready_line() {
        let v = json!({"event": "ready", "url": "ws://127.0.0.1:4100/", "port": 4100});
        assert_eq!(ready_url(&v), Some(("ws://127.0.0.1:4100/".into(), 4100)));
        // Never anything but loopback.
        let v = json!({"event": "ready", "url": "ws://0.0.0.0:4100/", "port": 4100});
        assert_eq!(ready_url(&v), None);
        assert_eq!(ready_url(&json!({"event": "ready"})), None);
    }

    #[test]
    fn the_page_is_served_on_loopback() {
        assert_eq!(page_url(5173), "http://127.0.0.1:5173/");
        let a = preview_args(5173);
        let at = |f: &str| a.iter().position(|x| x == f).map(|i| a[i + 1].as_str());
        assert_eq!(at("--host"), Some("127.0.0.1"));
        assert_eq!(at("--port"), Some("5173"));
        assert_eq!(at("--outDir"), Some(DEV_DIST));
        assert!(a.contains(&"--strictPort".to_string()));
    }

    #[test]
    fn dev_commands() {
        assert_eq!(dev_command("pair").unwrap(), DevCommand::Pair);
        assert_eq!(
            dev_command(" stop-plugin ").unwrap(),
            DevCommand::Stack("stop-plugin", "plugin")
        );
        assert_eq!(
            dev_command("network down").unwrap(),
            DevCommand::Bridge("network down".into())
        );
        assert_eq!(dev_command("").unwrap(), DevCommand::Nothing);
        assert_eq!(dev_command("quit").unwrap(), DevCommand::Quit);
        assert!(dev_command("rm -rf").is_err());
        assert_eq!(dev_port(&[]).unwrap(), None);
        assert_eq!(
            dev_port(&["--port".into(), "4000".into()]).unwrap(),
            Some(4000)
        );
        assert!(dev_port(&["--port".into()]).is_err());
    }

    #[test]
    fn the_tests_are_told_the_bridge_and_the_control_folder() {
        let env = e2e_env("ws://127.0.0.1:1/", "/c");
        assert_eq!(env[0], ("RLORD_BRIDGE_URL", "ws://127.0.0.1:1/".into()));
        assert_eq!(env[1], ("RLORD_STACK_CONTROL", "/c".into()));
    }
}
