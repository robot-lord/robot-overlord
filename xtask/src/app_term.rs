//! `cargo xtask app test-term`: the real Claude Code's terminal on the
//! phone's code, end to end, with no model and no phone. It starts:
//!
//! - a demo rlord with the real Claude Code (pinned) on the fake model
//!   (`scripts/demo/demo.sh --real-claude --web`, its own tmux socket and
//!   a free `[web] port`);
//! - the real `rlord-relay` on 127.0.0.1 (plain `ws`, loopback only);
//! - the demo host set up for that relay, and `rlord-device bridge` (the
//!   phone's engine) paired with it, the pairing confirmed by the CLI;
//! - the Vitest project `vitest.app-term.config.ts`, which feeds a
//!   headless xterm.js through the page's `RemotePaneLink` and reads the
//!   screen as text.
//!
//! The tests are told (environment): `RLORD_BRIDGE_URL`,
//! `RLORD_TERM_DIR` (the demo's folder: its `fake-model.log` and the
//! sessions' folders under `src/`), `RLORD_TERM_TMUX` and
//! `RLORD_TERM_SOCKET` (to look at or drive the demo's tmux),
//! `RLORD_TERM_RLORD` (the demo's `rlord`, with `RLORD_TERM_ENV` its
//! environment as JSON) and `RLORD_TERM_CONTROL` (a folder: a file
//! `<name>.cmd` holding `network down` or `network up` goes to the
//! bridge, answered by `<name>.done`). After the run the fake model's log
//! must have no `BLOCKED` line.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::app_dev::{build_device, DeviceBridge};
use crate::cmd::{self, run};
use crate::web;

const UI: &str = "crates/web/ui";
const TERM_CONFIG: &str = "vitest.app-term.config.ts";
const WAIT: Duration = Duration::from_secs(60);

