//! `cargo xtask install-tools`: the Linux release build's packaging tools
//! (zig, cargo-zigbuild, cargo-deb, cargo-generate-rpm) at the versions
//! pinned here, installed into `target/release-tools`. The one place their
//! versions live: the release workflow and a local `cargo xtask release
//! --local` both install them with this task, and `build-release linux`
//! runs only these copies, refusing any other version.
//!
//! Prebuilt and checksum-pinned where the project ships a binary for the
//! host (zig, cargo-zigbuild; cargo-deb on x86_64 Linux); otherwise built
//! with `cargo install --locked`. Rust itself is pinned by
//! rust-toolchain.toml; cargo-deny comes inside CI's cargo-deny-action,
//! pinned by its commit.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::cmd::{self, cargo, TempDir};

/// Where the tools go, under the repository root.
pub const DIR: &str = "target/release-tools";

pub const ZIG: &str = "0.14.1";
pub const ZIGBUILD: &str = "0.24.1";
pub const DEB: &str = "3.8.0";
pub const GENERATE_RPM: &str = "0.21.1";

const USAGE: &str = "usage: cargo xtask install-tools";

/// A machine the release build runs on: CI's Linux runner, or the
/// maintainer's Mac for `--local`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    LinuxX86_64,
    LinuxAarch64,
    MacAarch64,
}

impl Host {
    pub fn from(os: &str, arch: &str) -> Option<Self> {
        match (os, arch) {
            ("linux", "x86_64") => Some(Self::LinuxX86_64),
            ("linux", "aarch64") => Some(Self::LinuxAarch64),
            ("macos", "aarch64") => Some(Self::MacAarch64),
            _ => None,
        }
    }

    fn current() -> Result<Self> {
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        Self::from(os, arch).with_context(|| {
            format!(
                "no release tools for {os}/{arch} (x86_64 or aarch64 Linux, Apple Silicon macOS)"
            )
        })
    }
}

/// A file to fetch and the SHA-256 it must have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub url: String,
    pub sha256: &'static str,
}

/// zig's tarball from ziglang.org (index.json lists the checksums).
pub fn zig(host: Host) -> Download {
    let sha256 = match host {
        Host::LinuxX86_64 => "24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c",
        Host::LinuxAarch64 => "f7a654acc967864f7a050ddacfaa778c7504a0eca8d2b678839c21eea47c992b",
        Host::MacAarch64 => "39f3dc5e79c22088ce878edc821dedb4ca5a1cd9f5ef915e9b3cc3053e8faefa",
    };
    Download {
        url: format!(
            "https://ziglang.org/download/{ZIG}/{}.tar.xz",
            zig_stem(zig_plat(host))
        ),
        sha256,
    }
}

/// The folder zig's tarball unpacks to (and the tarball's name without
/// `.tar.xz`).
fn zig_stem(plat: &str) -> String {
    format!("zig-{plat}-{ZIG}")
}

fn zig_plat(host: Host) -> &'static str {
    match host {
        Host::LinuxX86_64 => "x86_64-linux",
        Host::LinuxAarch64 => "aarch64-linux",
        Host::MacAarch64 => "aarch64-macos",
    }
}

/// cargo-zigbuild's release tarball (checksums: the GitHub release's
/// asset digests).
pub fn zigbuild(host: Host) -> Download {
    let (triple, sha256) = zigbuild_asset(host);
    Download {
        url: format!(
            "https://github.com/rust-cross/cargo-zigbuild/releases/download/v{ZIGBUILD}/cargo-zigbuild-{triple}.tar.xz"
        ),
        sha256,
    }
}

fn zigbuild_asset(host: Host) -> (&'static str, &'static str) {
    match host {
        Host::LinuxX86_64 => (
            "x86_64-unknown-linux-musl",
            "f09b180a517af0e4dd4bffc921bb88e5ab705452a9dd948c61e489b3020c68c2",
        ),
        Host::LinuxAarch64 => (
            "aarch64-unknown-linux-gnu",
            "70bb4c9cc71a6d16211b9079996e3cd588671053312adeff4d043499ed887601",
        ),
        Host::MacAarch64 => (
            "aarch64-apple-darwin",
            "e38d2a30aa8f4c27bf9e3ced266aae1f47e4dbd6be939852ec07d8662eec408e",
        ),
    }
}

/// cargo-deb's release ships only an amd64 `.deb`; elsewhere it is built.
pub fn deb(host: Host) -> Option<Download> {
    (host == Host::LinuxX86_64).then(|| Download {
        url: format!(
            "https://github.com/kornelski/cargo-deb/releases/download/v{DEB}/cargo-deb_{DEB}-1_amd64.deb"
        ),
        sha256: "c6bd0a4affaa232814cc615b2597457f1d44fdfe1c8ebd643f3a491a1f244040",
    })
}

