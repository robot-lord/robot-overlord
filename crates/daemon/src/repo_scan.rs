//! The repo finder's disk scan, run in the background.
//!
//! A scan walks the disk and can block for as long as it likes (a macOS
//! privacy prompt nobody answers, a dead network mount), so callers never
//! wait on one for long. They get the last result for their roots, which is
//! refreshed in the background once it is a minute old. Only roots with no
//! result yet are waited on, during their scan's first seconds.
//!
//! At most one scan per set of roots runs at a time, and a stuck one is not
//! retried: whatever blocked it would block the retry too. Its roots keep
//! their last result (or report `scanning`) until it finishes; changing
//! the roots (`code_dir`, `repo_roots`) scans the new ones. Nothing scans until a look-up asks, so
//! a privacy prompt the skip list misses never appears unprompted.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rlord_core::git::{Scan, Skipped};

/// What a scan covers: (roots, depth, granted folders sorted). The granted
/// set changes the result (`privacy::Guard`), so a new session folder or
/// code folder scans again; a result for the same roots under an older
/// granted set is dropped when the new one lands.
pub type Key = (Vec<String>, u8, Vec<String>);

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

#[derive(Default)]
struct State {
    /// Each set of roots' last result: when it was found, and what.
    found: HashMap<Key, (Instant, Scan)>,
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

    /// Repos under `key`'s roots, found by `scan` on a background thread.
    pub fn get<F>(&self, key: &Key, scan: F) -> Found
    where
        F: FnOnce(&Key) -> Scan + Send + 'static,
    {
        let (lock, done) = &*self.state;
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh(&mut st, key, scan);
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
            Some((_, scan)) => Found {
                repos: scan.repos.clone(),
                skipped: scan.skipped.clone(),
                skipped_more: scan.skipped_more,
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

    /// Start a scan of `key` unless its result is fresh or one is running.
    fn refresh<F>(&self, st: &mut State, key: &Key, scan: F)
    where
        F: FnOnce(&Key) -> Scan + Send + 'static,
    {
        let fresh = st
            .found
            .get(key)
            .is_some_and(|(at, _)| at.elapsed() < self.timing.fresh);
        if fresh || st.running.contains_key(key) {
            return;
        }
        st.running.insert(key.clone(), Instant::now());
        let state = self.state.clone();
        let k = key.clone();
        let spawned = std::thread::Builder::new()
            .name("repo-scan".into())
            .spawn(move || run(state, k, scan));
        if spawned.is_err() {
            st.running.remove(key);
        }
    }
}

/// One scan, on its own thread. Frees its roots' slot even if it panics.
fn run<F>(state: Shared, key: Key, scan: F)
where
    F: FnOnce(&Key) -> Scan,
{
    struct Slot(Shared, Key);
    impl Drop for Slot {
        fn drop(&mut self) {
            let (lock, done) = &*self.0;
            let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
            st.running.remove(&self.1);
            if std::thread::panicking() {
                // Don't rerun a scan that panics on every look-up: an empty
                // result, retried once it is stale.
                tracing::warn!(roots = ?self.1.0, "repo scan panicked");
                st.found
                    .insert(self.1.clone(), (Instant::now(), Scan::default()));
            }
            done.notify_all();
        }
    }
    let _slot = Slot(state.clone(), key.clone());
    let found = scan(&key);
    let mut st = state.0.lock().unwrap_or_else(|e| e.into_inner());
    st.found.retain(|k, _| (&k.0, k.1) != (&key.0, key.1));
    st.found.insert(key, (Instant::now(), found));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn key(r: &str) -> Key {
        (vec![r.to_string()], 3, Vec::new())
    }

    /// A scan that blocks until the returned sender sends or drops.
    fn gated(repos: &[&str]) -> (mpsc::Sender<()>, impl FnOnce(&Key) -> Scan + Send + 'static) {
        let (tx, rx) = mpsc::channel::<()>();
        let repos: Vec<String> = repos.iter().map(|r| r.to_string()).collect();
        (tx, move |_: &Key| {
            let _ = rx.recv();
            repos.into()
        })
    }

    /// Ask until `key` has a result (scans run on their own threads).
    fn settled(s: &Scanner, key: &Key) -> Vec<String> {
        for _ in 0..200 {
            let f = s.get(key, |_| panic!("rescanned while one was running"));
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
        let f = s.get(&key("a"), |_| vec!["/a/x".to_string()].into());
        assert!(!f.scanning);
        assert_eq!(f.repos, vec!["/a/x".to_string()]);
        let f = s.get(&key("a"), |_| panic!("rescanned a fresh result"));
        assert_eq!(f.repos, vec!["/a/x".to_string()]);
    }

    #[test]
    fn stuck_scan_blocks_no_one_and_is_not_retried() {
        let s = Scanner::with_timing(timing(50));
        let (release, scan) = gated(&["/a/x"]);
        let f = s.get(&key("a"), scan);
        assert!(f.scanning && f.repos.is_empty());
        // Past first_wait: no waiting, and no second scan of the same roots.
        let t = Instant::now();
        let f = s.get(&key("a"), |_| panic!("started a second scan"));
        assert!(f.scanning);
        assert!(t.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
        assert_eq!(settled(&s, &key("a")), vec!["/a/x".to_string()]);
    }

    #[test]
    fn changed_roots_scan_beside_a_stuck_one() {
        let s = Scanner::with_timing(timing(50));
        let (_hold, scan) = gated(&[]);
        s.get(&key("a"), scan);
        let f = Scanner::get(&s, &key("b"), |_| vec!["/b/y".to_string()].into());
        let repos = if f.scanning {
            settled(&s, &key("b"))
        } else {
            f.repos
        };
        assert_eq!(repos, vec!["/b/y".to_string()]);
    }

    /// Another granted set is another scan; its result replaces the old
    /// one for the same roots.
    #[test]
    fn a_new_granted_set_scans_again() {
        let s = Scanner::with_timing(timing(5_000));
        let k1 = key("a");
        s.get(&k1, |_| vec!["/a/x".to_string()].into());
        let mut k2 = key("a");
        k2.2 = vec!["/a/mnt".into()];
        let f = s.get(&k2, |k| vec![k.2[0].clone()].into());
        assert_eq!(f.repos, ["/a/mnt"]);
        let st = s.state.0.lock().unwrap();
        assert!(!st.found.contains_key(&k1) && st.found.contains_key(&k2));
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
        let f = s.get(&key("h"), move |_| scan);
        assert_eq!(f.repos, vec!["/h/a".to_string()]);
        assert_eq!((f.skipped, f.skipped_more), (skipped, 3));
    }

    #[test]
    fn panicking_scan_leaves_an_empty_result() {
        let s = Scanner::with_timing(timing(5_000));
        let f = s.get(&key("a"), |_| panic!("scan blew up"));
        assert!(!f.scanning && f.repos.is_empty());
        // Not rerun on the next look-up.
        let f = s.get(&key("a"), |_| panic!("reran a scan that panicked"));
        assert!(!f.scanning && f.repos.is_empty());
    }
}