pub fn test_term(root: &Path, args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return cmd::usage("usage: cargo xtask app test-term");
    }
    let tmux = which("tmux").context("test-term needs tmux")?;
    let ui = root.join(UI);
    web::ensure_installed(&ui)?;
    build_device()?;
    run(cmd::cargo().args([
        "build",
        "-p",
        "robot-overlord-relay",
        "--bin",
        "rlord-relay",
    ]))?;
    // A short folder: the daemon's unix socket lives under it.
    let demo = Demo::start(root)?;
    let relay = Relay::start(root, &demo.dir.join("relay"))?;
    let env = demo.env()?;
    let mut rlord = |args: &[&str]| -> Command {
        let mut c = Command::new(demo.dir.join("bin/rlord"));
        c.args(args).envs(&env);
        c
    };
    run(&mut rlord(&[
        "relay",
        "setup",
        &relay.address,
        "--token",
        &relay.token,
    ]))?;
    // The offer (its QR as text on the first line); it waits for the
    // device and, with no terminal, leaves it for `relay confirm`.
    let mut pair = rlord(&["relay", "pair", "--text"])
        .stdout(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .context("rlord relay pair")?;
    let qr = BufReader::new(pair.stdout.take().context("pair stdout")?)
        .lines()
        .map_while(std::result::Result::ok)
        .find(|l| l.starts_with("RLORD1:"))
        .context("no pairing QR")?;
    // The host links to the relay for the pairing: only then is the
    // offer's rendezvous there for the device.
    let until = Instant::now() + WAIT;
    while !cmd::output(&mut rlord(&["relay", "status"]))
        .unwrap_or_default()
        .contains("link:    connected")
    {
        if Instant::now() > until {
            bail!("the demo host never linked to the relay");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let state = cmd::TempDir::new_in(&root.join("target"), "term-device")?;
    let mut bridge = DeviceBridge::start(root, state.path(), &qr, None)?;
    // Control files for the tests (the bridge's network).
    let control = demo.dir.join("term-control");
    std::fs::create_dir_all(&control)?;
    let mut c = web::tool(&ui, "vitest")?;
    c.args(["run", "--config", TERM_CONFIG])
        .env("RLORD_BRIDGE_URL", &bridge.url)
        .env("RLORD_TERM_DIR", &demo.dir)
        .env("RLORD_TERM_TMUX", &tmux)
        .env("RLORD_TERM_SOCKET", &demo.socket)
        .env("RLORD_TERM_RLORD", demo.dir.join("bin/rlord"))
        .env("RLORD_TERM_ENV", serde_json::to_string(&env)?)
        .env("RLORD_TERM_CONTROL", &control);
    let mut child = c.spawn().context("starting vitest")?;
    // The tests continue the pairing (as the page does); the host's user
    // confirms it here once it waits.
    let mut confirmed = false;
    let mut last_try = Instant::now() - Duration::from_secs(1);
    let status = loop {
        if !confirmed && last_try.elapsed() > Duration::from_millis(500) {
            last_try = Instant::now();
            confirmed = try_confirm(&mut rlord);
            if confirmed {
                // (It may not have seen the device before the confirm: it
                // would wait out the offer.)
                let _ = pair.kill();
                let _ = pair.wait();
            }
        }
        for (name, line) in pending_commands(&control)? {
            let r = bridge.send(&line);
            std::fs::write(
                control.join(format!("{name}.done")),
                if r.is_ok() { "ok" } else { "error" },
            )?;
        }
        if let Some(s) = child.try_wait()? {
            break s;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = pair.kill();
    let _ = pair.wait();
    bridge.quit();
    drop(relay);
    let log = std::fs::read_to_string(demo.dir.join("fake-model.log")).unwrap_or_default();
    drop(demo);
    if !status.success() {
        bail!("the terminal tests failed ({status})");
    }
    let blocked = blocked_lines(&log);
    if !blocked.is_empty() {
        bail!(
            "Claude Code tried to reach something besides the fake model:\n{}",
            blocked.join("\n")
        );
    }
    Ok(())
}

/// `<name>.cmd` files not answered yet, oldest name first, read and
/// removed.
fn pending_commands(dir: &Path) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "cmd"))
        .collect();
    names.sort();
    for p in names {
        let line = std::fs::read_to_string(&p)?.trim().to_string();
        std::fs::remove_file(&p)?;
        let name = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push((name, line));
    }
    Ok(out)
}

/// The fake model's log lines that say a request went somewhere else.
fn blocked_lines(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|l| l.contains("BLOCKED") || l.contains("auth=UNEXPECTED"))
        .collect()
}

/// `rlord relay confirm`: true once it worked (it fails until a device
/// waits).
fn try_confirm(rlord: &mut impl FnMut(&[&str]) -> Command) -> bool {
    rlord(&["relay", "confirm"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(program))
            .find(|f| f.is_file())
    })
}

/// The demo rlord, stopped (and its folder removed) when dropped.
struct Demo {
    root: PathBuf,
    dir: PathBuf,
    socket: String,
}

impl Demo {
    fn start(root: &Path) -> Result<Demo> {
        let id = std::process::id();
        let dir = PathBuf::from(format!("/tmp/rlord-term-{id}"));
        let _ = std::fs::remove_dir_all(&dir);
        let socket = format!("rlord-term-{id}");
        let d = Demo {
            root: root.to_path_buf(),
            dir: dir.clone(),
            socket: socket.clone(),
        };
        run(Command::new(root.join("scripts/demo/demo.sh"))
            .args(["--real-claude", "--web"])
            .arg(&dir)
            .env("RLORD_DEMO_SOCKET", &socket)
            // Answers slow enough to interrupt one with Esc.
            .env("RLORD_FAKE_MODEL_DELAY_MS", "250"))?;
        // The plugin on a free port of its own (another demo or a real
        // install may have the default one).
        let cfg = dir.join("config/robot-overlord/config.toml");
        let mut text = std::fs::read_to_string(&cfg)?;
        text.push_str(&format!("\n[web]\nport = {}\n", free_port()?));
        std::fs::write(&cfg, text)?;
        Ok(d)
    }

