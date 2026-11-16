//! The repo finder's disk scan, run in the background.
//!
//! A scan walks the disk and can block for as long as it likes (a macOS
//! privacy prompt nobody answers, a dead network mount), so callers never
//! wait on one for long. They get the last result for their roots, which is
//! refreshed in the background once it is a minute old, or once the
//! granted folders it was made with (`privacy::Guard`: those sessions run
//! in) have changed. Only roots with no result yet are waited on, during
//! their scan's first seconds.
//!
//! At most one scan per set of roots runs at a time, and a stuck one is not
//! retried: whatever blocked it would block the retry too. Its roots keep
//! their last result (or report `scanning`) until it finishes; changing
//! the roots (`code_dir`, `repo_roots`) scans the new ones. Nothing scans
//! until a look-up asks, so a privacy prompt the skip list misses never
//! appears unprompted. Nothing touches the disk before the hand-off to the
//! scan's thread: the scan itself resolves the granted folders.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rlord_core::git::{Scan, Skipped};

/// What a scan covers: (roots, depth).
pub type Key = (Vec<String>, u8);

/// Timings, set by the daemon (`Scanner::default`) and by tests.
#[derive(Clone, Copy)]
pub struct Timing {
    /// A result younger than this is served without a rescan.
    pub fresh: Duration,
    /// How long into a scan a caller with no result yet waits for it.
    pub first_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            fresh: Duration::from_secs(60),
            first_wait: Duration::from_secs(2),
        }
    }
}

/// A finished scan: when, with which granted folders (as asked, sorted),
/// and what it found.
struct Result_ {
    at: Instant,
    granted: Vec<String>,
    scan: Scan,
}

#[derive(Default)]
struct State {
    /// Each set of roots' last result.
    found: HashMap<Key, Result_>,
    /// Roots being scanned now, and since when.
    running: HashMap<Key, Instant>,
}

type Shared = Arc<(Mutex<State>, Condvar)>;

#[derive(Default)]
pub struct Scanner {
    state: Shared,
    timing: Timing,
}

/// A scan's answer.
pub struct Found {
    pub repos: Vec<String>,
    /// Folders the scan stayed out of (`git::scan_repos`), and how many
    /// more than are listed.
    pub skipped: Vec<Skipped>,
    pub skipped_more: usize,
    /// No result for these roots yet, and a scan of them is running.
    pub scanning: bool,
}

impl Scanner {
    #[cfg(test)]
    fn with_timing(timing: Timing) -> Self {
        Scanner {
            state: Arc::default(),
            timing,
        }
    }

