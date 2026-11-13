//! `cargo xtask publish-crates [--dry-run]`: publish a stable release's
//! crates to crates.io (users: cargo install robot-overlord). Run by
//! `cargo xtask release` after the GitHub release; needs `cargo login` with
//! a token that can publish.
//!
//! The workspace's internal dependencies are path-only in git (so PRs don't
//! have to bump them); crates.io needs a version on each, so this pins them
//! to the release version for the publish and puts the files back after.
//! (If it is interrupted, put them back with
//! `git checkout -- crates/*/Cargo.toml`.)

use anyhow::{bail, Context, Result};

use crate::cmd::{self, cargo};
use crate::version::{self, Version, CRATES};

const MANIFESTS: [&str; 3] = [
    "crates/daemon/Cargo.toml",
    "crates/tui/Cargo.toml",
    "crates/rlord/Cargo.toml",
];

pub fn main(args: &[String]) -> Result<()> {
    let dry_run = match args {
        [] => false,
        [flag] if flag == "--dry-run" => true,
        _ => return cmd::usage("usage: cargo xtask publish-crates [--dry-run]"),
    };
    publish(dry_run)
}

/// The manifests as they were, written back when dropped.
struct Restore(Vec<(&'static str, String)>);

impl Drop for Restore {
    fn drop(&mut self) {
        for (path, text) in &self.0 {
            if let Err(e) = std::fs::write(path, text) {
                eprintln!("publish-crates: couldn't restore {path}: {e}");
            }
        }
    }
}

pub fn publish(dry_run: bool) -> Result<()> {
    let ver = version::current()?;
    if ver.is_dev() {
        bail!("publish-crates: {ver} is a dev build; only stable releases go to crates.io");
    }

    let mut restore = Restore(Vec::new());
    for path in MANIFESTS {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        std::fs::write(path, pin_internal_deps(&text, &ver))?;
        restore.0.push((path, text));
    }

    // Dependencies first; each must be on the index before the next.
    for krate in CRATES {
        if !dry_run && cmd::succeeds(cargo().args(["info", &format!("{krate}@{ver}")])) {
            println!("publish-crates: {krate} {ver} already published");
            continue;
        }
        let mut publish = cargo();
        publish.args(["publish", "-p", krate, "--allow-dirty"]);
        if dry_run {
            publish.arg("--dry-run");
        }
        cmd::run(&mut publish)?;
    }
    Ok(())
}

/// Add `version = "=<ver>"` to every `package = "robot-overlord-…", path =
/// "…"` dependency in a manifest.
fn pin_internal_deps(manifest: &str, ver: &Version) -> String {
    const START: &str = "package = \"robot-overlord-";
    const MIDDLE: &str = "\", path = \"";
    let mut out = String::with_capacity(manifest.len() + 64);
    let mut rest = manifest;
    while let Some(at) = rest.find(START) {
        let after_start = at + START.len();
        out.push_str(&rest[..after_start]);
        rest = &rest[after_start..];
        // [a-z]+ then `", path = "` then [^"]+ then `"`.
        let name = rest.bytes().take_while(u8::is_ascii_lowercase).count();
        let Some(path) = rest[name..].strip_prefix(MIDDLE).filter(|_| name > 0) else {
            continue;
        };
        let Some(len) = path.find('"').filter(|len| *len > 0) else {
            continue;
        };
        let end = name + MIDDLE.len() + len + 1;
        out.push_str(&rest[..end]);
        out.push_str(&format!(", version = \"={ver}\""));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_internal_dependencies_only() {
        let ver = Version::parse("1.0.0").unwrap();
        let manifest = "\
[package]
name = \"robot-overlord-tui\"

[dependencies]
rlord-core = { package = \"robot-overlord-core\", path = \"../core\" }
rlord-daemon = { package = \"robot-overlord-daemon\", path = \"../daemon\" }
anyhow.workspace = true
other = { package = \"robot-overlord-\", path = \"../x\" }
remote = { package = \"robot-overlord-core\", version = \"1\" }
";
        let pinned = pin_internal_deps(manifest, &ver);
        assert_eq!(
            pinned,
            "\
[package]
name = \"robot-overlord-tui\"

[dependencies]
rlord-core = { package = \"robot-overlord-core\", path = \"../core\", version = \"=1.0.0\" }
rlord-daemon = { package = \"robot-overlord-daemon\", path = \"../daemon\", version = \"=1.0.0\" }
anyhow.workspace = true
other = { package = \"robot-overlord-\", path = \"../x\" }
remote = { package = \"robot-overlord-core\", version = \"1\" }
"
        );
    }

    #[test]
    fn every_manifest_with_an_internal_dependency_is_pinned() {
        let ver = Version::parse("1.0.0").unwrap();
        for (path, text) in [
            ("crates/core", include_str!("../../crates/core/Cargo.toml")),
            (
                "crates/daemon",
                include_str!("../../crates/daemon/Cargo.toml"),
            ),
            ("crates/tui", include_str!("../../crates/tui/Cargo.toml")),
            (
                "crates/rlord",
                include_str!("../../crates/rlord/Cargo.toml"),
            ),
        ] {
            let listed = MANIFESTS.contains(&format!("{path}/Cargo.toml").as_str());
            let internal = text.matches("package = \"robot-overlord-").count();
            assert_eq!(internal > 0, listed, "{path}: MANIFESTS is out of date");
            let pinned = pin_internal_deps(text, &ver);
            assert_eq!(
                pinned.matches("version = \"=1.0.0\"").count(),
                internal,
                "{path}"
            );
        }
    }
}
