//! `cargo xtask check-version-bump [BASE] [REV]` (defaults: origin/main,
//! HEAD): fail unless REV bumps `[workspace.package] version` above BASE's
//! and Cargo.lock agrees. Every PR merged to main ships a release (see
//! CLAUDE.md), so every PR must bump the version: a development build
//! (X.Y.Z-dev.N, the usual) or a stable release (X.Y.Z).
//!
//! Used by CI (pull requests) and .githooks/pre-push.

use anyhow::{bail, Context, Result};

use crate::cmd::{self, git};
use crate::version::{self, Version};

pub fn main(args: &[String]) -> Result<()> {
    if args.len() > 2 {
        return cmd::usage("usage: cargo xtask check-version-bump [BASE] [REV]");
    }
    let base = args.first().map_or("origin/main", String::as_str);
    let rev = args.get(1).map_or("HEAD", String::as_str);

    let fork = cmd::output(git().args(["merge-base", base, rev]))?;
    let base_ver = version_at(&fork)?;
    let new_ver = version_at(rev)?;
    let lock = cmd::output(git().args(["show", &format!("{rev}:Cargo.lock")]))?;
    let line = check(
        &base_ver,
        &new_ver,
        version::lock_version(&lock).as_deref(),
        base,
    )?;
    println!("{line}");
    Ok(())
}

fn version_at(rev: &str) -> Result<String> {
    let toml = cmd::output(git().args(["show", &format!("{rev}:Cargo.toml")]))?;
    version::workspace_version(&toml).context("version-bump: could not read versions")
}

/// The line to print when the bump is fine, or why it isn't.
fn check(base_ver: &str, new_ver: &str, lock_ver: Option<&str>, base: &str) -> Result<String> {
    let (old, new) = (Version::parse(base_ver)?, Version::parse(new_ver)?);
    if new <= old {
        bail!(
            "version-bump: Cargo.toml version is {new}; {base} has {old}.\n\
             Every PR ships a release, so bump [workspace.package] version in Cargo.toml\n\
             and commit Cargo.lock too: usually the next development build (0.18.0-dev.1,\n\
             then -dev.2, …), or X.Y.Z for a stable release."
        );
    }
    if lock_ver != Some(new_ver) {
        bail!(
            "version-bump: Cargo.lock has {} {}, Cargo.toml has {new} — run cargo build and commit Cargo.lock",
            version::PACKAGE,
            lock_ver.unwrap_or("no version"),
        );
    }
    Ok(format!("version-bump: {old} -> {new}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_bump_the_lock_agrees_with() {
        for (old, new) in [
            ("0.23.0-dev.7", "0.23.0-dev.8"),
            ("0.23.0-dev.9", "0.23.0-dev.10"),
            ("0.23.0-dev.8", "0.23.0"),
            ("0.23.0", "0.23.1-dev.1"),
            ("0.23.0", "1.0.0"),
        ] {
            let line = check(old, new, Some(new), "origin/main").unwrap();
            assert_eq!(line, format!("version-bump: {old} -> {new}"));
        }
    }

    #[test]
    fn refuses_the_same_or_an_older_version() {
        for (old, new) in [
            ("0.23.0-dev.7", "0.23.0-dev.7"),
            ("0.23.0-dev.10", "0.23.0-dev.9"),
            ("0.23.0", "0.23.0-dev.9"),
            ("0.23.1", "0.23.0"),
        ] {
            let err = check(old, new, Some(new), "origin/main").unwrap_err();
            assert!(err.to_string().contains("origin/main has"), "{err}");
        }
    }

    #[test]
    fn refuses_a_stale_lock() {
        let err = check(
            "0.23.0-dev.7",
            "0.23.0-dev.8",
            Some("0.23.0-dev.7"),
            "origin/main",
        )
        .unwrap_err();
        assert!(err.to_string().contains("Cargo.lock has"), "{err}");
        assert!(check("0.23.0-dev.7", "0.23.0-dev.8", None, "origin/main").is_err());
    }
}
