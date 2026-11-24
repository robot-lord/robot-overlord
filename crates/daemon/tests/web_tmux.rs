//! The web UI's tmux behaviours, on a scratch tmux server (socket
//! `rlord-test-web`). Gated: `RLORD_TMUX_TESTS=1 cargo test`. Never touches
//! `tmux -L rlord` or the default server; no agents, only `cat`/`sleep`.
//!
//! A browser pane is a real tmux client on a PTY, attached to a throwaway
//! "view session" grouped with the target (`docs/working/web-ui-remote.md`,
//! "Pane streaming"). These tests drive such clients through a PTY, as the
//! bridge will.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rlord_core::tmux;

/// One scratch server for the whole file: tests take turns.
static LOCK: Mutex<()> = Mutex::new(());

const SOCKET: &str = "rlord-test-web";

/// `None` unless `RLORD_TMUX_TESTS=1`. Points rlord's tmux at the scratch
/// socket, its data and config dirs at a scratch folder (so the generated
/// tmux.conf is rlord's defaults, without the user's profile or tweaks),
/// and starts from an empty server.
fn setup() -> Option<MutexGuard<'static, ()>> {
    if std::env::var("RLORD_TMUX_TESTS").as_deref() != Ok("1") {
        return None;
    }
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("web-tmux");
    std::fs::create_dir_all(&dir).unwrap();
    // Safety: this test process only, and the same values every time.
    std::env::set_var("RLORD_TMUX_SOCKET", SOCKET);
    std::env::set_var("XDG_DATA_HOME", &dir);
    std::env::set_var("XDG_CONFIG_HOME", &dir);
    tx(&["kill-server"]);
    Some(guard)
}

