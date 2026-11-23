//! The daemon running the web plugin (`web.rs`, `Web`), with a stand-in
//! plugin: a shell script (`RLORD_WEB_BIN`, or the installed path for the
//! hash check) that runs this test binary's `standin` test in a mode. No
//! network, no tmux, no agents.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use rlord_core::config::Config;
use rlord_core::db::Db;
use rlord_core::plugin;
use rlord_daemon::Daemon;

/// The stand-in plugin: does nothing unless run by a stand-in script
/// (`RLORD_STANDIN_MODE`), then plays a plugin and exits.
#[test]
fn standin() {
    let Ok(mode) = std::env::var("RLORD_STANDIN_MODE") else {
        return;
    };
    if let Ok(count) = std::env::var("RLORD_STANDIN_COUNT") {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(count)
            .unwrap();
        writeln!(f, "started").unwrap();
    }
    match mode.as_str() {
        "exit3" => std::process::exit(plugin::EXIT_INCOMPATIBLE),
        "exit4" => std::process::exit(plugin::EXIT_PORT_IN_USE),
        "crash" => {
            eprintln!("stand-in: crashed");
            std::process::exit(1)
        }
        "silent" => {
            std::thread::sleep(Duration::from_secs(30));
            std::process::exit(0)
        }
        _ => {}
    }
    if mode == "slow" {
        std::thread::sleep(Duration::from_millis(300));
    }
    if mode == "stubborn" {
        // Safety: ignoring a signal in this process only.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }
    let sock = std::env::var("RLORD_STANDIN_SOCK").unwrap();
    let s = UnixStream::connect(sock).unwrap();
    let mut w = s.try_clone().unwrap();
    writeln!(
        w,
        "{}",
        json!({"id": 1, "method": "web.hello", "params": {
            "protocol": plugin::WEB_PROTOCOL, "version": "stand-in",
            "pid": std::process::id(), "port": 4242}})
    )
    .unwrap();
    let mut lines = BufReader::new(s).lines();
    let _reply = lines.next();
    if mode == "relay" {
        relay_standin();
    }
    // Stay until the daemon goes (or a signal ends it).
    for l in lines {
        if l.is_err() {
            break;
        }
    }
    std::process::exit(0)
}

/// The stand-in's `relay` mode, after its hello: subscribe as the plugin
/// does, and answer every relay command with its pid; until the daemon
/// goes (or a signal ends it).
fn relay_standin() {
    let sock = std::env::var("RLORD_STANDIN_SOCK").unwrap();
    let s = UnixStream::connect(&sock).unwrap();
    let mut w = s.try_clone().unwrap();
    writeln!(w, "{}", json!({"id": 1, "method": "web.subscribe"})).unwrap();
    for l in BufReader::new(s).lines() {
        let Ok(l) = l else { break };
        let v: Value = serde_json::from_str(&l).unwrap_or_default();
        if v["notify"] != "remote" {
            continue;
        }
        let mut c = UnixStream::connect(&sock).unwrap();
        writeln!(
            c,
            "{}",
            json!({"id": 2, "method": "web.remote_reply", "params": {
                "id": v["id"], "ok": {"pid": std::process::id()}}})
        )
        .unwrap();
        let _ = BufReader::new(c).lines().next();
    }
    std::process::exit(0)
}

/// Tests take turns: `RLORD_WEB_BIN` and the data folder are the
/// process's.
static LOCK: Mutex<()> = Mutex::new(());

fn base() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("web-supervise");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Safety: set once, under the lock, before any daemon starts.
        std::env::set_var("RLORD_TMUX_SOCKET", "rlord-test-web-supervise");
        std::env::set_var("RLORD_NO_LOGIN_SHELL", "1");
        std::env::set_var("XDG_DATA_HOME", dir.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
        std::env::set_var("XDG_RUNTIME_DIR", dir.join("run"));
        dir
    })
}

