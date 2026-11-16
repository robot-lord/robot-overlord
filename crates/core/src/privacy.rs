//! Predict, before touching a folder, whether reading it would stall: a
//! macOS privacy (TCC) prompt, or another filesystem (a network mount, a
//! removable disk, a File Provider folder).
//!
//! No public API reports TCC status without risking the prompt, and the
//! TCC database needs Full Disk Access. So the guard goes by name: the
//! home folders macOS protects and `/Volumes/*`. The other-filesystem rule
//! (`st_dev`, like `find -xdev`) is applied by the walker (`git::scan_repos`).
//! A folder the user named, or one a session already runs in, counts as
//! granted: macOS grants the whole protected folder once it is allowed.

use std::path::{Component, Path, PathBuf};

/// Why a folder is left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Why {
    /// macOS would ask the user's permission.
    MacPrivacy,
    /// Another disk or a network mount.
    OtherFilesystem,
}

impl Why {
    pub fn id(self) -> &'static str {
        match self {
            Why::MacPrivacy => "privacy",
            Why::OtherFilesystem => "other_fs",
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Why::MacPrivacy => "macOS asks permission",
            Why::OtherFilesystem => "another disk or network mount",
        }
    }
}

/// Folders directly under `$HOME` that macOS guards with a privacy prompt.
pub const PROTECTED_HOME_DIRS: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Movies",
    "Music",
    "Pictures",
    "Library",
];

/// `$HOME` as given and resolved (it may sit behind a symlink, and paths
/// are recorded either way). Empty when unset.
pub fn home_paths() -> Vec<PathBuf> {
    let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        return Vec::new();
    };
    let h = PathBuf::from(h);
    let mut v = Vec::new();
    if let Ok(c) = std::fs::canonicalize(&h) {
        if c != h {
            v.push(c);
        }
    }
    v.push(h);
    v
}

/// Says which folders would raise a prompt. No disk access after it is
/// built: it is asked before anything touches the folder.
///
/// Nothing counts as granted because a session ran in a folder: a macOS
/// grant belongs to the app that made the access (the "responsible
/// process"), so a session under a terminal or an editor proves nothing
/// about rlord's daemon. Only folders the user named as scan roots are
/// read inside a guarded folder (`prompts_within`).
#[derive(Debug, Clone, Default)]
pub struct Guard {
    /// macOS only: the home whose protected folders are guarded, as given
    /// and resolved (paths may come either way).
    homes: Vec<PathBuf>,
    /// macOS only: guard `/Volumes/*`.
    volumes: bool,
    /// Compare paths ASCII-case-insensitively (macOS volumes usually are).
    fold_case: bool,
}

impl Guard {
    /// The real machine: on macOS the user's home and `/Volumes`; elsewhere
    /// nothing is guarded by name.
    pub fn new() -> Guard {
        let mac = cfg!(target_os = "macos");
        let mut homes = Vec::new();
        if let Some(h) = mac
            .then(|| std::env::var_os("HOME"))
            .flatten()
            .filter(|h| !h.is_empty())
        {
            let h = PathBuf::from(h);
            if let Ok(c) = std::fs::canonicalize(&h) {
                if c != h {
                    homes.push(c);
                }
            }
            homes.push(h);
        }
        Guard {
            homes,
            volumes: mac,
            fold_case: mac,
        }
    }

    /// A guard with the given home and `/Volumes` rule (tests; macOS-style
    /// case folding when either is on).
    pub fn with(home: Option<&Path>, volumes: bool) -> Guard {
        Guard::with_homes(
            &home.map(Path::to_path_buf).into_iter().collect::<Vec<_>>(),
            volumes,
        )
    }

    /// `with`, for a home known by more than one path (tests).
    pub fn with_homes(homes: &[PathBuf], volumes: bool) -> Guard {
        Guard {
            homes: homes.to_vec(),
            volumes,
            fold_case: !homes.is_empty() || volumes,
        }
    }

    /// By name only: `path` is, or is inside, a folder macOS guards with a
    /// prompt.
    pub fn prompts(&self, path: &Path) -> Option<Why> {
        self.guarded_ancestor(path).map(|_| Why::MacPrivacy)
    }

    /// `prompts`, for a folder met while walking from `root`: none when the
    /// walk started inside that same guarded folder (the user named a
    /// folder in it as a root, so they expect it read).
    pub fn prompts_within(&self, path: &Path, root: &Path) -> Option<Why> {
        let guarded = self.guarded_ancestor(path)?;
        (!self.is_within(root, &guarded)).then_some(Why::MacPrivacy)
    }

