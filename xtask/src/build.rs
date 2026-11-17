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
            sign(&bin)?;
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

/// The identifier a Developer ID signature carries. Fixed, so macOS sees
/// every build as the same program.
const SIGN_ID: &str = "com.robot-lord.rlord";

/// What to sign with.
#[derive(Debug, PartialEq, Eq)]
enum Signer {
    /// A Developer ID, by its SHA-1 hash (unambiguous across renewals) and name.
    Id {
        hash: String,
        name: String,
    },
    AdHoc,
}

/// Sign `bin`. Outside CI a Mac must have a Developer ID (an ad-hoc dev
/// build would reset every user's macOS privacy grants); set
/// `RLORD_SIGN_IDENTITY=-` to sign ad-hoc on purpose.
fn sign(bin: &Path) -> Result<()> {
    let ci = std::env::var_os("CI").is_some_and(|v| !v.is_empty());
    let listing = Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let env = std::env::var("RLORD_SIGN_IDENTITY").ok();
    let signer = choose(env.as_deref(), &listing, ci)?;
    match &signer {
        Signer::Id { name, .. } => eprintln!("signing with {name}"),
        Signer::AdHoc => eprintln!("signing ad-hoc"),
    }
    cmd::run(Command::new("codesign").args(sign_args(&signer)).arg(bin)).context(
        "codesign failed. With a Developer ID it needs the network (secure \
         timestamp) and keychain access to the key (run once from a GUI \
         session and choose Always Allow). RLORD_SIGN_IDENTITY=- signs ad-hoc.",
    )
}

/// Pick the signer. `env` is `RLORD_SIGN_IDENTITY`: "-" for ad-hoc, a name
/// or hash to match, or empty/unset for the first "Developer ID
/// Application" in `listing` (`security find-identity -v` output). With
/// none found, CI signs ad-hoc and anywhere else it is an error.
fn choose(env: Option<&str>, listing: &str, ci: bool) -> Result<Signer> {
    let env = env.map(str::trim).filter(|v| !v.is_empty());
    if env == Some("-") {
        return Ok(Signer::AdHoc);
    }
    let ids = developer_ids(listing);
    let found = match env {
        Some(want) => ids.into_iter().find(|(h, n)| h == want || n == want),
        None => ids.into_iter().next(),
    };
    match (found, env) {
        (Some((hash, name)), _) => Ok(Signer::Id { hash, name }),
        (None, Some(want)) => {
            bail!("no valid Developer ID Application identity matches RLORD_SIGN_IDENTITY={want}")
        }
        (None, None) if ci => Ok(Signer::AdHoc),
        (None, None) => bail!(
            "no valid \"Developer ID Application\" identity in the keychain \
             (security find-identity -v -p codesigning). Set \
             RLORD_SIGN_IDENTITY=- to sign ad-hoc on purpose."
        ),
    }
}

/// `(hash, name)` of each "Developer ID Application" line in `security
/// find-identity -v` output, in order.
fn developer_ids(listing: &str) -> Vec<(String, String)> {
    listing
        .lines()
        .filter_map(|line| {
            let hash = line.split_whitespace().nth(1)?;
            let rest = &line[line.find('"')? + 1..];
            let name = &rest[..rest.find('"')?];
            (hash.len() == 40 && name.starts_with("Developer ID Application:"))
                .then(|| (hash.to_string(), name.to_string()))
        })
        .collect()
}

/// `codesign` arguments. No hardened runtime: only notarization needs it
/// (not done), and it would stop debuggers attaching.
fn sign_args(signer: &Signer) -> Vec<&str> {
    match signer {
        Signer::Id { hash, .. } => vec![
            "--force",
            "--sign",
            hash,
            "--timestamp",
            "--identifier",
            SIGN_ID,
        ],
        Signer::AdHoc => vec!["--force", "--sign", "-"],
    }
}

#[cfg(test)]
mod sign_tests {
    use super::*;

    const H1: &str = "0123456789ABCDEF0123456789ABCDEF01234567";
    const H2: &str = "89ABCDEF0123456789ABCDEF0123456789ABCDEF";
    const NAME: &str = "Developer ID Application: B A (PL2X8AJK8A)";

    fn listing() -> String {
        format!(
            "  1) {H1} \"Apple Development: X (T1)\"\n  2) {H2} \"{NAME}\"\n     2 valid identities found\n"
        )
    }

    #[test]
    fn picks_the_developer_id_by_hash() {
        let s = choose(None, &listing(), false).unwrap();
        assert_eq!(
            s,
            Signer::Id {
                hash: H2.into(),
                name: NAME.into()
            }
        );
        assert_eq!(
            sign_args(&s),
            [
                "--force",
                "--sign",
                H2,
                "--timestamp",
                "--identifier",
                SIGN_ID
            ]
        );
    }

    #[test]
    fn the_override() {
        assert_eq!(choose(Some("-"), &listing(), false).unwrap(), Signer::AdHoc);
        // Empty counts as unset: the keychain is searched.
        assert!(matches!(
            choose(Some(" "), &listing(), false).unwrap(),
            Signer::Id { .. }
        ));
        assert!(matches!(
            choose(Some(H2), &listing(), false).unwrap(),
            Signer::Id { .. }
        ));
        assert!(matches!(
            choose(Some(NAME), &listing(), false).unwrap(),
            Signer::Id { .. }
        ));
        assert!(choose(Some("nope"), &listing(), true).is_err());
    }

    #[test]
    fn no_identity_is_an_error_except_on_ci() {
        let none = "     0 valid identities found\n";
        assert_eq!(choose(None, none, true).unwrap(), Signer::AdHoc);
        assert!(choose(None, none, false).is_err());
        assert_eq!(sign_args(&Signer::AdHoc), ["--force", "--sign", "-"]);
    }
}
