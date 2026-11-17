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
    let e = tmux::view_command("rlord_nosuch", "webview-9").unwrap_err();
    assert!(e.to_string().contains("not running"), "{e}");
    assert!(tmux::view_command(target, "rlord_other").is_err());

    // The web table: root's pane mouse bindings, no keyboard keys; the
    // same again when remade.
    tmux::bind_web_table().unwrap();
    let keys = tmux::bindings(tmux::WEB_TABLE);
    assert!(keys.iter().any(|(k, _)| k == "MouseDown1Pane"), "{keys:?}");
    assert!(keys.iter().any(|(k, _)| k == "WheelUpPane"), "{keys:?}");
    assert!(keys.iter().all(|(k, _)| k.ends_with("Pane")), "{keys:?}");
    tmux::bind_web_table().unwrap();
    assert_eq!(tmux::bindings(tmux::WEB_TABLE), keys);

    let mut c = PtyClient::run(tmux::view_command(target, "webview-1").unwrap(), 80, 24);
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
