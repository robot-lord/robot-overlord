//! `cargo xtask relay-image [--dist DIR] [--push]`: the relay's container
//! image (deploy/relay/Dockerfile) for linux/amd64 and linux/arm64, from
//! the release's own Linux relay tarballs. Nothing is compiled: each
//! platform's image holds the static binary from its tarball.
//!
//! The tarballs come from the GitHub release of the version in Cargo.toml
//! (`gh release download`), or from `--dist DIR` (local or workflow files).
//! Either way they are checked against the SHA256SUMS beside them first.
//!
//! Tags: `ghcr.io/robot-lord/rlord-relay:<version>`, and `:latest` for a
//! stable release only. Without `--push` it only builds (the result stays
//! in the build cache). With `--push` it pushes, prints the image's digest,
//! and writes `name` and `digest` to GITHUB_OUTPUT when that is set, for
//! the release workflow to attest. Log in to ghcr.io first.
//!
//! Needs `docker buildx`. It builds on its own docker-container builder
//! (made on first use), since Docker's default builder may not build for
//! two platforms at once.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::build::sha256;
use crate::cmd::{self, gh, TempDir};
use crate::version::{self, Version, LINUX, RELAY_IMAGE, REPO};

const USAGE: &str = "usage: cargo xtask relay-image [--dist DIR] [--push]";

/// The Dockerfile, used as it is in the checkout.
const DOCKERFILE: &str = "deploy/relay/Dockerfile";
/// The buildx builder this task makes and reuses.
const BUILDER: &str = "rlord-relay";

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    dist: Option<PathBuf>,
    push: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut o = Options::default();
        let mut args = args.iter();
        while let Some(a) = args.next() {
            match a.as_str() {
                "--push" => o.push = true,
                "--dist" => match args.next() {
                    Some(d) => o.dist = Some(PathBuf::from(d)),
                    None => return cmd::usage(USAGE),
                },
                _ => return cmd::usage(format!("unknown option {a}\n{USAGE}")),
            }
        }
        Ok(o)
    }
}

pub fn main(args: &[String], invoked_from: &Path) -> Result<()> {
    let opts = Options::parse(args)?;
    let ver = version::current()?;
    let tarballs = tarballs(&ver);

    // Under target/, not the system temp dir: colima only sees the home
    // folder.
    let scratch = TempDir::new_in(Path::new("target"), "relay-image")?;
    let context = scratch.path().join("context");
    std::fs::create_dir(&context)?;

    let from = match &opts.dist {
        // DIST is the caller's path, not relative to the repository root.
        Some(d) => invoked_from.join(d),
        None => {
            if ver.is_dev() {
                bail!(
                    "{} is a dev build, which has no Linux relay tarballs on GitHub; pass --dist DIR",
                    ver.tag()
                );
            }
            let dl = scratch.path().join("download");
            std::fs::create_dir(&dl)?;
            let mut c = gh();
            c.args(["release", "download", &ver.tag(), "-R", REPO, "-D"])
                .arg(&dl)
                .args(["-p", "SHA256SUMS"]);
            for t in &tarballs {
                c.args(["-p", t]);
            }
            cmd::run(&mut c)?;
            dl
        }
    };

    let sums_path = from.join("SHA256SUMS");
    let sums = std::fs::read_to_string(&sums_path)
        .with_context(|| format!("reading {}", sums_path.display()))?;
    for t in &tarballs {
        let file = from.join(t);
        if !file.is_file() {
            bail!("no {t} in {}", from.display());
        }
        check_sum(&sums, t, &sha256(&file)?)?;
        std::fs::copy(&file, context.join(t))?;
    }
    std::fs::copy(DOCKERFILE, context.join("Dockerfile"))?;

    if !cmd::succeeds(Command::new("docker").args(["buildx", "version"]))? {
        bail!("relay-image needs docker buildx");
    }
    if !cmd::succeeds(Command::new("docker").args(["buildx", "inspect", BUILDER]))? {
        cmd::run(Command::new("docker").args([
            "buildx",
            "create",
            "--name",
            BUILDER,
            "--driver",
            "docker-container",
        ]))?;
    }

    let metadata = scratch.path().join("metadata.json");
    cmd::run(Command::new("docker").args(buildx_args(&ver, &context, &metadata, opts.push)))?;
    if !opts.push {
        println!("built {} (not pushed)", ver.relay_image_tags().join(", "));
        return Ok(());
    }

    let json = std::fs::read_to_string(&metadata).context("reading buildx's metadata")?;
    let digest = digest(&json).context("buildx's metadata has no image digest")?;
    println!(
        "pushed {} ({RELAY_IMAGE}@{digest})",
        ver.relay_image_tags().join(", ")
    );
    if let Some(path) = std::env::var_os("GITHUB_OUTPUT").filter(|p| !p.is_empty()) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.to_string_lossy()))?;
        writeln!(file, "name={RELAY_IMAGE}")?;
        writeln!(file, "digest={digest}")?;
    }
    Ok(())
}

/// The relay tarballs the image is built from: one per Linux target.
fn tarballs(ver: &Version) -> Vec<String> {
    LINUX.iter().map(|t| ver.relay_tarball(*t)).collect()
}

