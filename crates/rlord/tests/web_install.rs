//! `rlord web install` and `remove` with `--from` a folder of release files
//! made here: never the network (PATH holds only `tar` and what it needs,
//! so no `curl` or `gh` could run), never a daemon (none is started for
//! these). A checkout's `rlord` has no compiled-in plugin hash; its debug
//! build takes the expected one from `RLORD_TEST_WEB_SHA256`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use rlord_core::plugin;

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Env {
    dir: tempfile::TempDir,
    bin: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        for tool in ["tar", "gzip", "bash"] {
            if let Some(p) = which(tool) {
                std::os::unix::fs::symlink(p, bin.join(tool)).unwrap();
            }
        }
        Env { dir, bin }
    }

    fn data(&self) -> PathBuf {
        self.dir.path().join("data/robot-overlord")
    }

    fn installed(&self) -> PathBuf {
        self.data().join("plugins/rlord-web")
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("config/robot-overlord/config.toml")
    }

    /// `rlord <args>`, no terminal, expecting plugin hash `hash`.
    fn rlord(&self, hash: Option<&str>, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_rlord"));
        c.args(args)
            .env_clear()
            .env("PATH", &self.bin)
            .env("HOME", self.dir.path())
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_RUNTIME_DIR", self.dir.path().join("run"))
            .env("RLORD_TMUX_SOCKET", "rlord-test-web-install")
            // Never the `rlord-web` built beside this rlord: a build with
            // no hash would run it (and start a daemon to run it under).
            .env("RLORD_WEB_BIN", self.dir.path().join("no-plugin"))
            .stdin(Stdio::null());
        if let Some(h) = hash {
            c.env("RLORD_TEST_WEB_SHA256", h);
        }
        c.output().unwrap()
    }

    /// A release folder: the plugin tarball (holding `contents` as the
    /// plugin) and SHA256SUMS. Returns the folder and the plugin's hash.
    fn release(&self, contents: &str) -> (PathBuf, String) {
        let target = plugin::release_target().expect("a release platform");
        let stem = plugin::tarball_stem(VERSION, target);
        let stage = self.dir.path().join("stage");
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(stage.join(&stem)).unwrap();
        let exe = stage.join(&stem).join("rlord-web");
        std::fs::write(&exe, contents).unwrap();
        let hash = plugin::sha256_file(&exe).unwrap();
        let out = self.dir.path().join("release");
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir(&out).unwrap();
        let tarball = format!("{stem}.tar.gz");
        let st = Command::new("tar")
            .env("COPYFILE_DISABLE", "1")
            .arg("-czf")
            .arg(out.join(&tarball))
            .arg("-C")
            .arg(&stage)
            .arg(&stem)
            .status()
            .unwrap();
        assert!(st.success());
        let sum = plugin::sha256_file(&out.join(&tarball)).unwrap();
        std::fs::write(out.join("SHA256SUMS"), format!("{sum}  {tarball}\n")).unwrap();
        (out, hash)
    }
}

fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(tool))
            .find(|f| f.is_file())
    })
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|r| {
            r.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn installs_a_matching_plugin_and_refuses_another() {
    let e = Env::new();
    let (rel, hash) = e.release("#!/bin/sh\necho plugin\n");
    let from = rel.to_str().unwrap();
    // No terminal and no --yes: refused, naming --yes.
    let o = e.rlord(Some(&hash), &["web", "install", "--from", from]);
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("rlord web install --yes"), "{}", text(&o));
    assert!(!e.installed().exists());
    // Another hash expected: nothing installed, nothing left behind.
    let o = e.rlord(
        Some(&"ab".repeat(32)),
        &["web", "install", "--yes", "--from", from],
    );
    assert!(!o.status.success(), "{}", text(&o));
    assert!(
        text(&o).contains("not the one this rlord was built with"),
        "{}",
        text(&o)
    );
    assert!(!e.installed().exists());
    assert_eq!(leftovers(&e.data().join("plugins")), Vec::<String>::new());
    // The matching one: installed, 0755 in a 0700 folder.
    let o = e.rlord(Some(&hash), &["web", "install", "--yes", "--from", from]);
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(plugin::sha256_file(&e.installed()).unwrap(), hash);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&e.installed()), 0o755);
    assert_eq!(mode(&e.data().join("plugins")), 0o700);
    assert_eq!(leftovers(&e.data().join("plugins")), Vec::<String>::new());
    // Again: already there.
    let o = e.rlord(Some(&hash), &["web", "install", "--yes", "--from", from]);
    assert!(text(&o).contains("already installed"), "{}", text(&o));
}

