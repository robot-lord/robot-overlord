//! `rlord web install` and `rlord web remove`: the opt-in for the web
//! plugin. Nothing else installs it. A download runs the compiled-in
//! `web-install.sh` (curl or gh, as `rlord update` does: this binary links
//! no network client); the tarball is checked against the release's
//! SHA256SUMS, unpacked, and the plugin inside is checked against the hash
//! built into this rlord (`plugin::verify`) before it is moved into place.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::json;

use rlord_core::config::Config;
use rlord_core::plugin;

/// The download glue, compiled in so installing needs no checkout.
const WEB_INSTALL_SH: &str = include_str!("../web-install.sh");

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Why this rlord can't install a plugin: it was built without a hash.
const NO_HASH: &str = "this rlord was built without a web plugin hash (a checkout's or a \
    crates.io build), so it can't install the plugin: release installs (get.sh, Homebrew, \
    .deb, .rpm) can. A checkout runs the rlord-web built beside it \
    (`cargo build -p robot-overlord-web`) or the one RLORD_WEB_BIN names.";

pub struct Install {
    pub yes: bool,
    pub if_installed: bool,
    pub from: Option<PathBuf>,
}

/// `rlord web install`.
pub fn install(opts: &Install) -> Result<()> {
    let dest = plugin::installed_path();
    let Some(expected) = plugin::install_hash() else {
        if opts.if_installed {
            // (update.sh on an install that can't have a plugin.)
            return Ok(());
        }
        bail!("{NO_HASH}");
    };
    if opts.if_installed && !dest.exists() {
        // Never opted in: nothing to do, and nothing said.
        return Ok(());
    }
    if plugin::verify(&dest, &expected).is_ok() {
        if !opts.if_installed {
            println!("rlord web: the plugin for rlord {VERSION} is already installed");
        }
        return Ok(());
    }
    let target = plugin::release_target().with_context(|| {
        format!(
            "there is no release web plugin for {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let stem = plugin::tarball_stem(VERSION, target);
    // Asked again only when this is the opt-in (an update of an installed
    // plugin was agreed to then).
    if !opts.yes && !opts.if_installed && !confirm(&stem, opts.from.as_deref())? {
        println!("not installed");
        return Ok(());
    }
    let dir = plugin::plugins_dir();
    rlord_core::private::dir(&dir)?;
    // Unpacked beside its final place, so the last step is one rename.
    let work = Work::new(&dir)?;
    let tarball = format!("{stem}.tar.gz");
    match &opts.from {
        Some(src) => {
            for f in [tarball.as_str(), "SHA256SUMS"] {
                std::fs::copy(src.join(f), work.0.join(f))
                    .with_context(|| format!("copying {} from {}", f, src.display()))?;
            }
        }
        None => {
            println!("downloading {tarball} (release v{VERSION})…");
            let st = Command::new("bash")
                .arg("-c")
                .arg(WEB_INSTALL_SH)
                .arg("rlord-web-install")
                .args([VERSION, target])
                .arg(&work.0)
                .status()
                .context("running bash")?;
            if !st.success() {
                bail!("downloading the web plugin failed");
            }
        }
    }
    check_sums(&work.0, &tarball)?;
    let st = Command::new("tar")
        .arg("-xzf")
        .arg(work.0.join(&tarball))
        .arg("-C")
        .arg(&work.0)
        .status()
        .context("running tar")?;
    if !st.success() {
        bail!("unpacking {tarball} failed");
    }
    let bin = work.0.join(&stem).join(plugin::BIN_NAME);
    plugin::verify(&bin, &expected).map_err(|e| match e {
        plugin::PluginError::Mismatch {
            found, expected, ..
        } => anyhow::anyhow!(
            "the downloaded plugin is not the one this rlord was built with \
             (sha256 {found}, expected {expected}): not installed"
        ),
        plugin::PluginError::NotInstalled => {
            anyhow::anyhow!("{tarball} has no {stem}/{}", plugin::BIN_NAME)
        }
        e => e.into(),
    })?;
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    std::fs::rename(&bin, &dest).with_context(|| format!("installing {}", dest.display()))?;
    println!("rlord web: installed {}", dest.display());
    if !opts.if_installed {
        println!("`rlord web` opens it; `rlord web remove` takes it away again");
    }
    Ok(())
}

/// Say what installing does and ask. Without a terminal: an error naming
/// `--yes`.
fn confirm(stem: &str, from: Option<&Path>) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("installing the web plugin needs a yes: run `rlord web install --yes`");
    }
    let port = Config::load().map(|c| c.web.port).unwrap_or_default();
    let source = match from {
        Some(d) => format!("copies {stem}.tar.gz from {}", d.display()),
        None => format!(
            "downloads {stem}.tar.gz from the GitHub release v{VERSION} \
             (github.com/robot-lord/robot-overlord)"
        ),
    };
    println!(
        "rlord's web UI is a separate plugin, rlord-web, that isn't installed.\n\
         Installing it {source}, checks it against the SHA-256 built into this rlord,\n\
         and puts it at {}.\n\
         It serves the web UI on 127.0.0.1 only (port {port}); nothing listens on other\n\
         addresses. `rlord web remove` undoes it.",
        plugin::installed_path().display()
    );
    print!("Install it? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// The tarball's line in SHA256SUMS matches it (a broken download, caught
/// before unpacking; the plugin's own hash is checked after).
fn check_sums(dir: &Path, tarball: &str) -> Result<()> {
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).context("reading SHA256SUMS")?;
    let want = sums
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let sum = f.next()?;
            let file = f.next()?;
            (file.trim_start_matches('*') == tarball).then_some(sum)
        })
        .next()
        .with_context(|| format!("SHA256SUMS doesn't list {tarball}"))?;
    let got = plugin::sha256_file(&dir.join(tarball))?;
    if !got.eq_ignore_ascii_case(want) {
        bail!("checksum mismatch for {tarball}: not installed");
    }
    Ok(())
}

