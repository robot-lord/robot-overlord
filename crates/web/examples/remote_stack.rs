//! A host for the phone app's runs (`cargo xtask app dev`, `app test-e2e`,
//! `app test-ios`): an in-memory daemon with a few fixture sessions, the
//! real `rlord-web` it supervises (`RLORD_WEB_BIN`), and the real relay in
//! process on 127.0.0.1 (plain `ws`, which the address rules allow only on
//! loopback). No tmux session is started (tmux is pointed at a scratch
//! socket), no agent runs, nothing leaves loopback.
//!
//! It prints one JSON line per event on stdout and takes one command per
//! line on stdin, or as a file `<dir>/control/<name>.cmd` (the simulator's
//! test runner writes those; the stack answers `<name>.done`):
//!
//! - `pair`: a new pairing code (`{"event":"qr","qr":"RLORD1:…"}`);
//! - `restart-relay`: stop the relay and start it on the same port with the
//!   same key (a new `boot`, an empty queue);
//! - `stop-plugin`, `start-plugin`: stop or start `rlord-web`;
//! - `status`: `relay.status`;
//! - `quit` (or the end of stdin).
//!
//! With `--auto-confirm` a pending pairing is confirmed as soon as it
//! shows (`{"event":"confirmed","name","code"}`).
//!
//! Usage: `RLORD_WEB_BIN=… remote_stack --dir DIR [--sessions N] [--auto-confirm]`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::config::Config;
use rlord_core::db::{Db, NewSession};
use rlord_core::paths;
use rlord_daemon::Daemon;

struct Args {
    dir: PathBuf,
    sessions: usize,
    auto_confirm: bool,
}

fn args() -> Result<Args> {
    let mut a = Args {
        dir: PathBuf::new(),
        sessions: 3,
        auto_confirm: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(s) = it.next() {
        match s.as_str() {
            "--dir" => a.dir = it.next().context("--dir DIR")?.into(),
            "--sessions" => a.sessions = it.next().context("--sessions N")?.parse()?,
            "--auto-confirm" => a.auto_confirm = true,
            other => bail!("unknown argument {other}"),
        }
    }
    if a.dir.as_os_str().is_empty() {
        bail!("usage: remote_stack --dir DIR [--sessions N] [--auto-confirm]");
    }
    if std::env::var_os("RLORD_WEB_BIN").is_none() {
        bail!("RLORD_WEB_BIN must name the rlord-web binary");
    }
    Ok(a)
}

fn say(v: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

/// One user command over the daemon's socket, as the CLI sends it.
fn rpc(method: &str, params: Value) -> Result<Value> {
    let mut s = std::os::unix::net::UnixStream::connect(paths::socket_path())?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let line = json!({"id": 1, "method": method, "params": params}).to_string() + "\n";
    s.write_all(line.as_bytes())?;
    let mut answer = String::new();
    BufReader::new(s).read_line(&mut answer)?;
    let v: Value = serde_json::from_str(&answer)?;
    match v.get("error") {
        Some(e) => bail!("{method}: {}", e["message"].as_str().unwrap_or("?")),
        None => Ok(v["result"].clone()),
    }
}

/// rlord's folders under `dir` (the daemon's socket in a short temporary
/// folder: a Unix socket's path is limited), tmux on a scratch socket,
/// `[web] port = 0`.
fn environment(dir: &Path) -> Result<tempfile::TempDir> {
    std::fs::create_dir_all(dir)?;
    let run = tempfile::Builder::new().prefix("rls").tempdir()?;
    // Safety: set before any thread starts.
    std::env::set_var("XDG_DATA_HOME", dir.join("data"));
    std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
    std::env::set_var("XDG_RUNTIME_DIR", run.path());
    std::env::set_var(
        "RLORD_TMUX_SOCKET",
        format!("rlord-app-stack-{}", std::process::id()),
    );
    std::env::set_var("RLORD_NO_LOGIN_SHELL", "1");
    let cfg = paths::config_path();
    std::fs::create_dir_all(cfg.parent().context("config folder")?)?;
    std::fs::write(&cfg, "[web]\nport = 0\n")?;
    Ok(run)
}

/// Fixture rows: a few sessions, none running.
fn fixtures(db: &Db, n: usize) -> Result<()> {
    for i in 0..n {
        let id = format!("fixture-{i}");
        let name = format!("fixture session {i}");
        db.insert_session(&NewSession {
            id: &id,
            name: &name,
            agent: "claude",
            agent_session: Some(&id),
            project_path: "/tmp",
            tmux_session: None,
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })?;
    }
    Ok(())
}

struct Relay {
    running: Option<rlord_relay::Running>,
    dir: PathBuf,
    listen: std::net::SocketAddr,
}

impl Relay {
    async fn start(dir: &Path, listen: std::net::SocketAddr) -> Result<Relay> {
        let mut cfg = rlord_relay::Config::new(dir);
        cfg.listen = listen;
        let running = rlord_relay::start(cfg).await?;
        let listen = running.addr();
        Ok(Relay {
            running: Some(running),
            dir: dir.to_path_buf(),
            listen,
        })
    }

    fn get(&self) -> &rlord_relay::Running {
        self.running.as_ref().expect("the relay runs")
    }

    async fn restart(&mut self) -> Result<()> {
        if let Some(r) = self.running.take() {
            r.shutdown().await;
        }
        let mut cfg = rlord_relay::Config::new(&self.dir);
        cfg.listen = self.listen;
        self.running = Some(rlord_relay::start(cfg).await?);
        Ok(())
    }
}

async fn command(line: &str, daemon: &Arc<Daemon>, relay: &mut Relay) -> Result<bool> {
    match line.trim() {
        "" => {}
        "pair" => {
            let p = tokio::task::spawn_blocking(|| rpc("relay.pair", json!({}))).await??;
            say(json!({"event": "qr", "qr": p["qr"]}));
        }
        "restart-relay" => {
            relay.restart().await?;
            say(json!({"event": "relay", "boot": relay.get().boot()}));
        }
        "stop-plugin" => {
            let d = daemon.clone();
            tokio::task::spawn_blocking(move || d.web().stop()).await?;
            say(json!({"event": "plugin", "running": false}));
        }
        "start-plugin" => {
            let d = daemon.clone();
            tokio::task::spawn_blocking(move || d.web().start(false)).await??;
            say(json!({"event": "plugin", "running": true}));
        }
        "status" => {
            let s = tokio::task::spawn_blocking(|| rpc("relay.status", json!({}))).await??;
            say(json!({"event": "status", "status": s}));
        }
        "quit" => return Ok(false),
        other => say(json!({"event": "error", "message": format!("unknown command {other}")})),
    }
    Ok(true)
}

/// Confirms a pending pairing as soon as it shows.
async fn auto_confirm() {
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let Ok(Ok(s)) = tokio::task::spawn_blocking(|| rpc("relay.status", json!({}))).await else {
            continue;
        };
        if s["pending"].is_object() {
            let pending = s["pending"].clone();
            if let Ok(Ok(_)) = tokio::task::spawn_blocking(|| rpc("relay.confirm", json!({}))).await
            {
                say(
                    json!({"event": "confirmed", "name": pending["name"], "code": pending["code"]}),
                );
            }
        }
    }
}

/// Commands from stdin and from `<dir>/control/*.cmd`, one channel.
fn commands(control: PathBuf) -> tokio::sync::mpsc::Receiver<(String, Option<PathBuf>)> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stdin_tx = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if stdin_tx.blocking_send((line, None)).is_err() {
                return;
            }
        }
        let _ = stdin_tx.blocking_send(("quit".into(), None));
    });
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));
        let Ok(entries) = std::fs::read_dir(&control) else {
            continue;
        };
        let mut cmds: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "cmd"))
            .collect();
        cmds.sort();
        for p in cmds {
            let line = std::fs::read_to_string(&p).unwrap_or_default();
            let _ = std::fs::remove_file(&p);
            if tx
                .blocking_send((line, Some(p.with_extension("done"))))
                .is_err()
            {
                return;
            }
        }
    });
    rx
}