/// An error unless SHA256SUMS lists `file` with checksum `actual`.
fn check_sum(sums: &str, file: &str, actual: &str) -> Result<()> {
    match version::sha256sums(sums)
        .into_iter()
        .find(|(_, f)| *f == file)
    {
        None => bail!("SHA256SUMS lacks {file}"),
        Some((want, _)) if !want.eq_ignore_ascii_case(actual) => {
            bail!("{file} doesn't match SHA256SUMS")
        }
        Some(_) => Ok(()),
    }
}

/// `docker buildx build` for both Linux platforms. Without `push` the
/// result stays in the build cache: a multi-platform image can't be loaded
/// into every Docker.
fn buildx_args(ver: &Version, context: &Path, metadata: &Path, push: bool) -> Vec<String> {
    let platforms: Vec<String> = LINUX.iter().map(|t| t.oci_platform()).collect();
    let mut a: Vec<String> = ["buildx", "build", "--builder", BUILDER, "--platform"]
        .map(String::from)
        .into();
    a.push(platforms.join(","));
    for tag in ver.relay_image_tags() {
        a.extend(["--tag".into(), tag]);
    }
    a.extend([
        "--label".into(),
        format!("org.opencontainers.image.version={ver}"),
        // The release workflow attests the pushed image with GitHub's own
        // provenance; buildx's would add an "unknown" platform per image.
        "--provenance=false".into(),
        "--metadata-file".into(),
        metadata.display().to_string(),
        if push {
            "--push"
        } else {
            "--output=type=cacheonly"
        }
        .into(),
        context.display().to_string(),
    ]);
    a
}

/// The pushed image's digest (`sha256:…`) from buildx's `--metadata-file`.
fn digest(metadata: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(metadata).ok()?;
    let d = v["containerimage.digest"].as_str()?;
    let hex = d.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| d.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn opts(args: &[&str]) -> Result<Options> {
        Options::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_options() {
        assert_eq!(opts(&[]).unwrap(), Options::default());
        assert_eq!(
            opts(&["--push", "--dist", "d"]).unwrap(),
            Options {
                dist: Some("d".into()),
                push: true
            }
        );
        assert!(opts(&["--dist"]).is_err());
        assert!(opts(&["--latest"]).is_err());
    }

    #[test]
    fn one_tarball_per_linux_platform() {
        assert_eq!(
            tarballs(&v("1.3.0")),
            [
                "rlord-relay-v1.3.0-x86_64-unknown-linux-musl.tar.gz",
                "rlord-relay-v1.3.0-aarch64-unknown-linux-musl.tar.gz",
            ]
        );
    }

    #[test]
    fn the_dockerfile_picks_each_platforms_tarball() {
        // Docker sets TARGETARCH to the platform's arch; the Dockerfile
        // maps it to the triple's, which names the tarball.
        let dockerfile = include_str!("../../deploy/relay/Dockerfile");
        for t in LINUX {
            let arch = t.triple.split('-').next().unwrap();
            let line = format!("{}) arch={arch} ;;", t.deb_arch);
            assert!(dockerfile.contains(&line), "Dockerfile lacks `{line}`");
        }
        assert!(dockerfile.contains("rlord-relay-v*-\"$arch\"-unknown-linux-musl.tar.gz"));
        for t in LINUX {
            assert!(t.triple.ends_with("-unknown-linux-musl"));
        }
    }

    #[test]
    fn checks_sums() {
        let sums = "AbC  a.tar.gz\ndef *b.tar.gz\n";
        assert!(check_sum(sums, "a.tar.gz", "abc").is_ok());
        assert!(check_sum(sums, "b.tar.gz", "def").is_ok());
        assert!(check_sum(sums, "b.tar.gz", "abc").is_err());
        assert!(check_sum(sums, "c.tar.gz", "abc").is_err());
    }

    #[test]
    fn builds_both_platforms_and_pushes_only_when_asked() {
        let ctx = Path::new("/ctx");
        let meta = Path::new("/m.json");
        let stable = buildx_args(&v("1.3.0"), ctx, meta, true);
        assert_eq!(
            stable,
            [
                "buildx",
                "build",
                "--builder",
                "rlord-relay",
                "--platform",
                "linux/amd64,linux/arm64",
                "--tag",
                "ghcr.io/robot-lord/rlord-relay:1.3.0",
                "--tag",
                "ghcr.io/robot-lord/rlord-relay:latest",
                "--label",
                "org.opencontainers.image.version=1.3.0",
                "--provenance=false",
                "--metadata-file",
                "/m.json",
                "--push",
                "/ctx",
            ]
        );
        let dev = buildx_args(&v("1.3.0-dev.4"), ctx, meta, false);
        assert!(!dev.iter().any(|a| a.ends_with(":latest")));
        assert!(!dev.iter().any(|a| a == "--push"));
        assert!(dev.iter().any(|a| a == "--output=type=cacheonly"));
    }

    #[test]
    fn reads_the_digest() {
        let d = format!("sha256:{}", "a".repeat(64));
        let json = format!(r#"{{"containerimage.digest": "{d}", "image.name": "x"}}"#);
        assert_eq!(digest(&json), Some(d));
        assert_eq!(digest(r#"{"containerimage.digest": "sha256:short"}"#), None);
        assert_eq!(digest("{}"), None);
        assert_eq!(digest("not json"), None);
    }
}
