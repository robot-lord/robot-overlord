//! `cargo xtask check-deps`: keep network code out of the `rlord` binary,
//! content crypto out of the relay, and IO out of the crypto core.
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
//!
//! The remote-access crates (docs/working/web-ui-remote.md, "Slice 3 plan",
//! `check-deps`) add three tree checks and two source checks:
//!
//! 1. `rlord` also names none of the end-to-end, relay and crypto crates
//!    (`qrcode` is allowed: it draws a string).
//! 2. `rlord-relay` holds no content crypto and none of rlord: it routes
//!    ciphertext and never needs to read it.
//! 3. `rlord_e2e` is sans-IO: no async runtime, sockets or TLS.
//! 4. The web plugin's one listener stays the loopback one in `main.rs`:
//!    no other file under `crates/web/src/` names `TcpListener`.
//! 5. `crates/relay` is the only crate that may bind a non-loopback
//!    address.

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
    // Remote access: the plugin and the relay hold it, never rlord.
    "robot-overlord-e2e",
    "robot-overlord-e2e-wasm",
    "robot-overlord-relay",
    "robot-overlord-relay-proto",
    "robot-overlord-e2e-device",
    "snow",
    "libcrux-ml-kem",
    "chacha20poly1305",
    "curve25519-dalek",
    "p256",
    "ecdsa",
    "ring",
    "aws-lc-rs",
    "tokio-rustls",
    "webpki-roots",
    // Not `wasm-bindgen`: `ulid` (through `web-time`) depends on it for
    // wasm32 targets only, which rlord is never built for. The WASM
    // crypto build is denied by name above.
];

/// The relay's package, and what it must never depend on: content crypto
/// (it only forwards ciphertext) and rlord itself.
const RELAY: &str = "robot-overlord-relay";
pub const RELAY_DENIED: &[&str] = &[
    "robot-overlord-e2e",
    "snow",
    "libcrux-ml-kem",
    "ml-kem",
    "chacha20poly1305",
    "curve25519-dalek",
    "x25519-dalek",
    "robot-overlord-core",
    "robot-overlord-daemon",
    "robot-overlord-web",
];

/// The crypto core's package, and what keeps it sans-IO.
const E2E: &str = "robot-overlord-e2e";
pub const E2E_DENIED: &[&str] = &[
    "tokio",
    "mio",
    "axum",
    "hyper",
    "tungstenite",
    "rustls",
    "socket2",
];

/// The one file of the web plugin that may name a TCP listener.
const WEB_SRC: &str = "crates/web/src";
const WEB_LISTENER: &str = "main.rs";

/// Spellings of a bind to every interface. Only `crates/relay` may have
/// one (its configured `listen`).
const ANY_ADDRESS: &[&str] = &[
    "0.0.0.0",
    "Ipv4Addr::UNSPECIFIED",
    "Ipv6Addr::UNSPECIFIED",
    "\"[::]",
];
/// The crates (sources and tests) held to loopback binds.
const LOOPBACK_CRATES: &[&str] = &[
    "crates/core",
    "crates/daemon",
    "crates/tui",
    "crates/rlord",
    "crates/web",
    "crates/e2e",
    "crates/e2e-wasm",
    "crates/e2e-device",
    "crates/relay-proto",
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

/// Every check; an error lists everything found.
pub fn check() -> Result<()> {
    let mut found: Vec<String> = denied_in_tree(&tree(version::PACKAGE)?, DENIED)
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
    let relay = denied_in_tree(&tree(RELAY)?, RELAY_DENIED);
    if !relay.is_empty() {
        bail!(
            "rlord-relay depends on {} (the relay forwards ciphertext and links no \
             content crypto and no part of rlord)",
            relay.join(", ")
        );
    }
    let e2e = denied_in_tree(&tree(E2E)?, E2E_DENIED);
    if !e2e.is_empty() {
        bail!(
            "rlord_e2e depends on {} (the crypto core is sans-IO)",
            e2e.join(", ")
        );
    }
    let mut found = listeners_in_web(Path::new(WEB_SRC))?;
    for dir in LOOPBACK_CRATES {
        found.extend(any_address_binds(Path::new(dir))?);
    }
    if !found.is_empty() {
        bail!(
            "a listener outside its place (the plugin listens on loopback in main.rs \
             only; only crates/relay binds every interface):\n  {}",
            found.join("\n  ")
        );
    }
    println!(
        "check-deps: rlord has no network dependency; rlord-relay no content crypto; \
         rlord_e2e no IO"
    );
    Ok(())
}

/// A package's normal and build dependencies, every target, one
/// `name vX.Y.Z` per line.
fn tree(package: &str) -> Result<String> {
    cmd::output(cargo().args([
        "tree",
        "--locked",
        "-p",
        package,
        "-e",
        "normal,build",
        "--target",
        "all",
        "--prefix",
        "none",
        "--format",
        "{p}",
    ]))
}

/// Names from `denied` in `cargo tree --prefix none --format {p}` output
/// (`name vX.Y.Z (path)`, one per line, repeats marked `(*)`). Sorted, once
/// each.
pub fn denied_in_tree(tree: &str, denied: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = tree
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|name| denied.contains(name))
        .map(str::to_string)
        .collect();
    v.sort();
    v.dedup();
    v
}

