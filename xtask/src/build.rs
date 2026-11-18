//! `cargo xtask build-release macos|linux DIST`: build rlord's release
//! files for one platform. The one place the release build commands live:
//! the release workflow and `cargo xtask release` both run it, so CI and a
//! local build can't drift.
//!
//! Linux needs the musl targets (rust-toolchain.toml installs them) and the
//! packaging tools at their pinned versions (`cargo xtask install-tools`,
//! xtask/src/tools.rs). Writes no SHA256SUMS: the caller sums the full set
//! of files.
//!
//! Each target also gets the opt-in web plugin, `rlord-web`, in its own
//! tarball, and `rlord` is built with that plugin's SHA-256 compiled in
//! (`RLORD_WEB_SHA256`), so it only ever installs or starts that exact
//! file. Order per target: the plugin (with the page embedded), signed on
//! macOS (the same identity as `rlord`, see [`sign`]), hashed; then
//! `rlord` with the hash. Nothing may touch the plugin
//! after it is hashed. The page is built once, before any target
//! (`cargo xtask web build`), or taken from `RLORD_WEB_DIST` (the release
//! workflow builds it in its own job, so Node stays off the build jobs).
//!
//! Linux targets also get the relay server, `rlord-relay`, in its own
//! tarball with an example config and a systemd unit. It is not a plugin
//! of `rlord`, so no hash of it is compiled in anywhere.

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
    // No network code in the rlord binary (the web plugin holds it).
    crate::check_deps::check()?;
    web_page()?;
    let stage = TempDir::new_in(Path::new("target"), "pack")?;
    let target_dir = target_dir()?;

    match platform {
        Platform::Macos => {
            if !is_apple_silicon_mac() {
                bail!("build macOS on Apple Silicon");
            }
            let plugin = plugin_binary(&target_dir, MAC);
            cmd::run(
                cargo()
                    .arg("build")
                    .args(plugin_args(MAC))
                    .env("RLORD_WEB_REQUIRE_ASSETS", "1"),
            )?;
            // Signed like rlord (its own identifier) before it is hashed,
            // and never touched after: the hash is of the signed file.
            sign(&plugin, WEB_SIGN_ID, !ver.is_dev())?;
            cmd::run(Command::new("codesign").arg("--verify").arg(&plugin))?;
            let hash = sha256(&plugin)?;
            cmd::run(
                cargo()
                    .arg("build")
                    .args(build_args(MAC))
                    .env("RLORD_WEB_SHA256", &hash),
            )?;
            let bin = binary(&target_dir, MAC);
            // The linker ad-hoc signs, but post-link strip can leave the
            // signature stale; re-sign so the binary runs on other Macs
            // (arm64 refuses unsigned code). With a Developer ID in the
            // keychain the signature keeps one identity across builds, so
            // macOS privacy grants (folder access) survive an update.
            sign(&bin, SIGN_ID, !ver.is_dev())?;
            cmd::run(Command::new("codesign").arg("--verify").arg(&bin))?;
            let reported = cmd::output(Command::new(&bin).arg("--version"))?;
            if reported != format!("rlord {ver}") {
                bail!("built binary reports the wrong version ({reported})");
            }
            pack(&ver, MAC, &bin, stage.path(), &dist)?;
            pack_web(&ver, MAC, &plugin, stage.path(), &dist)?;
            check_install(&ver, MAC, &bin, &dist)?;
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
                let plugin = plugin_binary(&target_dir, t);
                cmd::run(
                    tool("zigbuild")
                        .args(plugin_args(t))
                        .env("RLORD_WEB_REQUIRE_ASSETS", "1"),
                )?;
                let hash = sha256(&plugin)?;
                cmd::run(
                    tool("zigbuild")
                        .args(build_args(t))
                        .env("RLORD_WEB_SHA256", &hash),
                )?;
                let bin = binary(&target_dir, t);
                pack(&ver, t, &bin, stage.path(), &dist)?;
                pack_web(&ver, t, &plugin, stage.path(), &dist)?;
                if is_native_linux(t) {
                    check_install(&ver, t, &bin, &dist)?;
                }
                // Its own invocation, like the plugin's, so its
                // dependencies' features don't unify into rlord's.
                cmd::run(tool("zigbuild").args(relay_args(t)))?;
                let relay = relay_binary(&target_dir, t);
                if is_native_linux(t) {
                    let reported = cmd::output(Command::new(&relay).arg("--version"))?;
                    if reported != format!("rlord-relay {ver}") {
                        bail!("built rlord-relay reports the wrong version ({reported})");
                    }
                }
                pack_relay(&ver, t, &relay, stage.path(), &dist)?;
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

/// The web page, ready to embed: copied from `RLORD_WEB_DIST` (built by
/// the workflow's own job), else built here.
fn web_page() -> Result<()> {
    let dist = Path::new(WEB_DIST);
    match std::env::var_os("RLORD_WEB_DIST").filter(|d| !d.is_empty()) {
        Some(from) => {
            let from = PathBuf::from(from);
            if !from.join("index.html").is_file() {
                bail!("RLORD_WEB_DIST={} holds no index.html", from.display());
            }
            if dist.exists() {
                std::fs::remove_dir_all(dist)?;
            }
            std::fs::create_dir_all(dist)?;
            // (`from/.`: its contents, hidden files too.)
            cmd::run(Command::new("cp").arg("-R").arg(from.join(".")).arg(dist))?;
        }
        None => crate::web::main(&["build".to_string()])?,
    }
    for f in ["index.html", WEB_LICENSES] {
        if !dist.join(f).is_file() {
            bail!("the web page has no {f} ({WEB_DIST})");
        }
    }
    Ok(())
}

/// Where `cargo xtask web build` puts the page; `rlord-web` embeds it.
const WEB_DIST: &str = "crates/web/ui/dist";
/// The bundled npm packages' licenses, written by the page's build.
const WEB_LICENSES: &str = "THIRD-PARTY-LICENSES.md";

/// SHA-256 of a file, lowercase hex.
pub fn sha256(path: &Path) -> Result<String> {
    let out = cmd::output(Command::new("shasum").args(["-a", "256", "--"]).arg(path))?;
    let sum = out.split_whitespace().next().unwrap_or("");
    if sum.len() != 64 || !sum.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("shasum printed no SHA-256 for {}", path.display());
    }
    Ok(sum.to_lowercase())
}

/// Whether this machine runs `target`'s Linux binaries.
fn is_native_linux(target: Target) -> bool {
    std::env::consts::OS == "linux"
        && target
            .triple
            .starts_with(&format!("{}-", std::env::consts::ARCH))
}

/// The coupling the release depends on, checked before anything ships:
/// the `rlord` just built installs the plugin just packed (`rlord web
/// install --from`, which checks it against the compiled-in hash), in a
/// scratch home.
fn check_install(ver: &Version, target: Target, rlord: &Path, dist: &Path) -> Result<()> {
    let home = TempDir::new_in(Path::new("target"), "web-install-check")?;
    let home = home.path().canonicalize()?;
    let from = home.join("release");
    std::fs::create_dir(&from)?;
    let tarball = ver.web_tarball(target);
    std::fs::copy(dist.join(&tarball), from.join(&tarball))?;
    std::fs::write(
        from.join("SHA256SUMS"),
        format!("{}  {tarball}\n", sha256(&from.join(&tarball))?),
    )?;
    let out = cmd::output(
        Command::new(rlord)
            .args(["web", "install", "--yes", "--from"])
            .arg(&from)
            .env("HOME", &home)
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_RUNTIME_DIR", home.join("run"))
            .env_remove("RLORD_WEB_BIN")
            .stdin(Stdio::null()),
    )
    .with_context(|| format!("the built rlord refuses its own web plugin ({tarball})"))?;
    if !out.contains("installed") {
        bail!("`rlord web install` of the built plugin said: {out}");
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

/// The web plugin's build: its own package and invocation, so its
/// dependencies' features never unify into `rlord`'s.
fn plugin_args(target: Target) -> [&'static str; 6] {
    [
        "--release",
        "--locked",
        "-p",
        "robot-overlord-web",
        "--target",
        target.triple,
    ]
}

fn plugin_binary(target_dir: &Path, target: Target) -> PathBuf {
    target_dir.join(target.triple).join("release/rlord-web")
}

const RELAY_PACKAGE: &str = "robot-overlord-relay";
/// What a self-hoster installs beside the binary.
const RELAY_EXAMPLE: &str = "deploy/relay/relay.toml.example";
const RELAY_UNIT: &str = "deploy/relay/rlord-relay.service";

/// The relay server's build (Linux only).
fn relay_args(target: Target) -> [&'static str; 6] {
    [
        "--release",
        "--locked",
        "-p",
        RELAY_PACKAGE,
        "--target",
        target.triple,
    ]
}

fn relay_binary(target_dir: &Path, target: Target) -> PathBuf {
    target_dir.join(target.triple).join("release/rlord-relay")
}

/// The relay's tarball: the binary, both licenses, the example config and
/// the systemd unit.
fn pack_relay(ver: &Version, target: Target, bin: &Path, stage: &Path, dist: &Path) -> Result<()> {
    tar(
        &ver.relay_tarball_stem(target),
        &[
            bin,
            Path::new("LICENSE-MIT"),
            Path::new("LICENSE-APACHE"),
            Path::new(RELAY_EXAMPLE),
            Path::new(RELAY_UNIT),
        ],
        stage,
        &dist.join(ver.relay_tarball(target)),
    )
}

/// The plugin's tarball: the binary, both licenses, and the bundled npm
/// packages' licenses. No installer: `rlord web install` puts it in place.
fn pack_web(ver: &Version, target: Target, bin: &Path, stage: &Path, dist: &Path) -> Result<()> {
    let licenses = Path::new(WEB_DIST).join(WEB_LICENSES);
    tar(
        &ver.web_tarball_stem(target),
        &[
            bin,
            &licenses,
            Path::new("LICENSE-MIT"),
            Path::new("LICENSE-APACHE"),
        ],
        stage,
        &dist.join(ver.web_tarball(target)),
    )
}

/// One tarball per target: the binary, the installer, the licenses.
fn pack(ver: &Version, target: Target, bin: &Path, stage: &Path, dist: &Path) -> Result<()> {
    tar(
        &ver.tarball_stem(target),
        &[
            bin,
            Path::new("scripts/install.sh"),
            Path::new("LICENSE-MIT"),
            Path::new("LICENSE-APACHE"),
            Path::new("README.md"),
        ],
        stage,
        &dist.join(ver.tarball(target)),
    )
}

/// `out`: a tarball of `files` in a top-level folder `name`.
fn tar(name: &str, files: &[&Path], stage: &Path, out: &Path) -> Result<()> {
    let dir = stage.join(name);
    std::fs::create_dir(&dir)?;
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
            .arg(out)
            .arg("-C")
            .arg(stage)
            .arg(name),
    )
}

/// The identifier a Developer ID signature carries. Fixed, so macOS sees
/// every build as the same program.
const SIGN_ID: &str = "com.robot-lord.rlord";
/// The web plugin's (`rlord-web`): its own program to macOS.
const WEB_SIGN_ID: &str = "com.robot-lord.rlord-web";

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

/// Sign `bin` with signing identifier `id`. Outside CI a Mac must have a Developer ID (an ad-hoc dev
/// build would reset every user's macOS privacy grants); set
/// `RLORD_SIGN_IDENTITY=-` to sign ad-hoc on purpose. In CI see
/// [`ci_adhoc_allowed`].
fn sign(bin: &Path, id: &str, stable: bool) -> Result<()> {
    let ci = std::env::var_os("CI").is_some_and(|v| !v.is_empty());
    let release = std::env::var("RLORD_RELEASE").ok();
    let ci = ci.then(|| ci_adhoc_allowed(release.as_deref(), stable));
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
        Signer::Id { name, .. } => eprintln!("signing {id} with {name}"),
        Signer::AdHoc if ci.is_some() => eprintln!(
            "warning: signing ad-hoc: no Developer ID in the keychain. A \
             published release would refuse this (RLORD_RELEASE=1)."
        ),
        Signer::AdHoc => eprintln!("signing {id} ad-hoc"),
    }
    cmd::run(
        Command::new("codesign")
            .args(sign_args(&signer, id))
            .arg(bin),
    )
    .context(
        "codesign failed. With a Developer ID it needs the network (secure \
         timestamp) and keychain access to the key (run once from a GUI \
         session and choose Always Allow). RLORD_SIGN_IDENTITY=- signs ad-hoc.",
    )
}

/// Pick the signer. `env` is `RLORD_SIGN_IDENTITY`: "-" for ad-hoc, a name
/// or hash to match, or empty/unset for the first "Developer ID
/// Application" in `listing` (`security find-identity -v` output). `ci` is
/// `None` outside CI, else whether CI may fall back to ad-hoc. With none
/// found, that fallback signs ad-hoc and anything else is an error.
fn choose(env: Option<&str>, listing: &str, ci: Option<bool>) -> Result<Signer> {
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
        (None, None) if ci == Some(true) => Ok(Signer::AdHoc),
        (None, None) if ci.is_some() => bail!(
            "no \"Developer ID Application\" identity in CI's keychain, and \
             this build is a release: it must not ship ad-hoc. Check the \
             MACOS_SIGN_P12 secrets in the `release` environment."
        ),
        (None, None) => bail!(
            "no valid \"Developer ID Application\" identity in the keychain \
             (security find-identity -v -p codesigning). Set \
             RLORD_SIGN_IDENTITY=- to sign ad-hoc on purpose."
        ),
    }
}

