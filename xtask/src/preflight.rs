//! `cargo xtask release-preflight`: the release workflow's first job. Fail
//! fast, before any build: only merged code, a stable version, the ref
//! matching Cargo.toml. A dry run reports these as warnings. A tag whose
//! release is already published (made by `cargo xtask release --local`) is
//! marked to skip, so a local fallback never races the workflow.
//!
//! Reads DRY_RUN, REF and GITHUB_REPOSITORY; writes `tag`, `assets` (the
//! files the builds must produce) and `skip` to GITHUB_OUTPUT.

use std::io::Write;

use anyhow::{bail, Context, Result};

use crate::cmd::{self, gh, git};
use crate::version::{self, Version};

pub fn main(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return cmd::usage("usage: cargo xtask release-preflight");
    }
    let env = |k: &str| std::env::var(k).with_context(|| format!("{k} isn't set"));
    let dry_run = env("DRY_RUN")? == "true";
    let reference = env("REF")?;
    let repo = env("GITHUB_REPOSITORY")?;
    let ver = version::current()?;
    let tag = ver.tag();

    let mut outputs = vec![
        format!("tag={tag}"),
        format!("assets={}", ver.workflow_assets().join(" ")),
    ];

    let head = cmd::output(git().args(["rev-parse", "HEAD"]))?;
    let merged = cmd::succeeds(git().args(["merge-base", "--is-ancestor", "HEAD", "origin/main"]));
    let problems = problems(&ver, &reference, (!merged).then_some(head.as_str()));
    for p in &problems {
        if dry_run {
            println!("::warning::{p} (dry run: continuing)");
        } else {
            println!("::error::{p}");
        }
    }
    // The tag is written even when the checks fail, as it always was.
    let failed = !dry_run && !problems.is_empty();

    // Published releases only (drafts aren't visible to this token).
    if !failed
        && !dry_run
        && cmd::succeeds(gh().args(["api", &format!("repos/{repo}/releases/tags/{tag}")]))
    {
        println!(
            "::notice::{tag} is already published (cargo xtask release --local); nothing to build"
        );
        outputs.push("skip=true".into());
    }

    let path = env("GITHUB_OUTPUT")?;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("opening {path}"))?;
    for line in outputs {
        writeln!(file, "{line}")?;
    }
    if failed {
        bail!("{tag} can't be released from {reference}");
    }
    Ok(())
}

/// What stops this ref from being released. `unmerged_head` is HEAD when
/// it isn't on origin/main.
fn problems(ver: &Version, reference: &str, unmerged_head: Option<&str>) -> Vec<String> {
    let tag = ver.tag();
    let mut problems = Vec::new();
    if let Some(head) = unmerged_head {
        problems.push(format!("{head} is not on origin/main"));
    }
    if ver.is_dev() {
        problems.push(format!("{tag} is not a stable version"));
    }
    if reference.strip_prefix("refs/tags/").unwrap_or(reference) != tag {
        problems.push(format!(
            "ref {reference} doesn't match Cargo.toml version {tag}"
        ));
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn a_stable_tag_on_main_has_no_problems() {
        assert!(problems(&v("1.0.0"), "refs/tags/v1.0.0", None).is_empty());
        // A manual run passes the bare tag.
        assert!(problems(&v("1.0.0"), "v1.0.0", None).is_empty());
    }

    #[test]
    fn reports_each_problem() {
        let all = problems(&v("0.23.0-dev.8"), "refs/heads/xtask-release", Some("abc"));
        assert_eq!(
            all,
            [
                "abc is not on origin/main",
                "v0.23.0-dev.8 is not a stable version",
                "ref refs/heads/xtask-release doesn't match Cargo.toml version v0.23.0-dev.8",
            ]
        );
        assert_eq!(
            problems(&v("1.0.0"), "refs/tags/v1.0.1", None),
            ["ref refs/tags/v1.0.1 doesn't match Cargo.toml version v1.0.0"]
        );
    }
}