struct Rig {
    _g: MutexGuard<'static, ()>,
    daemon: Arc<Daemon>,
    sock: PathBuf,
    dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Rig {
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        base();
        // Safety: under the lock.
        std::env::remove_var("RLORD_WEB_BIN");
        let _ = std::fs::remove_file(plugin::installed_path());
        let _ = std::fs::remove_file(plugin::log_path());
        let daemon = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        daemon.web().set_expected_hash(None);
        daemon.web().set_backoff_base(Duration::from_millis(20));
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let (tx, rx) = std::sync::mpsc::channel();
        let (d2, s2) = (daemon.clone(), sock.clone());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let listener = Daemon::bind(&s2).unwrap();
                tx.send(()).unwrap();
                let _ = d2.serve(listener).await;
            });
        });
        rx.recv().unwrap();
        Rig {
            _g: g,
            daemon,
            sock,
            dir,
        }
    }

    fn count_file(&self) -> PathBuf {
        self.dir.path().join("count")
    }

    /// Times the stand-in was started.
    fn starts(&self) -> usize {
        std::fs::read_to_string(self.count_file())
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// A stand-in script at `at` in `mode`.
    fn script(&self, at: &Path, mode: &str) {
        let exe = std::env::current_exe().unwrap();
        let text = format!(
            "#!/bin/sh\n\
             export RLORD_STANDIN_MODE={mode}\n\
             export RLORD_STANDIN_SOCK='{}'\n\
             export RLORD_STANDIN_COUNT='{}'\n\
             exec '{}' standin --exact --nocapture --test-threads=1\n",
            self.sock.display(),
            self.count_file().display(),
            exe.display()
        );
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, text).unwrap();
        std::fs::set_permissions(at, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Use a stand-in in `mode` through `RLORD_WEB_BIN`.
    fn standin(&self, mode: &str) {
        let at = self.dir.path().join(format!("rlord-web-{mode}"));
        self.script(&at, mode);
        // Safety: under the lock.
        std::env::set_var("RLORD_WEB_BIN", &at);
    }

    fn raw(&self, method: &str) -> Value {
        let mut s = UnixStream::connect(&self.sock).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        writeln!(s, "{}", json!({"id": 1, "method": method, "params": {}})).unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn ok(&self, method: &str) -> Value {
        let r = self.raw(method);
        assert!(r.get("error").is_none(), "{method}: {r}");
        r["result"].clone()
    }

    fn err(&self, method: &str) -> String {
        let r = self.raw(method);
        r["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} didn't fail: {r}"))
            .to_string()
    }

    fn wait_state(&self, state: &str) -> Value {
        let start = Instant::now();
        loop {
            let st = self.ok("web.status");
            if st["state"] == state {
                return st;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "never {state}: {st}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.daemon.web().stop();
    }
}

fn alive(pid: u64) -> bool {
    // Safety: signal 0 only checks the process exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[test]
fn start_waits_for_hello_and_stop_ends_it() {
    let r = Rig::new();
    r.standin("slow");
    let t = Instant::now();
    let st = r.ok("web.start");
    assert!(
        t.elapsed() >= Duration::from_millis(300),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(st["state"], "running", "{st}");
    assert_eq!(st["port"], 4242);
    assert_eq!(st["installed"], true);
    assert_eq!(st["verified"], false);
    let pid = st["pid"].as_u64().unwrap();
    assert!(alive(pid));
    // Started already: the same one.
    assert_eq!(r.ok("web.start")["pid"], pid);
    let st = r.ok("web.stop");
    assert_eq!(st["state"], "stopped", "{st}");
    assert!(st["port"].is_null() && st["pid"].is_null(), "{st}");
    assert!(!alive(pid));
    // Stopped is stopped: not restarted.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(r.starts(), 1);
}

/// Stopped, then a relay command at once; stopped and started again at
/// once, then a relay command: each reaches the plugin running now, never
/// the stopped one's stream (which the daemon has let go of by the time
/// `web.stop` returns).
#[test]
fn a_relay_command_right_after_a_stop_or_restart_reaches_the_new_plugin() {
    let r = Rig::new();
    r.standin("relay");
    let mut pid = r.ok("web.start")["pid"].clone();
    for i in 0..5 {
        r.ok("web.stop");
        // (It starts the plugin, and waits for its stream.)
        let a = r.ok("relay.pair_cancel");
        let now = r.ok("web.status")["pid"].clone();
        assert_ne!(now, pid, "stop {i}");
        assert_eq!(a["pid"], now, "stop {i}: {a}");
        r.ok("web.stop");
        pid = r.ok("web.start")["pid"].clone();
        assert_ne!(pid, now, "restart {i}");
        let a = r.ok("relay.pair_cancel");
        assert_eq!(a["pid"], pid, "restart {i}: {a}");
    }
}

#[test]
fn a_plugin_ignoring_sigterm_is_killed() {
    let r = Rig::new();
    r.standin("stubborn");
    let pid = r.ok("web.start")["pid"].as_u64().unwrap();
    let t = Instant::now();
    r.ok("web.stop");
    assert!(t.elapsed() >= Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(!alive(pid));
}

#[test]
fn a_crash_is_retried_with_backoff_then_given_up() {
    let r = Rig::new();
    r.standin("crash");
    let e = r.err("web.start");
    assert!(e.contains("didn't start"), "{e}");
    let st = r.wait_state("failed");
    assert_eq!(r.starts(), 5, "{st}");
    assert!(st["why"].as_str().unwrap().contains("5 times"), "{st}");
    // (After libtest's own words on the same line.)
    assert!(
        st["last_error"]
            .as_str()
            .unwrap()
            .ends_with("stand-in: crashed"),
        "{st}"
    );
    // An explicit start tries again.
    let _ = r.err("web.start");
    assert!(r.starts() > 5);
}

#[test]
fn incompatible_and_port_in_use_are_not_retried() {
    for (mode, says) in [("exit3", "protocol"), ("exit4", "port is in use")] {
        let r = Rig::new();
        r.standin(mode);
        let e = r.err("web.start");
        assert!(e.contains(says), "{mode}: {e}");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(r.starts(), 1, "{mode}");
        let st = r.ok("web.status");
        assert_eq!(st["state"], "failed", "{st}");
        assert!(st["why"].as_str().unwrap().contains(says), "{st}");
    }
}

#[test]
fn a_hash_mismatch_is_reported_and_nothing_is_spawned() {
    let r = Rig::new();
    // A release build: only the installed plugin, and only with its hash.
    r.daemon.web().set_expected_hash(Some("00".repeat(32)));
    let e = r.err("web.start");
    assert!(e.contains("not installed"), "{e}");
    r.script(&plugin::installed_path(), "slow");
    let e = r.err("web.start");
    assert!(e.contains("not the one this rlord was built with"), "{e}");
    let st = r.ok("web.status");
    assert_eq!(st["installed"], false, "{st}");
    assert!(st["problem"].as_str().unwrap().contains("sha256"), "{st}");
    assert_eq!(st["state"], "failed");
    // RLORD_WEB_BIN doesn't get round it.
    r.standin("slow");
    let _ = r.err("web.start");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(r.starts(), 0);
    // The right hash: it starts, verified.
    let h = plugin::sha256_file(&plugin::installed_path()).unwrap();
    r.daemon.web().set_expected_hash(Some(h));
    let st = r.ok("web.start");
    assert_eq!(
        (st["state"].as_str(), st["verified"].as_bool()),
        (Some("running"), Some(true))
    );
}

#[test]
fn rotate_drops_the_token_and_restarts() {
    let r = Rig::new();
    r.standin("hello");
    let pid = r.ok("web.start")["pid"].as_u64().unwrap();
    std::fs::write(plugin::token_path(), "old").unwrap();
    let st = r.ok("web.rotate");
    assert_eq!(st["state"], "running", "{st}");
    assert_ne!(st["pid"].as_u64().unwrap(), pid);
    assert!(!alive(pid));
    assert!(!plugin::token_path().exists());
}

#[test]
fn a_hello_from_another_process_is_not_the_plugin() {
    let r = Rig::new();
    let mut s = UnixStream::connect(&r.sock).unwrap();
    writeln!(
        s,
        "{}",
        json!({"id": 1, "method": "web.hello", "params": {
            "protocol": plugin::WEB_PROTOCOL, "version": "x", "pid": 1, "port": 9}})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    assert!(line.contains("\"result\""), "{line}");
    let st = r.ok("web.status");
    assert_eq!(st["state"], "stopped", "{st}");
    assert!(st["port"].is_null());
}
