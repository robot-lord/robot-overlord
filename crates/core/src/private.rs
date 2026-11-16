//! rlord's own folders and files are private to the user: folders 0700,
//! the state DB and per-session files 0600. They hold prompts, launch
//! commands, transcript excerpts and the overlord's notes.
//!
//! They get those modes when they are created (never chmod-ed afterwards,
//! so there is no moment they are readable by others). Every place that
//! may be first to make one of rlord's folders goes through `dir` /
//! `dir_under`. A folder that already exists is left as it is.

use std::fs::{DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const DIR_MODE: u32 = 0o700;
pub const FILE_MODE: u32 = 0o600;

/// Make sure `dir` exists; created 0700 if missing. Its parents are made
/// as the system makes folders (they aren't rlord's). An existing folder
/// is left as it is.
pub fn dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    match DirBuilder::new().mode(DIR_MODE).create(dir) {
        Err(e) if e.kind() == ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        r => r,
    }
}

/// `sub`, somewhere below rlord's folder `root`: `root` is made first
/// (private), the folders between them inside it, each 0700 too.
pub fn dir_under(root: &Path, sub: &Path) -> std::io::Result<()> {
    dir(root)?;
    if sub.is_dir() {
        return Ok(());
    }
    DirBuilder::new().recursive(true).mode(DIR_MODE).create(sub)
}

/// Create `path` empty, 0600, if it doesn't exist (an existing file is
/// left as it is).
pub fn touch(path: &Path) -> std::io::Result<()> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)
    {
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        r => r.map(drop),
    }
}

/// Open `path` for appending, created 0600 if it's new.
pub fn append(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(FILE_MODE)
        .open(path)
}

/// Replace `path`'s contents: written to a new 0600 file beside it, then
/// renamed over it, so a reader sees the old or the new text, never a
/// half-written one, and the file is never readable by others. No fsync:
/// for files rlord can write again (`write_synced` for the rest).
pub fn write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    write_with(path, contents, false)
}

/// `write`, synced to disk before the rename, for a file that must
/// survive a crash.
pub fn write_synced(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    write_with(path, contents, true)
}

fn write_with(path: &Path, contents: &[u8], sync: bool) -> std::io::Result<()> {
    // A temp name of its own (another writer's is never touched): taken
    // with `create_new`, the next one tried if it exists.
    let (tmp, mut f) = loop {
        let tmp = tmp_beside(path);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&tmp)
        {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let written = f
        .write_all(contents)
        .and_then(|_| if sync { f.sync_all() } else { Ok(()) })
        .and_then(|_| {
            drop(f);
            std::fs::rename(&tmp, path)
        });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// A temp file name beside `path`, unique to this process, thread and
/// call.
fn tmp_beside(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let thread: String = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    path.with_file_name(format!(
        ".{}.tmp-{}-{}-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id(),
        thread,
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn new_folders_are_private_from_the_start() {
        let t = tempfile::tempdir().unwrap();
        let fresh = t.path().join("a/robot-overlord");
        dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o700);
        // Made again: fine.
        dir(&fresh).unwrap();

        let root = t.path().join("root");
        let under = root.join("sessions/x");
        dir_under(&root, &under).unwrap();
        for d in [&root, &root.join("sessions"), &under] {
            assert_eq!(mode(d), 0o700, "{}", d.display());
        }
    }

    #[test]
    fn files_are_created_private() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("context.md");
        write(&p, b"one").unwrap();
        assert_eq!(mode(&p), 0o600);
        write_synced(&p, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(mode(&p), 0o600);
        // Nothing left beside it.
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 1);
        // Another writer's temp file beside it is left alone.
        let other = t.path().join(".context.md.tmp-other");
        std::fs::write(&other, b"theirs").unwrap();
        write(&p, b"three").unwrap();
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "theirs");
        assert_ne!(tmp_beside(&p), tmp_beside(&p));
        std::fs::remove_file(&other).unwrap();

        let db = t.path().join("state.db");
        touch(&db).unwrap();
        assert_eq!(mode(&db), 0o600);
        let log = t.path().join("rlordd.log");
        append(&log).unwrap();
        assert_eq!(mode(&log), 0o600);
    }
}