    /// The guarded folder `path` is in (or is), if any.
    fn guarded_ancestor(&self, path: &Path) -> Option<PathBuf> {
        for home in &self.homes {
            for d in PROTECTED_HOME_DIRS {
                let g = home.join(d);
                if self.is_within(path, &g) {
                    return Some(g);
                }
            }
        }
        if self.volumes {
            let mut c = path.components();
            if c.next() == Some(Component::RootDir)
                && c.next()
                    .is_some_and(|v| self.eq(v.as_os_str(), "Volumes".as_ref()))
            {
                if let Some(vol) = c.next() {
                    return Some(Path::new("/Volumes").join(vol));
                }
            }
        }
        None
    }

    /// The same folder, as this machine compares paths (ASCII case folded
    /// on macOS).
    pub fn same(&self, a: &Path, b: &Path) -> bool {
        a.components().count() == b.components().count() && self.is_within(a, b)
    }

    /// Paths compare case-insensitively here (macOS).
    pub fn folds_case(&self) -> bool {
        self.fold_case
    }

    /// `inner` is `outer` or below it, component by component.
    pub fn is_within(&self, inner: &Path, outer: &Path) -> bool {
        let mut i = inner.components();
        for o in outer.components() {
            match i.next() {
                Some(c) if self.eq(c.as_os_str(), o.as_os_str()) => {}
                _ => return false,
            }
        }
        true
    }

    fn eq(&self, a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
        if self.fold_case {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_folders_prompt_by_name() {
        let home = Path::new("/Users/me");
        let g = Guard::with(Some(home), true);
        for d in PROTECTED_HOME_DIRS {
            assert_eq!(g.prompts(&home.join(d)), Some(Why::MacPrivacy), "{d}");
        }
        assert_eq!(
            g.prompts(Path::new("/Users/me/Documents/code/app")),
            Some(Why::MacPrivacy)
        );
        assert_eq!(g.prompts(Path::new("/Users/me/src")), None);
        assert_eq!(g.prompts(Path::new("/Users/me")), None);
        assert_eq!(g.prompts(Path::new("/Users/me/DocumentsX")), None);
        assert_eq!(g.prompts(Path::new("/Volumes/X/y")), Some(Why::MacPrivacy));
        assert_eq!(g.prompts(Path::new("/Volumes")), None);
        // Case-insensitive, as macOS volumes usually are.
        assert_eq!(
            g.prompts(Path::new("/users/ME/documents/a")),
            Some(Why::MacPrivacy)
        );
        // Linux: nothing by name.
        let g = Guard::with(None, false);
        assert_eq!(g.prompts(Path::new("/Users/me/Documents")), None);
        assert_eq!(g.prompts(Path::new("/Volumes/X")), None);
    }

    /// A home behind a symlink is guarded by either path.
    #[test]
    fn both_spellings_of_home_are_guarded() {
        let g = Guard::with_homes(
            &[PathBuf::from("/home/me"), PathBuf::from("/var/home/me")],
            false,
        );
        assert_eq!(
            g.prompts(Path::new("/home/me/Documents/a")),
            Some(Why::MacPrivacy)
        );
        assert_eq!(
            g.prompts(Path::new("/var/home/me/Desktop")),
            Some(Why::MacPrivacy)
        );
        assert_eq!(g.prompts(Path::new("/var/home/me/src")), None);
    }

    #[test]
    fn same_folder_follows_the_case_rule() {
        let mac = Guard::with(Some(Path::new("/Users/me")), true);
        assert!(mac.same(Path::new("/Users/me/Code"), Path::new("/users/ME/code")));
        assert!(!mac.same(Path::new("/Users/me/Code"), Path::new("/Users/me/Code/x")));
        let linux = Guard::with(None, false);
        assert!(!linux.same(Path::new("/home/me/Code"), Path::new("/home/me/code")));
        assert!(linux.same(Path::new("/home/me/code"), Path::new("/home/me/code")));
    }

    /// Inside a root the user named, its own guarded folder is read; any
    /// other guarded folder is not, and a walk from outside never enters.
    #[test]
    fn a_named_root_opens_only_its_own_guarded_folder() {
        let g = Guard::with(Some(Path::new("/Users/me")), true);
        let docs_root = Path::new("/Users/me/Documents/code");
        assert_eq!(
            g.prompts_within(Path::new("/Users/me/Documents/code/app"), docs_root),
            None
        );
        assert_eq!(
            g.prompts_within(Path::new("/Users/me/Desktop/x"), docs_root),
            Some(Why::MacPrivacy)
        );
        let home_root = Path::new("/Users/me");
        assert_eq!(
            g.prompts_within(Path::new("/Users/me/Documents"), home_root),
            Some(Why::MacPrivacy)
        );
        assert_eq!(
            g.prompts_within(Path::new("/Volumes/Ext/r/a"), Path::new("/Volumes/Ext/r")),
            None
        );
    }
}
