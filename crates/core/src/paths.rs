use std::path::PathBuf;

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Persistent data root: `$XDG_DATA_HOME/robot-overlord` or `~/.local/share/robot-overlord`.
pub fn data_dir() -> PathBuf {
    env_dir("XDG_DATA_HOME")
        .or_else(|| env_dir("HOME").map(|h| h.join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("robot-overlord")
}

/// Runtime root for the daemon socket: `$XDG_RUNTIME_DIR/robot-overlord`,
/// falling back to the data dir (which at least has correct ownership).
pub fn runtime_dir() -> PathBuf {
    env_dir("XDG_RUNTIME_DIR")
        .map(|d| d.join("robot-overlord"))
        .unwrap_or_else(|| data_dir().join("run"))
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("daemon.sock")
}

pub fn pid_path() -> PathBuf {
    runtime_dir().join("daemon.pid")
}

pub fn db_path() -> PathBuf {
    data_dir().join("state.db")
}

/// User config: `$XDG_CONFIG_HOME/robot-overlord/config.toml` or `~/.config/...`.
pub fn config_path() -> PathBuf {
    env_dir("XDG_CONFIG_HOME")
        .or_else(|| env_dir("HOME").map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("robot-overlord")
        .join("config.toml")
}