    /// Repos under `key`'s roots, found by `scan` on a background thread
    /// and handed `granted` (the folders sessions run in, as stored: the
    /// scan resolves them). A result made with another granted set is
    /// served as is and refreshed in the background.
    pub fn get<F>(&self, key: &Key, granted: Vec<String>, scan: F) -> Found
    where
        F: FnOnce(&Key, &[String]) -> Scan + Send + 'static,
    {
        let mut granted = granted;
        granted.sort();
        granted.dedup();
        let (lock, done) = &*self.state;
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh(&mut st, key, granted, scan);
        if !st.found.contains_key(key) {
            if let Some(at) = st.running.get(key).copied() {
                let left = self.timing.first_wait.saturating_sub(at.elapsed());
                st = done
                    .wait_timeout_while(st, left, |st| {
                        !st.found.contains_key(key) && st.running.contains_key(key)
                    })
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
        match st.found.get(key) {
            Some(r) => Found {
                repos: r.scan.repos.clone(),
                skipped: r.scan.skipped.clone(),
                skipped_more: r.scan.skipped_more,
                scanning: false,
            },
            None => Found {
                repos: Vec::new(),
                skipped: Vec::new(),
                skipped_more: 0,
                scanning: st.running.contains_key(key),
            },
        }
    }

    /// Start a scan of `key` unless one is running, or its result is fresh
    /// and was made with these granted folders.
    fn refresh<F>(&self, st: &mut State, key: &Key, granted: Vec<String>, scan: F)
    where
        F: FnOnce(&Key, &[String]) -> Scan + Send + 'static,
    {
        let fresh = st
            .found
            .get(key)
            .is_some_and(|r| r.at.elapsed() < self.timing.fresh && r.granted == granted);
        if fresh || st.running.contains_key(key) {
            return;
        }
        st.running.insert(key.clone(), Instant::now());
        let state = self.state.clone();
        let k = key.clone();
        let spawned = std::thread::Builder::new()
            .name("repo-scan".into())
            .spawn(move || run(state, k, granted, scan));
        if spawned.is_err() {
            st.running.remove(key);
        }
    }
}

/// One scan, on its own thread. Frees its roots' slot even if it panics.
fn run<F>(state: Shared, key: Key, granted: Vec<String>, scan: F)
where
    F: FnOnce(&Key, &[String]) -> Scan,
{
    struct Slot(Shared, Key, Vec<String>);
    impl Drop for Slot {
        fn drop(&mut self) {
            let (lock, done) = &*self.0;
            let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
            st.running.remove(&self.1);
            if std::thread::panicking() {
                // Don't rerun a scan that panics on every look-up: an empty
                // result, retried once it is stale.
                tracing::warn!(roots = ?self.1.0, "repo scan panicked");
                st.found.insert(
                    self.1.clone(),
                    Result_ {
                        at: Instant::now(),
                        granted: self.2.clone(),
                        scan: Scan::default(),
                    },
                );
            }
            done.notify_all();
        }
    }
    let _slot = Slot(state.clone(), key.clone(), granted.clone());
    let found = scan(&key, &granted);
    let mut st = state.0.lock().unwrap_or_else(|e| e.into_inner());
    st.found.insert(
        key,
        Result_ {
            at: Instant::now(),
            granted,
            scan: found,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn key(r: &str) -> Key {
        (vec![r.to_string()], 3)
    }

    /// No granted folders.
    fn none() -> Vec<String> {
        Vec::new()
    }

    /// A scan that blocks until the returned sender sends or drops.
    fn gated(
        repos: &[&str],
    ) -> (
        mpsc::Sender<()>,
        impl FnOnce(&Key, &[String]) -> Scan + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel::<()>();
        let repos: Vec<String> = repos.iter().map(|r| r.to_string()).collect();
        (tx, move |_: &Key, _: &[String]| {
            let _ = rx.recv();
            repos.into()
        })
    }

    /// Ask until `key` has a result (scans run on their own threads).
    fn settled(s: &Scanner, key: &Key) -> Vec<String> {
        for _ in 0..200 {
            let f = s.get(key, none(), |_, _| {
                panic!("rescanned while one was running")
            });
            if !f.scanning {
                return f.repos;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no result");
    }

    fn timing(first_wait_ms: u64) -> Timing {
        Timing {
            fresh: Duration::from_secs(60),
            first_wait: Duration::from_millis(first_wait_ms),
        }
    }

    #[test]
    fn fast_scan_is_waited_on_and_cached() {
        let s = Scanner::with_timing(timing(5_000));
        let f = s.get(&key("a"), none(), |_, _| vec!["/a/x".to_string()].into());
        assert!(!f.scanning);
        assert_eq!(f.repos, vec!["/a/x".to_string()]);
        let f = s.get(&key("a"), none(), |_, _| panic!("rescanned a fresh result"));
        assert_eq!(f.repos, vec!["/a/x".to_string()]);
    }

    #[test]
    fn stuck_scan_blocks_no_one_and_is_not_retried() {
        let s = Scanner::with_timing(timing(50));
        let (release, scan) = gated(&["/a/x"]);
        let f = s.get(&key("a"), none(), scan);
        assert!(f.scanning && f.repos.is_empty());
        // Past first_wait: no waiting, and no second scan of the same roots,
        // whatever the granted folders.
        let t = Instant::now();
        let f = s.get(&key("a"), none(), |_, _| panic!("started a second scan"));
        assert!(f.scanning);
        let f = s.get(&key("a"), vec!["/g".into()], |_, _| {
            panic!("started a second scan")
        });
        assert!(f.scanning);
        assert!(t.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
        assert_eq!(settled(&s, &key("a")), vec!["/a/x".to_string()]);
    }

    #[test]
    fn changed_roots_scan_beside_a_stuck_one() {
        let s = Scanner::with_timing(timing(50));
        let (_hold, scan) = gated(&[]);
        s.get(&key("a"), none(), scan);
        let f = Scanner::get(&s, &key("b"), none(), |_, _| {
            vec!["/b/y".to_string()].into()
        });
        let repos = if f.scanning {
            settled(&s, &key("b"))
        } else {
            f.repos
        };
        assert_eq!(repos, vec!["/b/y".to_string()]);
    }

    /// New granted folders: the old result is served at once (no wait)
    /// and refreshed in the background with the new set, which the scan
    /// is handed.
    #[test]
    fn new_granted_folders_refresh_in_the_background() {
        let s = Scanner::with_timing(timing(5_000));
        s.get(&key("a"), none(), |_, _| vec!["/a/x".to_string()].into());
        // The same set (in another order, with a duplicate): still fresh.
        let g = vec!["/g2".to_string(), "/g1".into(), "/g2".into()];
        let (release, scan) = gated(&["/a/x", "/a/mnt/y"]);
        let t = Instant::now();
        let f = s.get(&key("a"), g.clone(), move |k, granted| {
            assert_eq!(granted, ["/g1", "/g2"]);
            scan(k, granted)
        });
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "waited on a stale result"
        );
        assert!(!f.scanning);
        assert_eq!(f.repos, ["/a/x"]);
        // While it runs: no second scan, the old result still served.
        let f = s.get(&key("a"), vec!["/g3".into()], |_, _| {
            panic!("started a second scan")
        });
        assert_eq!(f.repos, ["/a/x"]);
        release.send(()).unwrap();
        for _ in 0..200 {
            let f = s.get(&key("a"), g.clone(), |_, _| {
                panic!("rescanned a fresh result")
            });
            if f.repos.len() == 2 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the refresh never landed");
    }

    #[test]
    fn skipped_folders_come_back() {
        use rlord_core::privacy::Why;
        let s = Scanner::with_timing(timing(5_000));
        let skipped = vec![Skipped {
            path: "/h/Documents".into(),
            why: Why::MacPrivacy,
        }];
        let scan = Scan {
            repos: vec!["/h/a".into()],
            skipped: skipped.clone(),
            skipped_more: 3,
        };
        let f = s.get(&key("h"), none(), move |_, _| scan);
        assert_eq!(f.repos, vec!["/h/a".to_string()]);
        assert_eq!((f.skipped, f.skipped_more), (skipped, 3));
    }

    #[test]
    fn panicking_scan_leaves_an_empty_result() {
        let s = Scanner::with_timing(timing(5_000));
        let f = s.get(&key("a"), none(), |_, _| panic!("scan blew up"));
        assert!(!f.scanning && f.repos.is_empty());
        // Not rerun on the next look-up.
        let f = s.get(&key("a"), none(), |_, _| {
            panic!("reran a scan that panicked")
        });
        assert!(!f.scanning && f.repos.is_empty());
    }
}