/// One tool as the build runs it: its file in `bin/`, how to ask its
/// version, and the version it must report.
struct Tool {
    bin: &'static str,
    version_args: &'static [&'static str],
    version: &'static str,
}

const TOOLS: [Tool; 4] = [
    Tool {
        bin: "zig",
        version_args: &["version"],
        version: ZIG,
    },
    Tool {
        bin: "cargo-zigbuild",
        version_args: &["--version"],
        version: ZIGBUILD,
    },
    Tool {
        bin: "cargo-deb",
        version_args: &["--version"],
        version: DEB,
    },
    Tool {
        bin: "cargo-generate-rpm",
        version_args: &["--version"],
        version: GENERATE_RPM,
    },
];

/// Whether a tool's `--version` output names exactly this version (as a
/// word, optionally `v`-prefixed: 0.24.1 is not 0.24.10).
pub fn reports_version(output: &str, version: &str) -> bool {
    output
        .split_whitespace()
        .any(|w| w.strip_prefix('v').unwrap_or(w) == version)
}

/// The first field of `sha256sum`/`shasum -a 256` output, lowercased.
pub fn parse_sha256(output: &str) -> Option<String> {
    let sum = output.split_whitespace().next()?;
    (sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit())).then(|| sum.to_lowercase())
}

/// `dir` first, then the current PATH.
pub fn path_with(dir: &Path) -> Result<OsString> {
    let rest = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&rest));
    std::env::join_paths(dirs).context("building PATH")
}

/// The tools' `bin/` folder (absolute), once every tool in it reports its
/// pinned version.
pub fn ready() -> Result<PathBuf> {
    let bin = std::path::absolute(Path::new(DIR).join("bin"))?;
    let wrong: Vec<String> = TOOLS
        .iter()
        .filter(|t| !installed(&bin, t))
        .map(|t| format!("{} {}", t.bin, t.version))
        .collect();
    if !wrong.is_empty() {
        bail!(
            "the Linux release build needs {} in {}: run cargo xtask install-tools",
            wrong.join(", "),
            bin.display()
        );
    }
    Ok(bin)
}

fn installed(bin: &Path, tool: &Tool) -> bool {
    let path = bin.join(tool.bin);
    if !path.is_file() {
        return false;
    }
    let Ok(path_env) = path_with(bin) else {
        return false;
    };
    // Not cmd::output: a missing or broken tool is an answer, not an error.
    Command::new(&path)
        .args(tool.version_args)
        .env("PATH", path_env)
        .output()
        .is_ok_and(|o| {
            o.status.success() && reports_version(&String::from_utf8_lossy(&o.stdout), tool.version)
        })
}

pub fn main(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        return cmd::usage(USAGE);
    }
    let host = Host::current()?;
    let root = std::path::absolute(DIR)?;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
    let scratch = TempDir::new_in(Path::new("target"), "tools")?;
    let scratch = scratch.path();

    for tool in &TOOLS {
        if installed(&bin, tool) {
            println!("{} {}: already installed", tool.bin, tool.version);
            continue;
        }
        println!("{} {}: installing", tool.bin, tool.version);
        match tool.bin {
            "zig" => {
                let tarball = fetch(&zig(host), scratch)?;
                let stem = zig_stem(zig_plat(host));
                let _ = std::fs::remove_dir_all(root.join(&stem));
                untar(&tarball, &root)?;
                // A symlink: zig finds its lib/ next to its real path.
                let link = bin.join("zig");
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(root.join(&stem).join("zig"), &link)?;
            }
            "cargo-zigbuild" => {
                let tarball = fetch(&zigbuild(host), scratch)?;
                untar(&tarball, scratch)?;
                let (triple, _) = zigbuild_asset(host);
                copy_exe(
                    &scratch.join(format!("cargo-zigbuild-{triple}/cargo-zigbuild")),
                    &bin,
                )?;
            }
            "cargo-deb" => match deb(host) {
                Some(d) => {
                    let pkg = fetch(&d, scratch)?;
                    // A .deb is an ar archive; the files are in data.tar.xz.
                    cmd::run(
                        Command::new("ar")
                            .arg("x")
                            .arg(&pkg)
                            .arg("data.tar.xz")
                            .current_dir(scratch),
                    )?;
                    untar(&scratch.join("data.tar.xz"), scratch)?;
                    copy_exe(&scratch.join("usr/bin/cargo-deb"), &bin)?;
                }
                None => cargo_install("cargo-deb", DEB, &root)?,
            },
            "cargo-generate-rpm" => cargo_install("cargo-generate-rpm", GENERATE_RPM, &root)?,
            other => bail!("no installer for {other}"),
        }
        if !installed(&bin, tool) {
            bail!(
                "{} doesn't report version {} after installing",
                tool.bin,
                tool.version
            );
        }
    }
    println!("release tools ready in {}", bin.display());
    Ok(())
}