fn main() -> Result<()> {
    let a = args()?;
    let _run = environment(&a.dir)?;
    let control = a.dir.join("control");
    std::fs::create_dir_all(&control)?;
    let db = Db::open_in_memory()?;
    fixtures(&db, a.sessions)?;
    let daemon = Daemon::new(db, Config::load()?);
    // A debug plugin built from this checkout: no compiled-in hash.
    daemon.web().set_expected_hash(None);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let listener = Daemon::bind(&paths::socket_path())?;
        let d = daemon.clone();
        tokio::spawn(async move { d.serve(listener).await });
        let d = daemon.clone();
        tokio::task::spawn_blocking(move || d.web().start(false)).await??;
        let mut relay = Relay::start(&a.dir.join("relay"), "127.0.0.1:0".parse()?).await?;
        let token = relay
            .get()
            .admin()
            .token_new(1, Duration::from_secs(600), "local")?;
        let address = relay.get().address().to_string();
        tokio::task::spawn_blocking(move || {
            rpc(
                "relay.setup",
                json!({"address": address, "token": token, "reset": true}),
            )
        })
        .await??;
        say(json!({
            "event": "ready",
            "relay": relay.get().origin(),
            "control": control,
            "sessions": a.sessions,
        }));
        if a.auto_confirm {
            tokio::spawn(auto_confirm());
        }
        let mut rx = commands(control);
        while let Some((line, done)) = rx.recv().await {
            let go_on = match command(&line, &daemon, &mut relay).await {
                Ok(go_on) => go_on,
                Err(e) => {
                    say(json!({"event": "error", "message": e.to_string()}));
                    true
                }
            };
            if let Some(done) = done {
                let _ = std::fs::write(done, "");
            }
            if !go_on {
                break;
            }
        }
        if let Some(r) = relay.running.take() {
            r.shutdown().await;
        }
        let d = daemon.clone();
        tokio::task::spawn_blocking(move || d.web().stop()).await?;
        anyhow::Ok(())
    })?;
    rt.shutdown_background();
    Ok(())
}
