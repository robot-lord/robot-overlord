//! The opt-in web plugin (`rlord-web`): where it lives, how rlord checks
//! it is the build it was made with, and the protocol number both sides
//! agree on. No network code: the plugin holds all of that
//! (`docs/working/web-ui-remote.md`, "Shape: two executables").

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::paths;

/// Bumped whenever a `web.*` daemon method the plugin uses changes shape.
/// The plugin's `web.hello` carries it; the daemon refuses another one.
pub const WEB_PROTOCOL: u32 = 1;

/// The plugin's executable name.
pub const BIN_NAME: &str = "rlord-web";

/// Exit code of a plugin the daemon refused (another protocol): not
/// retried.
pub const EXIT_INCOMPATIBLE: i32 = 3;
/// Exit code of a plugin whose port is taken: not retried.
pub const EXIT_PORT_IN_USE: i32 = 4;

/// The daemon's RPC error code for a `web.hello` with another protocol.
pub const RPC_ERR_INCOMPATIBLE: i32 = -32010;
/// The daemon's RPC error code for a `web.call` op that isn't on its list.
pub const RPC_ERR_NOT_ALLOWED: i32 = -32011;

/// The SHA-256 (hex) of the plugin this `rlord` was released with, set by
/// the release build (`RLORD_WEB_SHA256`). None in a checkout's build or a
/// crates.io install: those never download a plugin.
pub const COMPILED_SHA256: Option<&str> = option_env!("RLORD_WEB_SHA256");

/// Where rlord keeps plugins: `<data dir>/plugins` (0700).
pub fn plugins_dir() -> PathBuf {
    paths::data_dir().join("plugins")
}

/// Where an installed web plugin lives.
pub fn installed_path() -> PathBuf {
    plugins_dir().join(BIN_NAME)
}

/// The web plugin's token file (0600, made by the plugin, read by the CLI).
pub fn token_path() -> PathBuf {
    paths::data_dir().join("web.token")
}

/// Where the daemon appends the plugin's output.
pub fn log_path() -> PathBuf {
    paths::data_dir().join("web.log")
}

/// SHA-256 of a file, lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `n` random bytes from the system, as hex.
pub fn random_hex(n: usize) -> Result<String> {
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .context("reading /dev/urandom")?;
    Ok(hex(&b))
}

/// Why a plugin can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    #[error("the web plugin is not installed (run `rlord web install`)")]
    NotInstalled,
    #[error(
        "the web plugin at {path} is not the one this rlord was built with \
         (sha256 {found}, expected {expected}); run `rlord web install`"
    )]
    Mismatch {
        path: String,
        found: String,
        expected: String,
    },
    #[error("reading the web plugin at {path}: {why}")]
    Unreadable { path: String, why: String },
}

/// Check that `path` is the plugin with SHA-256 `expected` (hex).
pub fn verify(path: &Path, expected: &str) -> std::result::Result<(), PluginError> {
    if !path.is_file() {
        return Err(PluginError::NotInstalled);
    }
    let found = sha256_file(path).map_err(|e| PluginError::Unreadable {
        path: path.display().to_string(),
        why: format!("{e:#}"),
    })?;
    if !found.eq_ignore_ascii_case(expected.trim()) {
        return Err(PluginError::Mismatch {
            path: path.display().to_string(),
            found,
            expected: expected.trim().to_lowercase(),
        });
    }
    Ok(())
}

/// A plugin executable rlord may start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    pub path: PathBuf,
    /// Checked against the compiled-in hash. False for a build without
    /// one (a checkout's), which says so.
    pub verified: bool,
}

/// The plugin to run, by `COMPILED_SHA256`.
pub fn locate() -> std::result::Result<Plugin, PluginError> {
    locate_with(COMPILED_SHA256)
}

/// The plugin to run when the expected hash is `expected`:
/// - a hash: the installed plugin only, and only if it matches;
/// - none (a checkout's build): `RLORD_WEB_BIN`, else an `rlord-web` beside
///   this executable (`target/debug/`), unverified.
pub fn locate_with(expected: Option<&str>) -> std::result::Result<Plugin, PluginError> {
    if let Some(h) = expected.filter(|h| !h.trim().is_empty()) {
        let path = installed_path();
        verify(&path, h)?;
        return Ok(Plugin {
            path,
            verified: true,
        });
    }
    if let Some(p) = std::env::var_os("RLORD_WEB_BIN").filter(|p| !p.is_empty()) {
        let path = PathBuf::from(p);
        return if path.is_file() {
            Ok(Plugin {
                path,
                verified: false,
            })
        } else {
            Err(PluginError::NotInstalled)
        };
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(BIN_NAME)))
        .filter(|p| p.is_file());
    match beside {
        Some(path) => Ok(Plugin {
            path,
            verified: false,
        }),
        None => Err(PluginError::NotInstalled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_takes_the_right_hash_only() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join(BIN_NAME);
        std::fs::write(&p, b"abc").unwrap();
        // sha256("abc")
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(sha256_file(&p).unwrap(), abc);
        assert_eq!(verify(&p, abc), Ok(()));
        assert_eq!(verify(&p, &abc.to_uppercase()), Ok(()));
        let wrong = "00".repeat(32);
        match verify(&p, &wrong) {
            Err(PluginError::Mismatch { found, .. }) => assert_eq!(found, abc),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            verify(&t.path().join("missing"), abc),
            Err(PluginError::NotInstalled)
        );
    }

    #[test]
    fn plugin_paths_are_under_the_data_dir() {
        let data = paths::data_dir();
        assert_eq!(installed_path(), data.join("plugins").join("rlord-web"));
        assert_eq!(token_path(), data.join("web.token"));
        assert_eq!(log_path(), data.join("web.log"));
    }

    #[test]
    fn random_hex_is_random() {
        let a = random_hex(32).unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, random_hex(32).unwrap());
    }
}
