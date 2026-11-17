//! `cargo xtask build-release macos|linux DIST`: build rlord's release
//! files for one platform. The one place the release build commands live:
//! the release workflow and `cargo xtask release` both run it, so CI and a
//! local build can't drift.
//!
//! Linux needs the musl targets (rust-toolchain.toml installs them) and the
//! packaging tools at their pinned versions (`cargo xtask install-tools`,
//! xtask/src/tools.rs). Writes no SHA256SUMS: the caller sums the full set
//! of files.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::cmd::{self, cargo, cargo_plugin, TempDir};
use crate::tools;
use crate::version::{self, Target, Version, LINUX, MAC};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Macos,
    Linux,
}

const USAGE: &str = "usage: cargo xtask build-release macos|linux DIST";

pub fn main(args: &[String], invoked_from: &Path) -> Result<()> {
    let [platform, dist] = args else {
        return cmd::usage(USAGE);
    };
    let platform = match platform.as_str() {
        "macos" => Platform::Macos,
        "linux" => Platform::Linux,
        other => return cmd::usage(format!("unknown platform {other} (macos|linux)")),
    };
    // DIST is the caller's path, not relative to the repository root.
    build(platform, &invoked_from.join(dist))
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
    let target_dir = target_dir()?;

    match platform {
        Platform::Macos => {
            if !is_apple_silicon_mac() {
                bail!("build macOS on Apple Silicon");
            }
            cmd::run(cargo().arg("build").args(build_args(MAC)))?;
            let bin = binary(&target_dir, MAC);
            // The linker ad-hoc signs, but post-link strip can leave the
            // signature stale; re-sign so the binary runs on other Macs
            // (arm64 refuses unsigned code). With a Developer ID in the
            // keychain the signature keeps one identity across builds, so
            // macOS privacy grants (folder access) survive an update.
            let identity = sign_identity();
            match &identity {
                Some(name) => eprintln!("signing with {name}"),
                None => eprintln!("signing ad-hoc (no Developer ID Application identity)"),
            }
            cmd::run(
                Command::new("codesign")
                    .args(sign_args(identity.as_deref()))
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
            // Only the pinned tools, run from their folder (not through
            // cargo, which would prefer ~/.cargo/bin), with zig on PATH.
            let bin = tools::ready()?;
            let path = tools::path_with(&bin)?;
            let tool = |name: &str| {
                let mut c = cargo_plugin(bin.join(format!("cargo-{name}")));
                c.arg(name).env("PATH", &path);
                c
            };
            for t in LINUX {
                cmd::run(tool("zigbuild").args(build_args(t)))?;
                pack(&ver, t, &binary(&target_dir, t), stage.path(), &dist)?;
            }
            // Distro packages: the same static binaries, requiring tmux.
            let out_dir = format!("{}/", dist.display());
            for t in LINUX {
                cmd::run(
                    tool("deb")
                        .args(["-p", version::PACKAGE, "--target", t.triple])
                        .args(["--no-build", "--no-strip", "--output", &out_dir])
                        .stdout(Stdio::null()),
                )?;
                let mut rpm = tool("generate-rpm");
                rpm.args(["-p", "crates/rlord", "--target", t.triple])
                    .args(["--auto-req", "disabled"])
                    // It doesn't ask cargo where the binary is (cargo-deb
                    // does): tell it, as the tarballs use.
                    .arg("--target-dir")
                    .arg(&target_dir);
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

fn build_args(target: Target) -> [&'static str; 6] {
    [
        "--release",
        "--locked",
        "-p",
        version::PACKAGE,
        "--target",
        target.triple,
    ]
}

/// Where cargo builds, as it resolves it (CARGO_TARGET_DIR,
/// `build.target-dir`), so the files packed are the ones just built.
pub fn target_dir() -> Result<PathBuf> {
    let json = cmd::output(cargo().args(["metadata", "--format-version", "1", "--no-deps"]))?;
    let meta: serde_json::Value = serde_json::from_str(&json).context("reading cargo metadata")?;
    meta["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .context("cargo metadata has no target_directory")
}

fn binary(target_dir: &Path, target: Target) -> PathBuf {
    target_dir.join(target.triple).join("release/rlord")
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
            // No extended attributes: macOS tags files (com.apple.provenance)
            // and GNU tar warns about every one when unpacking on Linux.
            // (Both bsdtar and GNU tar take this.) COPYFILE_DISABLE stops
            // bsdtar adding AppleDouble `._*` entries, which `--no-xattrs`
            // alone doesn't; GNU tar ignores it.
            .env("COPYFILE_DISABLE", "1")
            .arg("--no-xattrs")
            .arg("-czf")
            .arg(dist.join(ver.tarball(target)))
            .arg("-C")
            .arg(stage)
            .arg(&name),
    )
}

/// The bundle-style identifier a Developer ID signature carries. Fixed, so
/// macOS sees every build as the same program.
const SIGN_ID: &str = "com.robot-lord.rlord";

/// `codesign` arguments: a Developer ID with hardened runtime and a secure
/// timestamp, or ad-hoc (`-`) when there is none.
fn sign_args(identity: Option<&str>) -> Vec<String> {
    match identity {
        Some(name) => [
            "--force",
            "--sign",
            name,
            "--options",
            "runtime",
            "--timestamp",
            "--identifier",
            SIGN_ID,
        ]
        .map(String::from)
        .to_vec(),
        None => ["--force", "--sign", "-"].map(String::from).to_vec(),
    }
}

/// The identity to sign with. `RLORD_SIGN_IDENTITY` wins ("-" or "" for
/// ad-hoc); otherwise the keychain's first valid "Developer ID
/// Application" identity, if any (none on CI runners: ad-hoc there).
fn sign_identity() -> Option<String> {
    if let Ok(v) = std::env::var("RLORD_SIGN_IDENTITY") {
        let v = v.trim();
        return (!v.is_empty() && v != "-").then(|| v.to_string());
    }
    let out = Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    developer_id(&String::from_utf8_lossy(&out.stdout))
}

/// The first "Developer ID Application: …" name in `security
/// find-identity -v` output.
fn developer_id(listing: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let start = line.find('"')?;
        let rest = &line[start + 1..];
        let name = &rest[..rest.find('"')?];
        name.starts_with("Developer ID Application:")
            .then(|| name.to_string())
    })
}

#[cfg(test)]
mod sign_tests {
    use super::*;

    #[test]
    fn picks_the_developer_id_identity() {
        let listing = "  1) ABCDEF \"Apple Development: X (T1)\"\n  2) 012345 \"Developer ID Application: Brandon Ayers (PL2X8AJK8A)\"\n     2 valid identities found\n";
        assert_eq!(
            developer_id(listing).as_deref(),
            Some("Developer ID Application: Brandon Ayers (PL2X8AJK8A)")
        );
        assert_eq!(developer_id("     0 valid identities found\n"), None);
    }

    #[test]
    fn signs_ad_hoc_without_an_identity() {
        assert_eq!(sign_args(None), ["--force", "--sign", "-"]);
        let a = sign_args(Some("Developer ID Application: X (T)"));
        assert!(a.windows(2).any(|w| w == ["--options", "runtime"]));
        assert!(a.windows(2).any(|w| w == ["--identifier", SIGN_ID]));
        assert!(a.contains(&"--timestamp".to_string()));
    }
}