/// Run a tmux command on the scratch server; stdout, trimmed (empty on
/// failure).
fn tx(args: &[&str]) -> String {
    let out = tmux::command().args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Whether a tmux command succeeds; its stderr otherwise.
fn tx_ok(args: &[&str]) -> Result<(), String> {
    let out = tmux::command().args(args).output().unwrap();
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A tmux client running in a PTY, as the web bridge runs one. The PTY is
/// plain `openpty` (libc), so the test needs no PTY crate.
struct PtyClient {
    child: std::process::Child,
    master: std::fs::File,
    /// Everything the client has drawn so far.
    out: Arc<Mutex<Vec<u8>>>,
}

impl PtyClient {
    /// `tmux <args>` (on the scratch server) in a fresh PTY.
    fn spawn(args: &[&str], cols: u16, rows: u16) -> PtyClient {
        let mut cmd = tmux::command();
        cmd.env_remove("TMUX")
            .env("TERM", "xterm-256color")
            .args(args);
        PtyClient::run(cmd, cols, rows)
    }

    /// `cmd` in a fresh PTY, as its controlling terminal.
    fn run(mut cmd: std::process::Command, cols: u16, rows: u16) -> PtyClient {
        use std::os::fd::FromRawFd;
        use std::os::unix::process::CommandExt;
        let (mut m, mut s) = (0, 0);
        let mut ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // Safety: plain libc calls on fds this function owns.
        unsafe {
            let rc = libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                // Raw: Linux's libc takes `*const winsize`, macOS's `*mut`.
                &raw mut ws,
            );
            assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
            libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(s, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        // Safety: freshly opened, owned here.
        let (master, slave) =
            unsafe { (std::fs::File::from_raw_fd(m), std::fs::File::from_raw_fd(s)) };
        cmd.stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        // Safety: only async-signal-safe calls between fork and exec. The
        // PTY becomes the child's controlling terminal (stdin is the slave).
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        drop(cmd);
        drop(slave);
        let mut reader = master.try_clone().unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&out);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // EIO once the client has exited.
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        PtyClient { child, master, out }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    fn drawn(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    fn exited(&mut self) -> bool {
        self.child.try_wait().unwrap().is_some()
    }

    fn close(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A view session as the web bridge makes it: grouped with `target`,
/// status row off, an empty key table, gone when its client leaves. One
/// tmux invocation, attached.
fn attach_view(target: &str, view: &str, cols: u16, rows: u16) -> PtyClient {
    let v = format!("={view}:");
    let c = PtyClient::spawn(
        &[
            "new-session",
            "-t",
            &format!("={target}"),
            "-s",
            view,
            ";",
            "set-option",
            "-t",
            &v,
            "status",
            "off",
            ";",
            "set-option",
            "-t",
            &v,
            "key-table",
            "rlord-web",
            ";",
            "set-option",
            "-t",
            &v,
            "destroy-unattached",
            "on",
        ],
        cols,
        rows,
    );
    wait_for("the view session's options", || {
        tx(&["show-options", "-v", "-t", &v, "destroy-unattached"]) == "on"
    });
    c
}

/// A pane that logs every byte it gets (raw mode, no echo) and asks for
/// SGR mouse reports, like a mouse-aware agent.
fn probe_cmd(log: &std::path::Path) -> String {
    format!(
        "stty raw -echo; printf '\\033[?1000h\\033[?1006hPROBE-READY'; exec cat > {}",
        tmux::shell_quote(&log.to_string_lossy())
    )
}

fn read_log(log: &std::path::Path) -> String {
    String::from_utf8_lossy(&std::fs::read(log).unwrap_or_default()).into_owned()
}

/// Step 1 of the slice-1 plan: the tmux behaviours the view session rests
/// on, before anything depends on them.
#[test]
fn view_session_behaviours() {
    let Some(_g) = setup() else { return };
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("web-tmux");
    let log = dir.join("probe.log");
    let _ = std::fs::remove_file(&log);
    let target = "rlord_webtarget";
    tmux::new_session(target, "/tmp", &probe_cmd(&log)).unwrap();
    // The target shows a status row of its own (as the overlord does).
    tx_ok(&["set-option", "-t", &format!("={target}:"), "status", "on"]).unwrap();
    // A root binding the browser must never reach (rlord's nav keys live
    // in root).
    tx_ok(&[
        "bind-key",
        "-n",
        "F12",
        "set-option",
        "-g",
        "@web_fired",
        "yes",
    ])
    .unwrap();
    wait_for("the probe", || {
        tmux::capture_pane(target).unwrap().contains("PROBE-READY")
    });
    let target_window = tmux::current_window_id(target).unwrap();

    // 1. The grouped session shows the target's window and pane.
    let mut c = attach_view(target, "webview-1", 80, 24);
    assert_eq!(tmux::current_window_id("webview-1").unwrap(), target_window);
    let group = tx(&[
        "display-message",
        "-p",
        "-t",
        "=webview-1:",
        "#{session_group}",
    ]);
    assert_eq!(group, target);
    wait_for("the target's screen in the client", || {
        c.drawn().contains("PROBE-READY")
    });

    // 2. `status off` on the view session: no status row for the client
    //    (the window takes all 24 rows), and the target keeps its own.
    wait_for("the window to take the client's size", || {
        tx(&[
            "display-message",
            "-p",
            "-t",
            &target_window,
            "#{window_height}",
        ]) == "24"
    });
    assert_eq!(
        tx(&["show-options", "-v", "-t", &format!("={target}:"), "status"]),
        "on"
    );
    assert_eq!(
        tx(&["show-options", "-v", "-t", "=webview-1:", "status"]),
        "off"
    );

    // 3. The empty key table: a root binding doesn't fire, and the key
    //    reaches the pane instead. Plain text too.
    c.send(b"hello");
    c.send(b"\x1b[24~");
    wait_for("the keys in the pane", || {
        let l = read_log(&log);
        l.contains("hello") && l.contains("\x1b[24~")
    });
    assert_eq!(
        tx(&["show-options", "-gqv", "@web_fired"]),
        "",
        "root binding fired"
    );

    // 4. Unbound mouse reaches a mouse-aware pane: tmux turned mouse
    //    reporting on for the client, and a click arrives as SGR.
    wait_for("mouse reporting on the client", || {
        c.drawn().contains("\x1b[?1006h")
    });
    c.send(b"\x1b[<0;5;5M\x1b[<0;5;5m");
    wait_for("the click in the pane", || {
        read_log(&log).contains("\x1b[<0;5;5M")
    });

    // 5. `destroy-unattached`: the view session goes with its client; the
    //    target stays.
    c.close();
    wait_for("the view session to go", || !tmux::has_session("webview-1"));
    assert!(tmux::has_session(target));

    // 6. Killing the target alone leaves its window alive while a view
    //    session links it (so rlord must kill view sessions too).
    let mut c = attach_view(target, "webview-2", 80, 24);
    let windows = || tx(&["list-windows", "-a", "-F", "#{window_id} #{pane_dead}"]);
    // (Plain `kill-session`: `tmux::kill_session` kills the views too.)
    tx_ok(&["kill-session", "-t", &format!("={target}")]).unwrap();
    assert!(!tmux::has_session(target));
    assert!(
        windows().lines().any(|l| l == format!("{target_window} 0")),
        "{}",
        windows()
    );
    assert!(!c.exited());
    c.close();
    wait_for("the window to go with the last session", || {
        !windows().contains(&target_window)
    });

    // 7. `new-session -t` on a missing name doesn't fail: it makes a new
    //    group with a fresh shell. (So check the target exists first.)
    tx_ok(&[
        "new-session",
        "-d",
        "-t",
        "=rlord_nosuch",
        "-s",
        "webview-3",
    ])
    .unwrap();
    assert!(tmux::has_session("webview-3"));
    let made = tx(&[
        "display-message",
        "-p",
        "-t",
        "=webview-3:",
        "#{session_group}|#{session_windows}|#{pane_start_command}",
    ]);
    // A group named after the missing target, one window, the default
    // shell (no start command).
    assert_eq!(made, "=rlord_nosuch|1|", "{made}");
    // And tmux refuses a command next to `-t`.
    tmux::new_session(target, "/tmp", "sleep 30").unwrap();
    let e = tx_ok(&[
        "new-session",
        "-d",
        "-t",
        &format!("={target}"),
        "-s",
        "webview-4",
        "echo",
    ])
    .unwrap_err();
    assert!(
        e.contains("command or window name given with target"),
        "{e}"
    );

    // Control for 3: a plain client on the target (root table) does fire
    // the binding, so the key was understood as F12. (Bound again: the
    // server exited with its last session in 6.)
    tx_ok(&[
        "bind-key",
        "-n",
        "F12",
        "set-option",
        "-g",
        "@web_fired",
        "yes",
    ])
    .unwrap();
    let mut plain = PtyClient::spawn(&["attach-session", "-t", &format!("={target}")], 80, 24);
    wait_for("the plain client", || {
        tx(&["list-clients", "-F", "#{session_name}"]).contains(target)
    });
    plain.send(b"\x1b[24~");
    wait_for("the root binding", || {
        tx(&["show-options", "-gqv", "@web_fired"]) == "yes"
    });
    plain.close();

    tx(&["kill-server"]);
}

/// Step 2: rlord's view-session helpers, and what they change elsewhere
/// (`list_clients`, `view_clients`, `kill_session`, the sweep).
#[test]
fn view_helpers() {
    let Some(_g) = setup() else { return };
    let target = "rlord_webhelp";
    tmux::new_session(target, "/tmp", "cat").unwrap();
    // Refused: a missing target (it would make a shell), a non-view name.
    let e = tmux::view_command("rlord_nosuch", "webview-9", false).unwrap_err();
    assert!(e.to_string().contains("not running"), "{e}");
    assert!(tmux::view_command(target, "rlord_other", false).is_err());

    // The web table: root's pane mouse bindings, no keyboard keys; the
    // same again when remade.
    tmux::bind_web_table().unwrap();
    let keys = tmux::bindings(tmux::WEB_TABLE);
    assert!(keys.iter().any(|(k, _)| k == "MouseDown1Pane"), "{keys:?}");
    assert!(keys.iter().any(|(k, _)| k == "WheelUpPane"), "{keys:?}");
    assert!(keys.iter().all(|(k, _)| k.ends_with("Pane")), "{keys:?}");
    tmux::bind_web_table().unwrap();
    assert_eq!(tmux::bindings(tmux::WEB_TABLE), keys);

    let mut c = PtyClient::run(
        tmux::view_command(target, "webview-1", false).unwrap(),
        80,
        24,
    );
    wait_for("the view's client", || {
        tx(&["list-clients", "-F", "#{session_name}"]) == "webview-1"
    });
    wait_for("the view's options", || {
        tx(&[
            "show-options",
            "-v",
            "-t",
            "=webview-1:",
            "destroy-unattached",
        ]) == "on"
    });
    assert!(tmux::view_shows("webview-1", target));
    assert!(!tmux::view_shows("webview-1", "rlord_nosuch"));
    assert_eq!(
        tx(&["show-options", "-v", "-t", "=webview-1:", "key-table"]),
        tmux::WEB_TABLE
    );
    // The browser is never the user's terminal...
    assert!(tmux::list_clients().is_empty());
    // ...but counts as a recent client of the target once typed in.
    c.send(b"typed in the browser");
    wait_for("the keys in the pane", || {
        tmux::capture_pane(target)
            .unwrap()
            .contains("typed in the browser")
    });
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let v = tmux::view_clients(target);
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].session, target);
    assert!(now_s - v[0].activity <= 2, "{v:?}");
    assert!(tmux::view_clients("rlord_nosuch").is_empty());

    // Killing the target kills its views (found by group, so after a
    // rename too), and the browser's client exits.
    tmux::rename_session(target, "rlord_webhelp2").unwrap();
    assert_eq!(tmux::view_clients("rlord_webhelp2").len(), 1);
    tmux::kill_session("rlord_webhelp2").unwrap();
    assert!(!tmux::has_session("webview-1"));
    wait_for("the view's client to exit", || c.exited());
    c.close();

    // The sweep: a view with no client goes, nothing else does.
    tmux::new_session(target, "/tmp", "sleep 30").unwrap();
    tx_ok(&[
        "new-session",
        "-d",
        "-t",
        &format!("={target}"),
        "-s",
        "webview-7",
    ])
    .unwrap();
    assert_eq!(tmux::sweep_views(), vec!["webview-7".to_string()]);
    assert!(tmux::has_session(target));
    assert!(tmux::sweep_views().is_empty());

    tx(&["kill-server"]);
}

/// Slice 2, step 1: a client's `client_activity` (what `tmux::Client`'s
/// `activity` reads) moves when the client types and not when its pane
/// prints, so "the surface the user used last" can rest on it. Its
/// resolution is one second (epoch seconds).
#[test]
fn client_activity_is_input_not_output() {
    let Some(_g) = setup() else { return };
    let target = "rlord_webactivity";
    // A pane that keeps printing (as an agent or the TUI redrawing does).
    tmux::new_session(target, "/tmp", "while :; do date +%s%N; sleep 0.05; done").unwrap();
    let mut c = PtyClient::spawn(&["attach-session", "-t", &format!("={target}")], 80, 24);
    wait_for("the client", || {
        tx(&["list-clients", "-F", "#{session_name}"]) == target
    });
    let activity = || -> i64 {
        tx(&["list-clients", "-F", "#{client_activity}"])
            .parse()
            .unwrap()
    };
    let drawn = |c: &PtyClient| c.out.lock().unwrap().len();
    let before = activity();
    let bytes = drawn(&c);
    // Output for a few seconds: drawn to the client, activity unchanged.
    std::thread::sleep(Duration::from_millis(3200));
    assert!(drawn(&c) > bytes + 1000, "the pane wasn't printing");
    assert_eq!(activity(), before, "output moved client_activity");
    // A resize (a browser view fitting itself) isn't input either.
    let ws = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // Safety: TIOCSWINSZ on the PTY master this test owns.
    unsafe {
        use std::os::fd::AsRawFd;
        libc::ioctl(c.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws);
    }
    wait_for("the resize", || {
        tx(&["list-clients", "-F", "#{client_width}"]) == "100"
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(activity(), before, "a resize moved client_activity");
    // A key press moves it to now.
    c.send(b"x");
    let now_s = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    };
    wait_for("the key press to count", || activity() > before);
    assert!(now_s() - activity() <= 1, "{} vs {}", activity(), now_s());
    c.close();
    tx(&["kill-server"]);
}

/// A daemon on the scratch tmux server, serving on a temp socket, with
/// session `s1` running in tmux session `rlord_s1`.
fn daemon(cfg: rlord_core::config::Config) -> (Arc<rlord_daemon::Daemon>, tempfile::TempDir) {
    use rlord_core::db::{Db, NewSession};
    let db = Db::open_in_memory().unwrap();
    db.insert_session(&NewSession {
        id: "s1",
        name: "s1",
        agent: "claude",
        agent_session: Some("s1"),
        project_path: "/tmp",
        tmux_session: Some("rlord_s1"),
        launch_spec: "{}",
        created_at: 1,
        repo_path: None,
        workspace_path: None,
        branch: Some("main"),
        role: "",
    })
    .unwrap();
    let d = rlord_daemon::Daemon::new(db, cfg);
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("d.sock");
    let (tx, rx) = std::sync::mpsc::channel();
    let d2 = d.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let listener = rlord_daemon::Daemon::bind(&sock).unwrap();
            tx.send(()).unwrap();
            let _ = d2.serve(listener).await;
        });
    });
    rx.recv().unwrap();
    (d, dir)
}

/// One request to the daemon at `dir`; the whole response.
fn call(dir: &tempfile::TempDir, method: &str, params: serde_json::Value) -> serde_json::Value {
    use std::io::BufRead;
    let mut s = std::os::unix::net::UnixStream::connect(dir.path().join("d.sock")).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    writeln!(
        s,
        "{}",
        serde_json::json!({"id": 1, "method": method, "params": params})
    )
    .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(s).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// The overlord's tool call; its result (panics on an error).
fn tool(dir: &tempfile::TempDir, name: &str, args: serde_json::Value) -> serde_json::Value {
    let r = call(
        dir,
        "mcp.call",
        serde_json::json!({"name": name, "arguments": args, "overlord": true}),
    );
    assert!(r["error"].is_null(), "{name}: {r}");
    r["result"].clone()
}

/// The plugin's stream: the next UI line.
fn next_ui(
    lines: &mut std::io::Lines<std::io::BufReader<std::os::unix::net::UnixStream>>,
) -> serde_json::Value {
    loop {
        let l = lines.next().expect("open").expect("a line in time");
        let v: serde_json::Value = serde_json::from_str(&l).unwrap();
        if v["notify"] == "ui" {
            return v;
        }
    }
}

/// Slice 2: a terminal and a web page both present. The one used later
/// gets `goto_session`: the terminal is switched, or the page is sent
/// `goto` and the terminal stays where it was; the other gets a notice.
/// `[web] ui_target` overrides.
#[test]
fn the_surface_used_last_gets_the_command() {
    use serde_json::json;
    use std::io::BufRead;
    let Some(_g) = setup() else { return };
    tmux::new_session("rlord_s1", "/tmp", "cat").unwrap();
    tmux::new_session("rlord_home", "/tmp", "cat").unwrap();
    // (The list's session: a stand-in for the TUI.)
    tmux::set_home("rlord_home").unwrap();
    let mut term = PtyClient::spawn(&["attach-session", "-t", "=rlord_home"], 80, 24);
    let client_on = || tx(&["list-clients", "-F", "#{session_name}"]);
    wait_for("the terminal", || client_on() == "rlord_home");
    let (_d, dir) = daemon(Default::default());
    let s = std::os::unix::net::UnixStream::connect(dir.path().join("d.sock")).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut w = s.try_clone().unwrap();
    writeln!(w, "{}", json!({"id": 1, "method": "web.subscribe"})).unwrap();
    let mut lines = std::io::BufReader::new(s).lines();
    lines.next().unwrap().unwrap();
    assert!(call(&dir, "web.pages", json!({"pages": ["p1-aa"]}))["error"].is_null());
    let page_input = || {
        let r = call(
            &dir,
            "web.call",
            json!({"op": "page.report", "via": "local", "page": "p1-aa", "args": {"input": true}}),
        );
        assert!(r["error"].is_null(), "{r}");
    };

    // The terminal typed last: it is switched; the page gets a notice.
    term.send(b"x");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(tool(&dir, "get_ui_state", json!({}))["target"], "terminal");
    let g = tool(&dir, "goto_session", json!({"session": "s1"}));
    assert_eq!(g["on"], "terminal", "{g}");
    wait_for("the terminal on s1", || client_on() == "rlord_s1");
    let n = next_ui(&mut lines);
    assert_eq!(n["cmd"]["cmd"], "notice", "{n}");
    assert!(
        n["cmd"]["text"]
            .as_str()
            .unwrap()
            .contains("in the terminal"),
        "{n}"
    );

    // The page used later (a second on: whole seconds): it gets `goto`,
    // and the terminal stays where it is.
    tx_ok(&[
        "switch-client",
        "-c",
        &tx(&["list-clients", "-F", "#{client_name}"]),
        "-t",
        "=rlord_home",
    ])
    .unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    page_input();
    assert_eq!(tool(&dir, "get_ui_state", json!({}))["target"], "web");
    let g = tool(&dir, "goto_session", json!({"session": "s1"}));
    assert_eq!(g["on"], "web", "{g}");
    let c = next_ui(&mut lines);
    assert_eq!(
        c["cmd"],
        json!({"cmd": "goto", "id": "s1", "overlord": false})
    );
    assert_eq!(c["page"], "p1-aa");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(client_on(), "rlord_home");

    // The terminal again.
    std::thread::sleep(Duration::from_millis(1100));
    term.send(b"y");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        tool(&dir, "goto_session", json!({"session": "s1"}))["on"],
        "terminal"
    );
    wait_for("the terminal on s1", || client_on() == "rlord_s1");
    drop(lines);
    drop(w);

    // ui_target = "web": the page, though the terminal typed last.
    let mut cfg = rlord_core::config::Config::default();
    cfg.web.ui_target = rlord_core::config::UiTarget::Web;
    let (_d2, dir2) = daemon(cfg);
    assert!(call(&dir2, "web.pages", json!({"pages": ["p2-bb"]}))["error"].is_null());
    term.send(b"z");
    std::thread::sleep(Duration::from_millis(300));
    let st = tool(&dir2, "get_ui_state", json!({}));
    assert_eq!(st["target"], "web", "{st}");
    assert_eq!(tool(&dir2, "show_list", json!({}))["on"], "web");
    assert_eq!(client_on(), "rlord_s1");
    // The key screen still goes to the terminal (keys change only there).
    let k = tool(&dir2, "open_keys", json!({}));
    assert_eq!(k["on"], "terminal", "{k}");
    wait_for("the terminal on the list", || client_on() == "rlord_home");

    term.close();
    tx(&["kill-server"]);
}

// ---- slice 5: which screen controls a pane (daemon/src/control.rs) ----

/// The size of `session`'s window, as tmux has it.
fn window_of(session: &str) -> (u16, u16) {
    tmux::window_size(session).expect("a window")
}

/// A session option set on `session` itself (empty: not set there).
fn local_opt(session: &str, opt: &str) -> String {
    tx(&["show-options", "-qv", "-t", &format!("={session}:"), opt])
}

/// `call`'s result; panics on an error.
fn ok_call(dir: &tempfile::TempDir, method: &str, params: serde_json::Value) -> serde_json::Value {
    let r = call(dir, method, params);
    assert!(r["error"].is_null(), "{method}: {r}");
    r["result"].clone()
}

/// A local page's op (the plugin stamps its id).
fn page_op(
    dir: &tempfile::TempDir,
    page: &str,
    op: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    ok_call(
        dir,
        "web.call",
        serde_json::json!({"op": op, "args": args, "via": "local", "page": page}),
    )
}

/// A page's pane socket opening `rlord_s1` at its size.
fn pane_open(dir: &tempfile::TempDir, page: &str, cols: u16, rows: u16) -> serde_json::Value {
    ok_call(
        dir,
        "web.pane_open",
        serde_json::json!({"target": "s1", "surface": format!("page:{page}"),
                           "cols": cols, "rows": rows}),
    )
}

/// A page holding a pane pins its window to the page's size; view clients
/// (attached `ignore-size`) never size it, whatever their size, and a
/// terminal coming to it doesn't either. The session's keys are held.
#[test]
fn a_held_window_is_pinned_and_views_never_size_it() {
    use serde_json::json;
    let Some(_g) = setup() else { return };
    tmux::new_session("rlord_s1", "/tmp", "cat").unwrap();
    let (_d, dir) = daemon(Default::default());
    let o = pane_open(&dir, "p1", 54, 20);
    assert_eq!(o["running"], true, "{o}");
    assert_eq!(o["control"], true, "a free pane is its opener's: {o}");
    assert_eq!(o["window"], json!({"cols": 54, "rows": 20}), "{o}");
    assert_eq!(window_of("rlord_s1"), (54, 20));
    assert_eq!(local_opt("rlord_s1", "key-table"), tmux::HELD_TABLE);
    assert_eq!(local_opt("rlord_s1", "assume-paste-time"), "0");
    // The holder's view, then another page's at another size: a viewer,
    // told the window's size; neither client sizes the window.
    let v1 = PtyClient::run(
        tmux::view_command("rlord_s1", "webview-1", false).unwrap(),
        54,
        20,
    );
    let o2 = pane_open(&dir, "p2", 120, 50);
    assert_eq!(o2["control"], false, "{o2}");
    assert_eq!(
        (o2["cols"].clone(), o2["rows"].clone()),
        (json!(54), json!(20))
    );
    let v2 = PtyClient::run(
        tmux::view_command("rlord_s1", "webview-2", true).unwrap(),
        120,
        50,
    );
    wait_for("both views", || {
        tx(&["list-clients", "-F", "#{session_name}"])
            .lines()
            .count()
            == 2
    });
    // The view-only client is read-only.
    assert!(tx(&["list-clients", "-F", "#{client_flags}"]).contains("read-only"));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(window_of("rlord_s1"), (54, 20));
    // A terminal comes: still the page's size (it views, padded).
    let term = PtyClient::spawn(&["attach-session", "-t", "=rlord_s1"], 100, 30);
    wait_for("the terminal", || tmux::list_clients().len() == 1);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(window_of("rlord_s1"), (54, 20));
    // The holder's terminal changes size: the window follows; a viewer's
    // doesn't move it.
    let s = ok_call(
        &dir,
        "web.pane_size",
        json!({"target": "s1", "surface": "page:p1", "cols": 60, "rows": 22}),
    );
    assert_eq!(s, json!({"control": true}));
    assert_eq!(window_of("rlord_s1"), (60, 22));
    let s = ok_call(
        &dir,
        "web.pane_size",
        json!({"target": "s1", "surface": "page:p2", "cols": 30, "rows": 10}),
    );
    assert_eq!(s, json!({"control": false}));
    assert_eq!(window_of("rlord_s1"), (60, 22));
    // Everyone sees who holds it.
    let c = ok_call(&dir, "control.get", json!({"target": "s1"}));
    assert_eq!(c["control"]["holder"], "page:p1", "{c}");
    assert_eq!(c["control"]["cols"], 60, "{c}");
    for c in [v1, v2, term] {
        c.close();
    }
    tx(&["kill-server"]);
}

/// The terminal holding a pane unpins its window (it follows the
/// terminal's client again) and gets its keys back: the key table the
/// session had (here a profile's) is restored.
#[test]
fn the_terminal_holding_unpins() {
    use serde_json::json;
    let Some(_g) = setup() else { return };
    tmux::new_session("rlord_s1", "/tmp", "cat").unwrap();
    // (As a session with the user's profile has.)
    tx_ok(&["set-option", "-t", "=rlord_s1:", "key-table", "mytable"]).unwrap();
    let term = PtyClient::spawn(&["attach-session", "-t", "=rlord_s1"], 100, 30);
    wait_for("the terminal", || tmux::list_clients().len() == 1);
    let own = window_of("rlord_s1");
    assert_eq!(own.0, 100);
    let (_d, dir) = daemon(Default::default());
    // Opened beside the terminal: the terminal's, the page a viewer.
    let o = pane_open(&dir, "p1", 54, 20);
    assert_eq!(o["control"], false, "{o}");
    assert_eq!(window_of("rlord_s1"), own);
    assert_eq!(local_opt("rlord_s1", "key-table"), "mytable");
    let c = ok_call(&dir, "control.get", json!({"target": "s1"}));
    assert_eq!(c["control"]["holder"], "terminal", "{c}");
    assert_eq!(c["control"]["by"], "terminal", "{c}");
    // The page takes over: pinned to it, the terminal's keys held.
    let t = page_op(
        &dir,
        "p1",
        "pane.take",
        json!({"target": "s1", "cols": 54, "rows": 20}),
    );
    assert_eq!(t, json!({"control": true, "cols": 54, "rows": 20}));
    assert_eq!(window_of("rlord_s1"), (54, 20));
    assert_eq!(local_opt("rlord_s1", "key-table"), tmux::HELD_TABLE);
    // Twice is once.
    page_op(
        &dir,
        "p1",
        "pane.take",
        json!({"target": "s1", "cols": 54, "rows": 20}),
    );
    // The terminal takes it back (`rlord control take`).
    let t = ok_call(&dir, "control.take", json!({"target": "rlord_s1"}));
    assert_eq!(t["control"], true, "{t}");
    wait_for("the window to follow the terminal", || {
        window_of("rlord_s1") == own
    });
    assert_eq!(local_opt("rlord_s1", "key-table"), "mytable");
    assert_eq!(local_opt("rlord_s1", "assume-paste-time"), "");
    assert!(tmux::held_sessions().is_empty());
    term.close();
    tx(&["kill-server"]);
}

/// While a page holds a session, what the terminal types reaches no pane
/// (a fast burst and C-c included: `assume-paste-time 0`); a key press
/// says who controls it. Once the terminal takes it back, its keys reach
/// the pane again.
#[test]
fn a_held_sessions_keys_reach_no_pane() {
    use serde_json::json;
    let Some(_g) = setup() else { return };
    let dir_log = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("web-tmux");
    let log = dir_log.join("held.log");
    let _ = std::fs::remove_file(&log);
    tmux::new_session("rlord_s1", "/tmp", &probe_cmd(&log)).unwrap();
    wait_for("the probe", || {
        tmux::capture_pane("rlord_s1")
            .unwrap()
            .contains("PROBE-READY")
    });
    let mut term = PtyClient::spawn(&["attach-session", "-t", "=rlord_s1"], 100, 30);
    wait_for("the terminal", || tmux::list_clients().len() == 1);
    let (_d, dir) = daemon(Default::default());
    pane_open(&dir, "p1", 54, 20);
    page_op(
        &dir,
        "p1",
        "pane.take",
        json!({"target": "s1", "cols": 54, "rows": 20}),
    );
    term.send(b"typed while held\r");
    term.send(b"\x03");
    term.send(b"x");
    wait_for("the held line", || {
        term.drawn().contains("Controlled by a browser on")
    });
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        read_log(&log),
        "",
        "a held terminal's keys reached the pane"
    );
    ok_call(&dir, "control.take", json!({"target": "s1"}));
    term.send(b"after");
    wait_for("the terminal's keys again", || {
        read_log(&log).contains("after")
    });
    assert!(!read_log(&log).contains("typed while held"));
    term.close();
    tx(&["kill-server"]);
}

