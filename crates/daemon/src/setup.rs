//! Setup: the code folder (`[session] code_dir`), where the user keeps
//! repos and where new ones go. The repo finder scans it.

use rlord_core::config::Config;
use rlord_core::db::Db;

/// kv row: setup has been answered (`done:<ms>` or `skipped:<ms>`). Not a
/// config key: declining must not write the config.
pub(crate) const KV: &str = "setup_code_dir";

/// The first-run form should open: no code folder set, and the user hasn't
/// answered setup yet.
pub(crate) fn needed(db: &Db, cfg: &Config) -> bool {
    cfg.session.code_dir_path().is_none() && db.kv_get(KV).ok().flatten().is_none()
}
