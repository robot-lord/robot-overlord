//! `cargo xtask smoke-linux DIST [IMAGE…]`: smoke-test release files on
//! Linux distros, in Docker (colima on the Mac). Each container installs
//! with get.sh from the local files (it installs tmux too), then runs the
//! daemon and a few commands on a scratch tmux socket
//! (`smoke_in_container.sh`, compiled in and written to a scratch folder
//! for the container, as fresh-install does; POSIX sh since the image has
//! nothing else).
//!
//! Native images by default; a `linux/amd64:IMAGE` entry runs the x86_64
//! build under emulation when Docker has it. get.sh checks the tarball
//! against DIST/SHA256SUMS, so write that first.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::cmd::{self, TempDir};

const SCRIPT: &str = include_str!("smoke_in_container.sh");

const USAGE: &str = "usage: cargo xtask smoke-linux DIST [IMAGE…]";

/// Arch: only as amd64 (emulated), where its mirrors often fail to sync;
/// run it by name when needed. The x86_64 build is tested on Ubuntu.
pub const DEFAULT_IMAGES: [&str; 6] = [
    "ubuntu:22.04",
    "ubuntu:24.04",
    "debian:12",
    "fedora:41",
    "amazonlinux:2023",
    "linux/amd64:ubuntu:24.04",
];

/// How many lines of each container's output to show.
const TAIL: usize = 6;

pub fn main(args: &[String], invoked_from: &Path) -> Result<()> {
    let Some((dist, images)) = args.split_first() else {
        return cmd::usage(USAGE);
    };
    let dist = invoked_from.join(dist);
    let dist = dist
        .canonicalize()
        .with_context(|| format!("{}", dist.display()))?;
    run(&dist, images)
}

/// An image to test in: `IMAGE`, or `PLATFORM:IMAGE` for a `linux/…`
/// platform.
#[derive(Debug, PartialEq, Eq)]
pub struct Spec<'a> {
    pub platform: Option<&'a str>,
    pub image: &'a str,
}

impl<'a> Spec<'a> {
    pub fn parse(s: &'a str) -> Self {
        match s.split_once(':') {
            Some((platform, image)) if platform.starts_with("linux/") => Self {
                platform: Some(platform),
                image,
            },
            _ => Self {
                platform: None,
                image: s,
            },
        }
    }
}

/// The last `n` lines of `text`.
pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Whether a container's output ends with the script's `OK`.
pub fn passed(output: &str) -> bool {
    output.lines().last() == Some("OK")
}

/// Smoke-test `dist` (absolute) on `images`, or the default set if empty.
pub fn run(dist: &Path, images: &[String]) -> Result<()> {
    let images: Vec<&str> = if images.is_empty() {
        DEFAULT_IMAGES.to_vec()
    } else {
        images.iter().map(String::as_str).collect()
    };
    if !cmd::succeeds(Command::new("docker").arg("info"))? {
        bail!("Docker isn't running (colima start)");
    }
    let get_sh = std::path::absolute("scripts/get.sh")?;
    // Under target/, not the system temp dir: colima only mounts $HOME.
    let files = TempDir::new_in(Path::new("target"), "smoke")?;
    let script = std::path::absolute(files.path().join("smoke.sh"))?;
    std::fs::write(&script, SCRIPT)?;

    let mut failed = Vec::new();
    for spec in &images {
        let out = test_in(Spec::parse(spec), dist, &get_sh, &script)?;
        println!("{out}");
        if !passed(&out) {
            eprintln!("FAILED: {spec}");
            failed.push(*spec);
        }
    }
    if !failed.is_empty() {
        bail!("smoke test failed on {}", failed.join(", "));
    }
    Ok(())
}

/// Run the test in one image: a header line and the output's tail.
fn test_in(spec: Spec, dist: &Path, get_sh: &Path, script: &Path) -> Result<String> {
    let mut header = format!("== {}", spec.image);
    let mut docker = Command::new("docker");
    docker.args(["run", "--rm"]);
    if let Some(p) = spec.platform {
        header.push_str(&format!(" --platform {p}"));
        docker.args(["--platform", p]);
    }
    let mount = |from: &Path, to: &str| format!("{}:{to}:ro", from.display());
    docker
        .arg("-v")
        .arg(mount(dist, "/dist"))
        .arg("-v")
        .arg(mount(get_sh, "/get.sh"))
        .arg("-v")
        .arg(mount(script, "/smoke.sh"))
        .args(["-e", "RLORD_DIST=/dist", "-e", "RLORD_YES=1"])
        .args([spec.image, "sh", "/smoke.sh"])
        .stdin(Stdio::null());

    // stdout and stderr interleaved in one pipe, as `2>&1` would.
    cmd::check_interrupted()?;
    let (mut reader, writer) = std::io::pipe()?;
    docker.stdout(writer.try_clone()?).stderr(writer);
    let mut child = docker.spawn().context("couldn't start docker")?;
    // The Command holds the pipe's write ends: drop it, or the read never
    // sees the end.
    drop(docker);
    let mut output = Vec::new();
    reader.read_to_end(&mut output)?;
    child.wait()?;
    cmd::check_interrupted()?;

    let tail = tail(&String::from_utf8_lossy(&output), TAIL);
    Ok(if tail.is_empty() {
        header
    } else {
        format!("{header}\n{tail}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_image_specs() {
        assert_eq!(
            Spec::parse("ubuntu:24.04"),
            Spec {
                platform: None,
                image: "ubuntu:24.04"
            }
        );
        assert_eq!(
            Spec::parse("linux/amd64:ubuntu:24.04"),
            Spec {
                platform: Some("linux/amd64"),
                image: "ubuntu:24.04"
            }
        );
        assert_eq!(
            Spec::parse("linux/arm64:archlinux"),
            Spec {
                platform: Some("linux/arm64"),
                image: "archlinux"
            }
        );
        assert_eq!(
            Spec::parse("archlinux"),
            Spec {
                platform: None,
                image: "archlinux"
            }
        );
        // A registry path, not a platform.
        assert_eq!(Spec::parse("ghcr.io/x/y:1").platform, None);
    }

    #[test]
    fn keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("a\nb", 6), "a\nb");
        assert_eq!(tail("", 6), "");
    }

    #[test]
    fn passes_only_on_a_final_ok() {
        assert!(passed("== x\nrlord 1.0.0\nOK"));
        assert!(passed("OK\n"));
        assert!(!passed("== x\nOK\nerror: boom"));
        assert!(!passed("== x"));
        assert!(!passed(""));
    }
}