/// The `.rs` files under `dir`, recursively.
fn rust_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd {
            let p = entry?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") {
                files.push(p);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// `path:line` for each `TcpListener` under the plugin's sources outside
/// `main.rs`.
fn listeners_in_web(dir: &Path) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for p in rust_files(dir)? {
        if p.strip_prefix(dir)
            .is_ok_and(|r| r == Path::new(WEB_LISTENER))
        {
            continue;
        }
        let text = std::fs::read_to_string(&p)?;
        for (i, line) in text.lines().enumerate() {
            if line.contains("TcpListener") {
                found.push(format!("{}:{}: TcpListener", p.display(), i + 1));
            }
        }
    }
    Ok(found)
}

/// `path:line: spelling` for each bind-to-everything spelling in `text`.
fn any_address_in(text: &str) -> Vec<(usize, &'static str)> {
    let mut v = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for a in ANY_ADDRESS {
            if line.contains(a) {
                v.push((i, *a));
            }
        }
    }
    v
}

fn any_address_binds(dir: &Path) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for p in rust_files(dir)? {
        let text = std::fs::read_to_string(&p)?;
        for (i, a) in any_address_in(&text) {
            found.push(format!("{}:{}: {a}", p.display(), i + 1));
        }
    }
    Ok(found)
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
        assert!(denied_in_tree(clean, DENIED).is_empty());
        let planted = format!("{clean}axum v0.8.9\nhyper v1.12.0\naxum v0.8.9 (*)\n");
        assert_eq!(denied_in_tree(&planted, DENIED), ["axum", "hyper"]);
        let plugin = "robot-overlord-web v0.23.0 (/x/crates/web)\n";
        assert_eq!(denied_in_tree(plugin, DENIED), ["robot-overlord-web"]);
        let crypto = "snow v0.10.0\nqrcode v0.14.1\nring v0.17.14\n";
        assert_eq!(denied_in_tree(crypto, DENIED), ["ring", "snow"]);
    }

    #[test]
    fn content_crypto_planted_in_the_relay_is_caught() {
        let clean = "robot-overlord-relay v1.3.0 (/x/crates/relay)\n\
                     robot-overlord-relay-proto v1.3.0 (/x/crates/relay-proto)\n\
                     p256 v0.14.0\nrustls v0.23.46\nring v0.17.14\nrusqlite v0.42.0\n";
        assert!(denied_in_tree(clean, RELAY_DENIED).is_empty());
        let planted = format!("{clean}snow v0.10.0\nrobot-overlord-core v1.3.0 (/x)\n");
        assert_eq!(
            denied_in_tree(&planted, RELAY_DENIED),
            ["robot-overlord-core", "snow"]
        );
    }

    #[test]
    fn io_planted_in_the_crypto_core_is_caught() {
        let clean = "robot-overlord-e2e v1.3.0 (/x/crates/e2e)\nsnow v0.10.0\nsha2 v0.10.9\n";
        assert!(denied_in_tree(clean, E2E_DENIED).is_empty());
        let planted = format!("{clean}tokio v1.48.0\nmio v1.0.0\n");
        assert_eq!(denied_in_tree(&planted, E2E_DENIED), ["mio", "tokio"]);
    }

    #[test]
    fn listeners_and_wide_binds_are_caught() {
        let tmp = std::env::temp_dir().join(format!("check-deps-{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(src.join("remote")).unwrap();
        std::fs::write(src.join("main.rs"), "TcpListener::bind(\"127.0.0.1:1\")").unwrap();
        assert!(listeners_in_web(&src).unwrap().is_empty());
        std::fs::write(src.join("remote/link.rs"), "let l = TcpListener::bind(a);").unwrap();
        let found = listeners_in_web(&src).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].ends_with("link.rs:1: TcpListener"));
        std::fs::write(
            src.join("x.rs"),
            "bind(\"0.0.0.0:80\")\nIpv6Addr::UNSPECIFIED",
        )
        .unwrap();
        assert_eq!(any_address_binds(&tmp).unwrap().len(), 2);
        assert!(any_address_in("bind(\"127.0.0.1:0\") // a::b").is_empty());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn the_remote_crates_are_clean_today() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        assert!(listeners_in_web(&root.join(WEB_SRC)).unwrap().is_empty());
        for dir in LOOPBACK_CRATES {
            let found = any_address_binds(&root.join(dir)).unwrap();
            assert!(found.is_empty(), "{found:?}");
        }
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