    /// The demo's environment (`env.sh`'s exports).
    fn env(&self) -> Result<BTreeMap<String, String>> {
        let text = std::fs::read_to_string(self.dir.join("env.sh"))?;
        Ok(parse_env(&text, &std::env::var("PATH").unwrap_or_default()))
    }
}

impl Drop for Demo {
    fn drop(&mut self) {
        // (RLORD_TERM_KEEP=1: leave it running, to look at after a failure.)
        if std::env::var("RLORD_TERM_KEEP").as_deref() == Ok("1") {
            eprintln!(
                "kept the demo: scripts/demo/demo.sh --stop {}",
                self.dir.display()
            );
            return;
        }
        let _ = Command::new(self.root.join("scripts/demo/demo.sh"))
            .arg("--stop")
            .arg(&self.dir)
            .status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `export A=1 B=2` lines as a map (`$PATH` as given).
fn parse_env(text: &str, path: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("export ") else {
            continue;
        };
        for kv in rest.split_whitespace() {
            if let Some((k, v)) = kv.split_once('=') {
                m.insert(k.to_string(), v.replace("$PATH", path));
            }
        }
    }
    m
}

fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).context("finding a free port")?;
    Ok(l.local_addr()?.port())
}

/// The real relay on 127.0.0.1, killed when dropped.
struct Relay {
    child: Child,
    address: String,
    token: String,
}

impl Relay {
    fn start(root: &Path, dir: &Path) -> Result<Relay> {
        std::fs::create_dir_all(dir)?;
        let conf = dir.join("relay.toml");
        let port = free_port()?;
        std::fs::write(
            &conf,
            format!(
                "origin = \"http://127.0.0.1:{port}\"\nlisten = \"127.0.0.1:{port}\"\n\
                 data_dir = \"{}\"\n[tls]\nmode = \"proxy\"\n",
                dir.display()
            ),
        )?;
        let bin = root.join("target/debug/rlord-relay");
        let token = cmd::output(
            Command::new(&bin)
                .arg("--config")
                .arg(&conf)
                .args(["token", "new"]),
        )?
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.contains(' '))
        .context("rlord-relay token new printed no token")?
        .to_string();
        let address = cmd::output(Command::new(&bin).arg("--config").arg(&conf).arg("address"))?
            .trim()
            .to_string();
        let child = Command::new(&bin)
            .arg("--config")
            .arg(&conf)
            .arg("serve")
            .stdout(Stdio::null())
            .spawn()
            .context("starting rlord-relay")?;
        let until = Instant::now() + WAIT;
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            if Instant::now() > until {
                bail!("rlord-relay never listened on 127.0.0.1:{port}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(Relay {
            child,
            address,
            token,
        })
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demos_env_is_read() {
        let text = "export XDG_DATA_HOME=/tmp/d/data XDG_CONFIG_HOME=/tmp/d/config\n\
                    # a comment\nexport PATH=/tmp/d/bin:$PATH\n";
        let m = parse_env(text, "/usr/bin");
        assert_eq!(m["XDG_DATA_HOME"], "/tmp/d/data");
        assert_eq!(m["PATH"], "/tmp/d/bin:/usr/bin");
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn only_other_destinations_are_blocked_lines() {
        let log = "POST /v1/messages 200\nBLOCKED CONNECT api.example.com:443\nauth=UNEXPECTED x\n";
        assert_eq!(blocked_lines(log).len(), 2);
    }

    #[test]
    fn commands_are_taken_in_order() {
        let tmp = cmd::TempDir::new_in(&std::env::temp_dir(), "term-cmds").unwrap();
        std::fs::write(tmp.path().join("002-up.cmd"), "network up\n").unwrap();
        std::fs::write(tmp.path().join("001-down.cmd"), "network down").unwrap();
        std::fs::write(tmp.path().join("000-x.done"), "ok").unwrap();
        let c = pending_commands(tmp.path()).unwrap();
        assert_eq!(
            c,
            vec![
                ("001-down".to_string(), "network down".to_string()),
                ("002-up".to_string(), "network up".to_string())
            ]
        );
        assert!(pending_commands(tmp.path()).unwrap().is_empty());
    }
}