/// The take key, bound in a held session's table beside rlord's
/// navigation keys, runs `rlord control take <its tmux session>`.
#[test]
fn the_take_key_takes() {
    use serde_json::json;
    let Some(_g) = setup() else { return };
    let dir_out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("web-tmux");
    let out = dir_out.join("take.out");
    let _ = std::fs::remove_file(&out);
    // A stand-in for `rlord` (the test binary isn't one): it writes what
    // it was asked.
    let bin = dir_out.join("rlord-standin.sh");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\necho \"$@\" > {}\n",
            tmux::shell_quote(&out.to_string_lossy())
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Safety: this test process only; the other tests don't read it.
    std::env::set_var("RLORD_TEST_HELD_BIN", &bin);
    tmux::new_session("rlord_s1", "/tmp", "cat").unwrap();
    let mut term = PtyClient::spawn(&["attach-session", "-t", "=rlord_s1"], 100, 30);
    wait_for("the terminal", || tmux::list_clients().len() == 1);
    let (_d, dir) = daemon(Default::default());
    pane_open(&dir, "p1", 54, 20);
    page_op(
        &dir,
        "p1",
        "pane.take",
        json!({"target": "s1", "cols": 54, "rows": 20}),
    );
    // rlord's keys in the held table: navigation, the take key, `Any`.
    let keys = tmux::bindings(tmux::HELD_TABLE);
    let bound = |k: &str| {
        keys.iter()
            .find(|(key, _)| key == k)
            .map(|(_, c)| c.clone())
    };
    assert!(
        bound("M-d").is_some_and(|c| c.contains("nav home")),
        "{keys:?}"
    );
    assert!(
        bound("M-o").is_some_and(|c| c.contains("nav overlord")),
        "{keys:?}"
    );
    assert!(
        bound("M-t").is_some_and(|c| c.contains("control take")),
        "{keys:?}"
    );
    assert!(
        bound("Any").is_some_and(|c| c.contains("display-message")),
        "{keys:?}"
    );
    // Option-T in the terminal.
    term.send(b"\x1bt");
    wait_for("the take command", || {
        std::fs::read_to_string(&out).is_ok_and(|s| s.trim() == "control take rlord_s1")
    });
    std::env::remove_var("RLORD_TEST_HELD_BIN");
    term.close();
    tx(&["kill-server"]);
}