/// Whether CI may sign ad-hoc when it has no Developer ID. `release` is
/// `RLORD_RELEASE`, set by release.yml: "1" for a run that publishes,
/// "dry-run" for one that doesn't. A publishing run never may; a dry run
/// may. Elsewhere in CI (no `RLORD_RELEASE`), only a dev version may: a
/// stable version built ad-hoc must come from a release dry run.
fn ci_adhoc_allowed(release: Option<&str>, stable: bool) -> bool {
    match release.map(str::trim) {
        Some("1") => false,
        Some("dry-run") => true,
        _ => !stable,
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
/// (not done), and it would stop debuggers attaching. `id` is the
/// signing identifier (a Developer ID signature only).
fn sign_args<'a>(signer: &'a Signer, id: &'a str) -> Vec<&'a str> {
    match signer {
        Signer::Id { hash, .. } => {
            vec!["--force", "--sign", hash, "--timestamp", "--identifier", id]
        }
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
        let s = choose(None, &listing(), None).unwrap();
        assert_eq!(
            s,
            Signer::Id {
                hash: H2.into(),
                name: NAME.into()
            }
        );
        assert_eq!(
            sign_args(&s, SIGN_ID),
            [
                "--force",
                "--sign",
                H2,
                "--timestamp",
                "--identifier",
                SIGN_ID
            ]
        );
        // The web plugin: same identity, its own identifier.
        assert_eq!(sign_args(&s, WEB_SIGN_ID)[5], "com.robot-lord.rlord-web");
    }

    #[test]
    fn the_override() {
        assert_eq!(choose(Some("-"), &listing(), None).unwrap(), Signer::AdHoc);
        // Empty counts as unset: the keychain is searched.
        assert!(matches!(
            choose(Some(" "), &listing(), None).unwrap(),
            Signer::Id { .. }
        ));
        assert!(matches!(
            choose(Some(H2), &listing(), None).unwrap(),
            Signer::Id { .. }
        ));
        assert!(matches!(
            choose(Some(NAME), &listing(), None).unwrap(),
            Signer::Id { .. }
        ));
        assert!(choose(Some("nope"), &listing(), Some(true)).is_err());
    }

    #[test]
    fn no_identity_is_an_error_except_on_ci() {
        let none = "     0 valid identities found\n";
        assert_eq!(choose(None, none, Some(true)).unwrap(), Signer::AdHoc);
        assert!(choose(None, none, None).is_err());
        assert_eq!(
            sign_args(&Signer::AdHoc, SIGN_ID),
            ["--force", "--sign", "-"]
        );
    }

    #[test]
    fn ci_adhoc_only_off_the_publishing_path() {
        // A publishing run: never, dev or stable.
        assert!(!ci_adhoc_allowed(Some("1"), true));
        assert!(!ci_adhoc_allowed(Some("1"), false));
        // A release dry run: yes, so it works before the secret exists.
        assert!(ci_adhoc_allowed(Some("dry-run"), true));
        // Other CI: dev versions only.
        assert!(ci_adhoc_allowed(None, false));
        assert!(ci_adhoc_allowed(Some(""), false));
        assert!(!ci_adhoc_allowed(None, true));
        assert!(!ci_adhoc_allowed(Some("yes"), true));
    }
}
