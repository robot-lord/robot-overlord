//! `cargo xtask web install|check|build|watch`: the web plugin's page
//! (`crates/web/ui/`), a TypeScript client built with Node tools.
//!
//! Node is needed to work on and build the page, never to install or run
//! rlord; a Rust-only build of `rlord-web` embeds a placeholder page
//! instead. Every tool is pinned exactly in `package.json`, with integrity
//! hashes in the committed `package-lock.json`, installed with `npm ci
//! --ignore-scripts` (no install scripts run; the tools' native binaries
//! come as optional dependencies, which need none) and run from
//! `node_modules/.bin`, never through `npx`, which could fetch a package.
//! See docs/working/web-ui-remote.md, "The frontend stack".

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::{cmd, web_app};

const UI: &str = "crates/web/ui";

const USAGE: &str = "usage: cargo xtask web install|check|build [--app]|check-app|watch

  install       npm ci --ignore-scripts in crates/web/ui (checks Node against .node-version)
  check         types (tsc), lint (oxlint, type-aware), format (biome, check only), tests
                (vitest); both entries, the local page's and the phone app's
  build         the page into crates/web/ui/dist/, which rlord-web embeds when built next
  build --app   the phone app's page into crates/web/ui/dist-app/, with its asset table
                (manifest.json) for the apps' shells; then check-app
  check-app     the built phone page: every file local, no network, its CSP, no xterm.js
                and no LocalTransport
  watch         rebuild dist/ on every change; run a debug rlord-web with
                RLORD_WEB_ASSETS=crates/web/ui/dist to serve it from disk";

pub fn main(args: &[String]) -> Result<()> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let ui = PathBuf::from(UI);
    match args.as_slice() {
        ["install"] => install(&ui),
        ["check"] => {
            ensure_installed(&ui)?;
            check(&ui)
        }
        ["build"] => {
            ensure_installed(&ui)?;
            run(&ui, "vite", &["build"])?;
            web_app::check_local(&ui.join("dist"))?;
            println!("built {UI}/dist: rebuild rlord-web to embed it");
            Ok(())
        }
        ["build", "--app"] => {
            ensure_installed(&ui)?;
            run(&ui, "vite", &["build", "--mode", "app"])?;
            web_app::finish(&ui.join("dist-app"))?;
            println!("built {UI}/dist-app for the phone app");
            Ok(())
        }
        ["check-app"] => web_app::check(&ui.join("dist-app")),
        ["watch"] => {
            ensure_installed(&ui)?;
            println!(
                "rebuilding {UI}/dist on change; serve it with a debug rlord-web and \
                 RLORD_WEB_ASSETS={UI}/dist"
            );
            run(&ui, "vite", &["build", "--watch"])
        }
        _ => cmd::usage(USAGE),
    }
}

/// Types, lint, format and unit tests, in that order.
fn check(ui: &Path) -> Result<()> {
    run(ui, "tsc", &["--noEmit", "-p", "."])?;
    run(ui, "oxlint", &["--type-aware", "--deny-warnings"])?;
    // Without --write: fails on any file biome would change.
    run(ui, "biome", &["format", "."])?;
    run(ui, "vitest", &["run"])
}

/// `npm ci --ignore-scripts`, after checking Node.
fn install(ui: &Path) -> Result<()> {
    check_node(ui)?;
    cmd::run(
        Command::new("npm")
            .args(["ci", "--ignore-scripts", "--no-audit", "--no-fund"])
            .current_dir(ui),
    )
}

/// Install first when `node_modules` is missing or older than the lockfile.
pub(crate) fn ensure_installed(ui: &Path) -> Result<()> {
    let modified = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let lock = modified(ui.join("package-lock.json"));
    let installed = modified(ui.join("node_modules/.package-lock.json"));
    match (lock, installed) {
        (Some(lock), Some(installed)) if installed >= lock => Ok(()),
        _ => install(ui),
    }
}

/// A tool from `node_modules/.bin`, run in `ui`.
fn run(ui: &Path, tool: &str, args: &[&str]) -> Result<()> {
    cmd::run(self::tool(ui, tool)?.args(args))
}

/// A command for a tool from `node_modules/.bin`, to run in `ui`.
pub(crate) fn tool(ui: &Path, tool: &str) -> Result<Command> {
    let bin = ui.join("node_modules/.bin").join(tool);
    if !bin.exists() {
        bail!(
            "{} is missing: run `cargo xtask web install`",
            bin.display()
        );
    }
    let bin = bin
        .canonicalize()
        .with_context(|| format!("resolving {}", bin.display()))?;
    let mut c = Command::new(bin);
    c.current_dir(ui);
    Ok(c)
}

/// Node's major version must be `.node-version`'s; another patch or minor
/// release of it only gets a note (CI and the release job use the exact one).
fn check_node(ui: &Path) -> Result<()> {
    let file = ui.join(".node-version");
    let want =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let want = want.trim();
    let have = cmd::output(Command::new("node").arg("--version"))
        .context("Node is needed to build the web page (see crates/web/ui/.node-version)")?;
    match node_matches(want, &have) {
        Some(true) => Ok(()),
        Some(false) => {
            eprintln!("note: Node {have}, {UI}/.node-version says {want} (same major: going on)");
            Ok(())
        }
        None => bail!("Node {have}, but {UI}/.node-version says {want}: install Node {want}"),
    }
}

/// Whether Node `have` (`v26.13.0`) is `want` (`26.13.1`): `Some(true)`
/// exactly, `Some(false)` with the same major, `None` otherwise.
fn node_matches(want: &str, have: &str) -> Option<bool> {
    let have = have.trim().trim_start_matches('v');
    let want = want.trim().trim_start_matches('v');
    let major = |v: &str| v.split('.').next().map(str::to_string);
    if have == want {
        Some(true)
    } else if major(have).is_some_and(|m| !m.is_empty()) && major(have) == major(want) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_versions() {
        assert_eq!(node_matches("26.13.1", "v26.13.1"), Some(true));
        assert_eq!(node_matches("26.13.1\n", "v26.13.1\n"), Some(true));
        assert_eq!(node_matches("26.13.1", "v26.13.0"), Some(false));
        assert_eq!(node_matches("26.13.1", "v26.2.0"), Some(false));
        assert_eq!(node_matches("26.13.1", "v24.13.1"), None);
        assert_eq!(node_matches("26.13.1", "v2.13.1"), None);
        assert_eq!(node_matches("26.13.1", ""), None);
    }

    #[test]
    fn pinned_exactly_and_node_named() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(UI);
        let pkg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("package.json")).unwrap())
                .unwrap();
        for section in ["dependencies", "devDependencies"] {
            for (name, v) in pkg[section].as_object().unwrap() {
                let v = v.as_str().unwrap();
                assert!(
                    v.chars().next().is_some_and(|c| c.is_ascii_digit()),
                    "{name} {v}: pin an exact version"
                );
            }
        }
        let node = std::fs::read_to_string(root.join(".node-version")).unwrap();
        assert!(node_matches(node.trim(), node.trim()) == Some(true));
    }
}