/// A scratch folder under the plugins folder, removed when done.
struct Work(PathBuf);

impl Work {
    fn new(parent: &Path) -> Result<Work> {
        let p = parent.join(format!(".install-{}", plugin::random_hex(6)?));
        rlord_core::private::dir(&p)?;
        Ok(Work(p))
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `rlord web remove`: stop the plugin, delete it and the token, and turn
/// `[web] enabled` off.
pub fn remove() -> Result<()> {
    // Only a daemon that is already running (none is started for this).
    let _ = super::call_running("web.stop", json!({}));
    let mut gone = Vec::new();
    for p in [plugin::installed_path(), plugin::token_path()] {
        match std::fs::remove_file(&p) {
            Ok(()) => gone.push(p.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", p.display())),
        }
    }
    if Config::load().is_ok_and(|c| c.web.enabled) {
        Config::set_bool("web", "enabled", false)?;
        gone.push("[web] enabled (now false)".into());
    }
    // Remote access needs the plugin: off with it.
    if Config::load().is_ok_and(|c| c.remote.enabled) {
        Config::set_bool("remote", "enabled", false)?;
        gone.push("[remote] enabled (now false)".into());
    }
    // Its keys and paired devices: only the plugin could use them.
    let remote = plugin::remote_dir();
    if remote.exists() {
        if remove_remote_asked(&remote)? {
            std::fs::remove_dir_all(&remote)
                .with_context(|| format!("removing {}", remote.display()))?;
            gone.push(remote.display().to_string());
        } else {
            println!(
                "rlord web: kept {} (remote access's keys and paired devices)",
                remote.display()
            );
        }
    }
    if gone.is_empty() {
        println!("rlord web: nothing to remove");
    } else {
        println!("rlord web: removed {}", gone.join(", "));
    }
    Ok(())
}

/// Whether to delete remote access's keys and paired devices: asked on a
/// terminal; kept without one.
fn remove_remote_asked(dir: &Path) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    print!(
        "Also delete remote access's keys and paired devices ({})? Every device would \
         pair again. [y/N] ",
        dir.display()
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Before `rlord web` asks the daemon: with no plugin, opt in (asked on a
/// terminal; otherwise an error naming `install --yes`). A checkout's build
/// says how it finds one instead.
pub fn ensure_installed() -> Result<()> {
    let expected = plugin::install_hash();
    match plugin::locate_with(expected.as_deref()) {
        Ok(_) => Ok(()),
        Err(plugin::PluginError::NotInstalled) if expected.is_none() => bail!("{NO_HASH}"),
        Err(plugin::PluginError::NotInstalled) => {
            if !std::io::stdin().is_terminal() {
                bail!(
                    "the web plugin is not installed: run `rlord web install --yes` \
                     (it downloads rlord-web for this version)"
                );
            }
            install(&Install {
                yes: false,
                if_installed: false,
                from: None,
            })?;
            if plugin::locate_with(expected.as_deref()).is_err() {
                bail!("the web plugin is not installed");
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}