#[test]
fn a_tarball_that_doesnt_match_its_sums_is_refused() {
    let e = Env::new();
    let (rel, hash) = e.release("#!/bin/sh\necho plugin\n");
    let sums = rel.join("SHA256SUMS");
    let line = std::fs::read_to_string(&sums).unwrap();
    std::fs::write(&sums, format!("{}{}", "0".repeat(64), &line[64..])).unwrap();
    let o = e.rlord(
        Some(&hash),
        &["web", "install", "--yes", "--from", rel.to_str().unwrap()],
    );
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("checksum mismatch"), "{}", text(&o));
    assert!(!e.installed().exists());
}

#[test]
fn if_installed_does_nothing_without_a_plugin_and_updates_one() {
    let e = Env::new();
    let (rel, hash) = e.release("#!/bin/sh\necho new\n");
    let from = rel.to_str().unwrap();
    let o = e.rlord(
        Some(&hash),
        &["web", "install", "--if-installed", "--from", from],
    );
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(text(&o), "");
    assert!(!e.installed().exists());
    // An old plugin (another version's): replaced, without asking.
    std::fs::create_dir_all(e.installed().parent().unwrap()).unwrap();
    std::fs::write(e.installed(), "old").unwrap();
    let o = e.rlord(
        Some(&hash),
        &["web", "install", "--if-installed", "--from", from],
    );
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(plugin::sha256_file(&e.installed()).unwrap(), hash);
    // A build with no hash has nothing to update.
    let o = e.rlord(None, &["web", "install", "--if-installed", "--from", from]);
    assert!(o.status.success(), "{}", text(&o));
}

#[test]
fn remove_cleans_up() {
    let e = Env::new();
    let (rel, hash) = e.release("#!/bin/sh\n");
    let o = e.rlord(
        Some(&hash),
        &["web", "install", "--yes", "--from", rel.to_str().unwrap()],
    );
    assert!(o.status.success(), "{}", text(&o));
    std::fs::write(e.data().join("web.token"), "t").unwrap();
    std::fs::create_dir_all(e.config().parent().unwrap()).unwrap();
    std::fs::write(e.config(), "# mine\n[web]\nenabled = true\nport = 47900\n").unwrap();
    let o = e.rlord(Some(&hash), &["web", "remove"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(!e.installed().exists());
    assert!(!e.data().join("web.token").exists());
    assert_eq!(
        std::fs::read_to_string(e.config()).unwrap(),
        "# mine\n[web]\nenabled = false\nport = 47900\n"
    );
    // No daemon was started for it.
    assert!(!e.dir.path().join("run/robot-overlord/daemon.sock").exists());
}

#[test]
fn rlord_web_without_a_plugin_or_a_terminal_names_install_yes() {
    let e = Env::new();
    let o = e.rlord(Some(&"ab".repeat(32)), &["web"]);
    assert!(!o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("rlord web install --yes"), "{}", text(&o));
    // Asked before any daemon: none was started.
    assert!(!e.dir.path().join("run/robot-overlord/daemon.sock").exists());
    // A build with no hash says where a checkout's plugin comes from.
    let o = e.rlord(None, &["web"]);
    assert!(!o.status.success(), "{}", text(&o));
    assert!(
        text(&o).contains("cargo build -p robot-overlord-web"),
        "{}",
        text(&o)
    );
}
