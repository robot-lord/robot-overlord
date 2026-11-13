//! `cargo xtask build-release macos|linux DIST`: build rlord's release
//! files for one platform. The one place the release build commands live:
//! the release workflow and `cargo xtask release` both run it, so CI and a
//! local build can't drift.
//!
//! Linux needs rustup targets x86_64/aarch64-unknown-linux-musl, zig,
//! cargo-zigbuild, cargo-deb and cargo-generate-rpm. Writes no SHA256SUMS:
//! the caller sums the full set of files.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::cmd::{self, cargo, TempDir};
use crate::version::{self, Target, Version, LINUX, MAC};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Macos,
    Linux,
}

const USAGE: &str = "usage: cargo xtask build-release macos|linux DIST";

pub fn main(args: &[String]) -> Result<()> {
    let [platform, dist] = args else {
        return cmd::usage(USAGE);
    };
    let platform = match platform.as_str() {
        "macos" => Platform::Macos,
        "linux" => Platform::Linux,
        other => return cmd::usage(format!("unknown platform {other} (macos|linux)")),
    };
    build(platform, Path::new(dist))
}

pub fn is_apple_silicon_mac() -> bool {
    std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64"
}

/// Build one platform's files into `dist` (created if missing).
pub fn build(platform: Platform, dist: &Path) -> Result<()> {
    std::fs::create_dir_all(dist).with_context(|| format!("creating {}", dist.display()))?;
    let dist = dist.canonicalize()?;
    let ver = version::current()?;
    let stage = TempDir::new_in(Path::new("target"), "pack")?;

    match platform {
        Platform::Macos => {
            if !is_apple_silicon_mac() {
                bail!("build macOS on Apple Silicon");
            }
            cargo_build("build", MAC)?;
            let bin = binary(MAC);
            // The linker ad-hoc signs, but post-link strip can leave the
            // signature stale; re-sign so the binary runs on other Macs
            // (arm64 refuses unsigned code).
            cmd::run(
                Command::new("codesign")
                    .args(["--force", "--sign", "-"])
                    .arg(&bin),
            )?;
            cmd::run(Command::new("codesign").arg("--verify").arg(&bin))?;
            let reported = cmd::output(Command::new(&bin).arg("--version"))?;
            if reported != format!("rlord {ver}") {
                bail!("built binary reports the wrong version ({reported})");
            }
            pack(&ver, MAC, &bin, stage.path(), &dist)?;
        }
        Platform::Linux => {
            for t in LINUX {
                cargo_build("zigbuild", t)?;
                pack(&ver, t, &binary(t), stage.path(), &dist)?;
            }
            // Distro packages: the same static binaries, requiring tmux.
            let out_dir = format!("{}/", dist.display());
            for t in LINUX {
                cmd::run(
                    cargo()
                        .args(["deb", "-p", version::PACKAGE, "--target", t.triple])
                        .args(["--no-build", "--no-strip", "--output", &out_dir])
                        .stdout(Stdio::null()),
                )?;
                let mut rpm = cargo();
                rpm.args(["generate-rpm", "-p", "crates/rlord", "--target", t.triple])
                    .args(["--auto-req", "disabled"]);
                // RPM forbids '-' in a version (only a dry run of a dev
                // version gets here).
                if ver.is_dev() {
                    rpm.arg("-s")
                        .arg(format!("version = \"{}\"", ver.package_version()));
                }
                cmd::run(rpm.args(["-o", &out_dir]).stdout(Stdio::null()))?;
            }
        }
    }
    Ok(())
}

fn cargo_build(subcommand: &str, target: Target) -> Result<()> {
    cmd::run(cargo().arg(subcommand).args([
        "--release",
        "--locked",
        "-p",
        version::PACKAGE,
        "--target",
        target.triple,
    ]))
}

fn binary(target: Target) -> PathBuf {
    Path::new("target")
        .join(target.triple)
        .join("release/rlord")
}

/// One tarball per target: the binary, the installer, the licenses.
fn pack(ver: &Version, target: Target, bin: &Path, stage: &Path, dist: &Path) -> Result<()> {
    let name = ver.tarball_stem(target);
    let dir = stage.join(&name);
    std::fs::create_dir(&dir)?;
    let files = [
        bin,
        Path::new("scripts/install.sh"),
        Path::new("LICENSE-MIT"),
        Path::new("LICENSE-APACHE"),
        Path::new("README.md"),
    ];
    for f in files {
        let to = dir.join(f.file_name().context("file name")?);
        std::fs::copy(f, &to).with_context(|| format!("copying {}", f.display()))?;
    }
    cmd::run(
        Command::new("tar")
            .arg("-czf")
            .arg(dist.join(ver.tarball(target)))
            .arg("-C")
            .arg(stage)
            .arg(&name),
    )
}