/// Download into `dir` and check the checksum; the file's path.
fn fetch(d: &Download, dir: &Path) -> Result<PathBuf> {
    let name = d.url.rsplit('/').next().context("URL has no file name")?;
    let to = dir.join(name);
    cmd::run(
        Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&to)
            .arg(&d.url),
    )?;
    let got = sha256(&to)?;
    if got != d.sha256 {
        bail!("{name}: SHA-256 is {got}, expected {}", d.sha256);
    }
    Ok(to)
}

fn sha256(file: &Path) -> Result<String> {
    let mut c = if cmd::on_path("sha256sum") {
        Command::new("sha256sum")
    } else {
        let mut c = Command::new("shasum");
        c.args(["-a", "256"]);
        c
    };
    let out = cmd::output(c.arg("--").arg(file))?;
    parse_sha256(&out).with_context(|| format!("unexpected checksum output: {out}"))
}

fn untar(tarball: &Path, into: &Path) -> Result<()> {
    cmd::run(
        Command::new("tar")
            .arg("-xJf")
            .arg(tarball)
            .arg("-C")
            .arg(into),
    )
}

fn copy_exe(from: &Path, bin: &Path) -> Result<()> {
    let to = bin.join(from.file_name().context("file name")?);
    // Remove first: copying over a running binary fails on Linux.
    let _ = std::fs::remove_file(&to);
    std::fs::copy(from, &to).with_context(|| format!("copying {}", from.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn cargo_install(krate: &str, version: &str, root: &Path) -> Result<()> {
    cmd::run(
        cargo()
            .args(["install", "--locked", "--force", "--root"])
            .arg(root)
            .arg(format!("{krate}@{version}")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_version_as_a_word() {
        assert!(reports_version("0.14.1\n", "0.14.1"));
        assert!(reports_version("cargo-zigbuild 0.24.1", "0.24.1"));
        assert!(reports_version("cargo-deb v3.8.0\n", "3.8.0"));
        assert!(!reports_version("cargo-zigbuild 0.24.10", "0.24.1"));
        assert!(!reports_version("cargo-deb 3.8.0-rc.1", "3.8.0"));
        assert!(!reports_version("", "3.8.0"));
    }

    #[test]
    fn parses_checksum_output() {
        let sum = "24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c";
        assert_eq!(
            parse_sha256(&format!("{sum}  /tmp/x.tar.xz")).as_deref(),
            Some(sum)
        );
        assert_eq!(
            parse_sha256(&format!("{}  x", sum.to_uppercase())).as_deref(),
            Some(sum)
        );
        assert_eq!(parse_sha256("deadbeef  x"), None);
        assert_eq!(parse_sha256(""), None);
    }

    #[test]
    fn knows_the_hosts() {
        assert_eq!(Host::from("linux", "x86_64"), Some(Host::LinuxX86_64));
        assert_eq!(Host::from("linux", "aarch64"), Some(Host::LinuxAarch64));
        assert_eq!(Host::from("macos", "aarch64"), Some(Host::MacAarch64));
        assert_eq!(Host::from("macos", "x86_64"), None);
    }

    #[test]
    fn downloads_name_the_pinned_versions() {
        for host in [Host::LinuxX86_64, Host::LinuxAarch64, Host::MacAarch64] {
            let z = zig(host);
            assert!(z
                .url
                .starts_with("https://ziglang.org/download/0.14.1/zig-"));
            assert!(z
                .url
                .ends_with(&format!("{}.tar.xz", zig_stem(zig_plat(host)))));
            assert!(zigbuild(host).url.contains(&format!("/v{ZIGBUILD}/")));
            for sha in [z.sha256, zigbuild(host).sha256] {
                assert_eq!(parse_sha256(sha).as_deref(), Some(sha));
            }
        }
        assert_eq!(
            zig(Host::LinuxX86_64).url,
            "https://ziglang.org/download/0.14.1/zig-x86_64-linux-0.14.1.tar.xz"
        );
        assert!(deb(Host::LinuxX86_64).is_some());
        assert!(deb(Host::MacAarch64).is_none());
    }

    #[test]
    fn puts_the_tools_first_on_path() {
        let p = path_with(Path::new("/opt/tools/bin")).unwrap();
        let first = std::env::split_paths(&p).next().unwrap();
        assert_eq!(first, Path::new("/opt/tools/bin"));
    }
}
