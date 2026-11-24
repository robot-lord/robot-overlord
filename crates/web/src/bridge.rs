//! A view's tmux client on a PTY: what shows a pane in a browser (the
//! local pane socket, `pane.rs`) and on a paired device (`remote/panes.rs`).
//! The client attaches to a throwaway view session grouped with the target
//! (`rlord_core::tmux::view_command`), with `ignore-size` (a view never
//! sizes a window: the daemon pins it for whoever controls the pane), and
//! `read-only` for a view-only device.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::unix::AsyncFd;

use rlord_core::tmux;

/// How long the view session has to show the target once attached.
const VIEW_WAIT: Duration = Duration::from_secs(3);

/// A terminal size from the page, within reason.
pub(crate) fn size(v: &Value) -> Option<(u16, u16)> {
    let n = |k: &str| {
        v[k].as_u64()
            .filter(|n| (2..=1000).contains(n))
            .map(|n| n as u16)
    };
    Some((n("cols")?, n("rows")?))
}

/// A fresh view session name.
pub(crate) fn view_name() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}{}-{}",
        tmux::VIEW_PREFIX,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// A view's tmux client, running on a PTY.
pub(crate) struct Bridge {
    pub(crate) pty: Pty,
    child: std::process::Child,
    view: String,
}

impl Bridge {
    /// Make the web key table (root's mouse bindings now), start the
    /// view's tmux client on a PTY, and confirm the view shows `target`.
    /// Blocking (tmux).
    pub(crate) fn attach(
        target: &str,
        view: &str,
        cols: u16,
        rows: u16,
        read_only: bool,
    ) -> Result<Bridge> {
        tmux::bind_web_table().context("making the web key table")?;
        let cmd = tmux::view_command(target, view, read_only)?;
        let (pty, child) = Pty::spawn(cmd, cols, rows)?;
        let mut b = Bridge {
            pty,
            child,
            view: view.to_string(),
        };
        let start = Instant::now();
        while !tmux::view_shows(view, target) {
            if start.elapsed() > VIEW_WAIT || b.child.try_wait().ok().flatten().is_some() {
                b.end();
                bail!("{target} went away");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(b)
    }

    /// Stop the client and make sure its view session is gone. Blocking.
    pub(crate) fn end(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            signal(self.child.id(), libc::SIGTERM);
            let start = Instant::now();
            while self.child.try_wait().ok().flatten().is_none() {
                if start.elapsed() > Duration::from_secs(1) {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        // (`destroy-unattached` should have done it.)
        if tmux::has_session(&self.view) {
            let _ = tmux::command()
                .args(["kill-session", "-t", &format!("={}", self.view)])
                .output();
        }
    }
}

fn signal(pid: u32, sig: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // Safety: kill(2) on our own unreaped child.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// The master side of a PTY, non-blocking, in tokio.
pub(crate) struct Pty {
    fd: AsyncFd<OwnedFd>,
}

impl Pty {
    /// Run `cmd` with a fresh PTY of `cols`×`rows` as its controlling
    /// terminal.
    pub(crate) fn spawn(
        mut cmd: std::process::Command,
        cols: u16,
        rows: u16,
    ) -> Result<(Pty, std::process::Child)> {
        use std::os::unix::process::CommandExt;
        let (mut m, mut s) = (0, 0);
        let mut ws = winsize(cols, rows);
        // Safety: plain libc calls on fds this function owns.
        unsafe {
            if libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                // A raw pointer: Linux's libc takes `*const winsize`,
                // macOS's `*mut` (a `&mut` trips clippy on Linux).
                &raw mut ws,
            ) != 0
            {
                bail!("openpty: {}", std::io::Error::last_os_error());
            }
            libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(s, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        // Safety: freshly opened, owned here.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        cmd.stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave.try_clone()?);
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
        let child = cmd.spawn().context("starting the view's tmux client")?;
        drop(cmd);
        drop(slave);
        // Safety: fcntl on the fd owned above.
        unsafe {
            let fl = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        // Safety: an OwnedFd keeps its descriptor open, and the same one,
        // for as long as the AsyncFd owns it.
        let fd = unsafe { AsyncFd::register(master) }.map_err(|e| e.into_parts().1)?;
        Ok((Pty { fd }, child))
    }

    pub(crate) async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            let r = guard.try_io(|fd| {
                // Safety: read(2) into a buffer we own.
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match r {
                Ok(r) => return r,
                Err(_would_block) => continue,
            }
        }
    }

    pub(crate) async fn write_all(&self, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let mut guard = self.fd.writable().await?;
            let r = guard.try_io(|fd| {
                // Safety: write(2) from a buffer we own.
                let n = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match r {
                Ok(Ok(n)) => data = &data[n..],
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// The terminal's new size; its client gets SIGWINCH.
    pub(crate) fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        let ws = winsize(cols, rows);
        // Safety: TIOCSWINSZ on our own PTY master.
        if unsafe { libc::ioctl(self.fd.get_ref().as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn sizes_are_bounded() {
        assert_eq!(size(&json!({"cols": 80, "rows": 24})), Some((80, 24)));
        assert_eq!(size(&json!({"cols": 1000, "rows": 2})), Some((1000, 2)));
        for bad in [
            json!({"cols": 0, "rows": 24}),
            json!({"cols": 80, "rows": 1}),
            json!({"cols": 1001, "rows": 24}),
            json!({"cols": "80", "rows": 24}),
            json!({"cols": 80}),
            json!({"cols": -1, "rows": 24}),
        ] {
            assert_eq!(size(&bad), None, "{bad}");
        }
    }

    #[test]
    fn view_names_are_views_and_unique() {
        let (a, b) = (view_name(), view_name());
        assert!(tmux::is_view(&a) && tmux::is_view(&b));
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn a_pty_carries_bytes_and_its_size() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "stty size; read line; echo got:$line"]);
        let (pty, mut child) = Pty::spawn(cmd, 91, 27).unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        while !String::from_utf8_lossy(&out).contains("27 91") {
            let n = pty.read(&mut buf).await.unwrap();
            out.extend_from_slice(&buf[..n]);
        }
        pty.write_all(b"hi\n").await.unwrap();
        while !String::from_utf8_lossy(&out).contains("got:hi") {
            match pty.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
        assert!(String::from_utf8_lossy(&out).contains("got:hi"), "{out:?}");
        pty.resize(100, 30).unwrap();
        let _ = child.wait();
    }
}
