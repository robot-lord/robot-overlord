//! The web token: 32 random bytes, hex, in `<data dir>/web.token` (0600).
//! Made by the plugin when missing and kept across restarts, so an open
//! page reconnects; `rlord web new-token` deletes it. The CLI only reads
//! it, to print the URL.

use std::path::Path;

use anyhow::{Context, Result};
use subtle::ConstantTimeEq;

/// The token in `path`, made first if it is missing or not a token.
pub fn load_or_create(path: &Path) -> Result<String> {
    if let Ok(t) = std::fs::read_to_string(path) {
        let t = t.trim();
        if valid(t) {
            return Ok(t.to_string());
        }
    }
    let t = rlord_core::plugin::random_hex(32)?;
    if let Some(dir) = path.parent() {
        rlord_core::private::dir(dir)?;
    }
    rlord_core::private::write(path, t.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(t)
}

fn valid(t: &str) -> bool {
    t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `given` is the token, in constant time.
pub fn same(token: &str, given: &str) -> bool {
    token.as_bytes().ct_eq(given.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn made_private_kept_and_remade() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data/web.token");
        let a = load_or_create(&p).unwrap();
        assert!(valid(&a));
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Kept across restarts.
        assert_eq!(load_or_create(&p).unwrap(), a);
        // Gone (a new token was asked for), or not a token: a new one.
        std::fs::remove_file(&p).unwrap();
        let b = load_or_create(&p).unwrap();
        assert_ne!(a, b);
        std::fs::write(&p, "short").unwrap();
        let c = load_or_create(&p).unwrap();
        assert!(valid(&c) && c != b);
    }

    #[test]
    fn compare_is_exact() {
        let t = "ab".repeat(32);
        assert!(same(&t, &t.clone()));
        assert!(!same(&t, &"ab".repeat(31)));
        assert!(!same(&t, &format!("{}ac", "ab".repeat(31))));
        assert!(!same(&t, ""));
    }
}
