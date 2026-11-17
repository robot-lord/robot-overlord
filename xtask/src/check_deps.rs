//! `cargo xtask check-deps`: keep network code out of the `rlord` binary.
//!
//! The web UI is an opt-in plugin (`rlord-web`, `crates/web`), so a user who
//! never opts in has nothing that can send a session over the network
//! (docs/working/web-ui-remote.md, "Crate layout and keeping the main binary
//! clean"). This fails when `rlord`'s dependency tree (normal and build,
//! every target) names an HTTP, websocket, TLS or network-client crate, or
//! the plugin itself, or when the four main crates' sources use a TCP or UDP
//! socket type. Run by CI's `check` job and by `build-release`.
//!
//! tokio's `net` feature is on in `rlord` for Unix sockets and compiles
//! tokio's TCP types too (tokio has no Unix-only feature): they stay unused,
//! which the source check enforces.

use std::path::Path;

use anyhow::{bail, Result};

use crate::cmd::{self, cargo};
use crate::version;

/// Crates `rlord` must never depend on.
pub const DENIED: &[&str] = &[
    "axum",
    "hyper",
    "h2",
    "http",
    "tungstenite",
    "tokio-tungstenite",
    "rustls",
    "native-tls",
    "reqwest",
    "ureq",
    "curl",
    "robot-overlord-web",
];

/// Socket types the main crates' sources must not name.
const DENIED_TYPES: &[&str] = &["TcpListener", "TcpStream", "UdpSocket"];

/// Files allowed a socket type, and why. Keep this list short: each entry
/// is network code in the binary of a user who never opted in.
const ALLOWED_FILES: &[(&str, &str)] = &[(
    "crates/core/src/agents/opencode_service.rs",
    "the OpenCode adapter's client for OpenCode's own background server, \
     at the local address OpenCode's service.json names (predates the web UI)",
)];

/// The crates whose sources make up `rlord`.
const MAIN_CRATES: &[&str] = &["crates/core", "crates/daemon", "crates/tui", "crates/rlord"];

pub fn main(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return cmd::usage("usage: cargo xtask check-deps");
    }
    check()
}

/// Both checks; an error lists everything found.
pub fn check() -> Result<()> {
    let tree = cmd::output(cargo().args([
        "tree",
        "--locked",
        "-p",
        version::PACKAGE,
        "-e",
        "normal,build",
        "--target",
        "all",
        "--prefix",
        "none",
        "--format",
        "{p}",
    ]))?;
    let mut found: Vec<String> = denied_in_tree(&tree)
        .into_iter()
        .map(|c| format!("rlord depends on {c}"))
        .collect();
    for dir in MAIN_CRATES {
        found.extend(denied_in_sources(&Path::new(dir).join("src"))?);
    }
    if !found.is_empty() {
        bail!(
            "network code in the rlord binary (it belongs in the rlord-web plugin):\n  {}",
            found.join("\n  ")
        );
    }
    println!("check-deps: rlord has no network dependency");
    Ok(())
}

/// Denied crate names in `cargo tree --prefix none --format {p}` output
/// (`name vX.Y.Z (path)`, one per line, repeats marked `(*)`). Sorted, once
/// each.
pub fn denied_in_tree(tree: &str) -> Vec<String> {
    let mut v: Vec<String> = tree
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| DENIED.contains(name))
        .map(str::to_string)
        .collect();
    v.sort();
    v.dedup();
    v
}

/// `path:line: Type` for each denied socket type in the `.rs` files under
/// `dir`.
fn denied_in_sources(dir: &Path) -> Result<Vec<String>> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") && !allowed(&p) {
                let text = std::fs::read_to_string(&p)?;
                for (i, t) in denied_in_source(&text) {
                    found.push(format!("{}:{}: {t}", p.display(), i + 1));
                }
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Whether `path` is on `ALLOWED_FILES` (compared by its ending, so the
/// root it was reached from doesn't matter).
fn allowed(path: &Path) -> bool {
    ALLOWED_FILES.iter().any(|(f, _)| path.ends_with(f))
}

/// (line index, type) for each denied socket type named in `text`.
fn denied_in_source(text: &str) -> Vec<(usize, &'static str)> {
    let mut v = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for t in DENIED_TYPES {
            if line.contains(t) {
                v.push((i, *t));
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_planted_network_crate_is_caught() {
        let clean = "robot-overlord v0.23.0 (/x/crates/rlord)\n\
                     tokio v1.48.0\n\
                     httparse v1.10.1\n\
                     http-body v1.0.1\n";
        assert!(denied_in_tree(clean).is_empty());
        let planted = format!("{clean}axum v0.8.9\nhyper v1.12.0\naxum v0.8.9 (*)\n");
        assert_eq!(denied_in_tree(&planted), ["axum", "hyper"]);
        let plugin = "robot-overlord-web v0.23.0 (/x/crates/web)\n";
        assert_eq!(denied_in_tree(plugin), ["robot-overlord-web"]);
    }

    #[test]
    fn a_tcp_socket_in_the_source_is_caught() {
        let src = "use tokio::net::UnixStream;\nlet l = std::net::TcpListener::bind(a);\n";
        assert_eq!(denied_in_source(src), [(1, "TcpListener")]);
        assert!(denied_in_source("use std::os::unix::net::UnixListener;").is_empty());
        assert!(allowed(Path::new(
            "./crates/core/src/agents/opencode_service.rs"
        )));
        assert!(!allowed(Path::new("crates/core/src/agents/opencode.rs")));
    }

    #[test]
    fn the_main_crates_are_clean_today() {
        // Run from the repository root by `cargo test` in xtask's folder.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        for dir in MAIN_CRATES {
            let found = denied_in_sources(&root.join(dir).join("src")).unwrap();
            assert!(found.is_empty(), "{found:?}");
        }
    }
}
