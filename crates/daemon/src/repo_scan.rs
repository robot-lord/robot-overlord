//! The repo finder's disk scan, run in the background.
//!
//! A scan walks the disk and can block for as long as it likes (a macOS
//! privacy prompt nobody answers, a dead network mount), so callers never
//! wait on one for long. They get the last result for their roots, which is
//! refreshed in the background once it is a minute old. Only roots that have
//! never been scanned are waited on, during the scan's first few seconds.
//! A scan running far too long is presumed stuck: one more is tried beside
//! it, and no more after that until something finishes.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// What a scan covers: (roots, depth).
pub type Key = (Vec<String>, u8);

/// Timings, set by the daemon (`Scanner::default`) and by tests.
#[derive(Clone, Copy)]
pub struct Timing {
    /// A result younger than this is served without a rescan.
    pub fresh: Duration,
    /// How long into a scan a caller with no result yet waits for it.
    pub first_wait: Duration,
    /// A scan running longer is presumed stuck.
    pub stuck: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            fresh: Duration::from_secs(60),
            first_wait: Duration::from_secs(2),
            stuck: Duration::from_secs(300),
        }
    }
}

/// Scans of the same roots in flight at most: one, plus one retry beside a
/// stuck one.
const MAX_RETRY: usize = 2;
/// Scans in flight at most, of any roots (a stuck walk keeps its thread).
const MAX_RUNNING: usize = 4;

/// A finished scan's result.
struct Last {
    key: Key,
    /// When its scan started (which result is newer) and finished (how
    /// fresh it is).
    started: Instant,
    finished: Instant,
    repos: Vec<String>,
}

#[derive(Default)]
struct State {
    last: Option<Last>,
    /// The newest scan started: its generation, key and start time.
    newest: Option<(u64, Key, Instant)>,
    generation: u64,
    running: usize,
    /// The roots last asked for: a result for others is old news.
    want: Option<Key>,
}

impl State {
    fn result_for(&self, key: &Key) -> Option<&Last> {
        self.last.as_ref().filter(|l| &l.key == key)
    }
}

#[derive(Default)]
pub struct Scanner {
    state: Arc<(Mutex<State>, Condvar)>,
    timing: Timing,
}

/// A scan's answer.
pub struct Found {
    pub repos: Vec<String>,
    /// No result for these roots yet: a scan is running, so `repos` is
    /// empty for now.
    pub scanning: bool,
}

type Shared = Arc<(Mutex<State>, Condvar)>;

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
        F: FnOnce(&Key) -> Vec<String> + Send + 'static,
    {
        let t = self.timing;
        let (lock, done) = &*self.state;
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh(&mut st, key, scan);
        if st.result_for(key).is_none() {
            let started = st
                .newest
                .as_ref()
                .filter(|(_, k, _)| k == key)
                .map(|(_, _, at)| *at);
            let left = started.map_or(Duration::ZERO, |at| {
                t.first_wait.saturating_sub(at.elapsed())
            });
            st = done
                .wait_timeout_while(st, left, |st| st.result_for(key).is_none())
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        match st.result_for(key) {
            Some(l) => Found {
                repos: l.repos.clone(),
                scanning: false,
            },
            None => Found {
                repos: Vec::new(),
                scanning: true,
            },
        }
    }

    /// Start a scan of `key` if one is due, without waiting for it (at
    /// daemon start and after a config reload, so the first look-up has a
    /// result).
    pub fn prefetch<F>(&self, key: &Key, scan: F)
    where
        F: FnOnce(&Key) -> Vec<String> + Send + 'static,
    {
        let mut st = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
        self.refresh(&mut st, key, scan);
    }

    /// Note `key` as wanted and start a scan of it unless its result is
    /// fresh, a scan of it is under way and not stuck, or the caps are hit.
    fn refresh<F>(&self, st: &mut State, key: &Key, scan: F)
    where
        F: FnOnce(&Key) -> Vec<String> + Send + 'static,
    {
        let t = self.timing;
        st.want = Some(key.clone());
        if st
            .result_for(key)
            .is_some_and(|l| l.finished.elapsed() < t.fresh)
        {
            return;
        }
        let under_way = st
            .newest
            .as_ref()
            .is_some_and(|(_, k, at)| k == key && at.elapsed() < t.stuck);
        let retry = st.newest.as_ref().is_some_and(|(_, k, _)| k == key);
        let cap = if retry { MAX_RETRY } else { MAX_RUNNING };
        if under_way || st.running >= cap {
            return;
        }
        st.generation += 1;
        let generation = st.generation;
        let started = Instant::now();
        let before = st.newest.replace((generation, key.clone(), started));
        st.running += 1;
        let state = self.state.clone();
        let k = key.clone();
        let spawned = std::thread::Builder::new()
            .name("repo-scan".into())
            .spawn(move || run(state, generation, started, k, scan));
        if spawned.is_err() {
            st.running -= 1;
            st.newest = before;
        }
    }
}

/// One scan, on its own thread: keep the result if it is for the roots last
/// asked for and its scan started after the kept one's (a stuck scan that
/// finishes late still counts until a later-started scan's result is in).
fn run<F>(state: Shared, generation: u64, started: Instant, key: Key, scan: F)
where
    F: FnOnce(&Key) -> Vec<String>,
{
    // Frees the slot even if the scan panics.
    struct Slot(Shared, u64);
    impl Drop for Slot {
        fn drop(&mut self) {
            let (lock, done) = &*self.0;
            let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
            st.running -= 1;
            if st.newest.as_ref().is_some_and(|(g, _, _)| *g == self.1) {
                st.newest = None;
            }
            done.notify_all();
        }
    }
    let _slot = Slot(state.clone(), generation);
    let repos = scan(&key);
    let mut st = state.0.lock().unwrap_or_else(|e| e.into_inner());
    let wanted = st.want.as_ref() == Some(&key);
    let newer = st.result_for(&key).is_none_or(|l| l.started < started);
    if wanted && newer {
        st.last = Some(Last {
            key,
            started,
            finished: Instant::now(),
            repos,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn key(r: &str) -> Key {
        (vec![r.to_string()], 3)
    }

    // Generous margins: CI runners are slow and busy.
    fn quick() -> Timing {
        Timing {
            fresh: Duration::from_secs(60),
            first_wait: Duration::from_secs(2),
            stuck: Duration::from_secs(60),
        }
    }

    #[test]
    fn fast_scan_is_waited_on_and_cached() {
        let s = Scanner::with_timing(quick());
        let f = s.get(&key("a"), |_| vec!["/a/x".into()]);
        assert!(!f.scanning);
        assert_eq!(f.repos, vec!["/a/x".to_string()]);
        // Cached: a second scan would panic.
        let f = s.get(&key("a"), |_| panic!("rescanned a fresh result"));
        assert_eq!(f.repos, vec!["/a/x".to_string()]);
    }

    /// Short waits, for tests where scans are meant to be stuck.
    fn stuck_timing() -> Timing {
        Timing {
            fresh: Duration::from_secs(60),
            first_wait: Duration::from_millis(50),
            stuck: Duration::from_millis(300),
        }
    }

    /// A scan that blocks until the returned sender sends or drops.
    fn gated(
        repos: &[&str],
    ) -> (
        mpsc::Sender<()>,
        impl FnOnce(&Key) -> Vec<String> + Send + 'static,
    ) {
        let (tx, rx) = mpsc::channel::<()>();
        let repos: Vec<String> = repos.iter().map(|r| r.to_string()).collect();
        (tx, move |_: &Key| {
            let _ = rx.recv();
            repos
        })
    }

    #[test]
    fn stuck_scan_blocks_no_one() {
        let s = Scanner::with_timing(Timing {
            stuck: Duration::from_secs(60),
            ..stuck_timing()
        });
        let (_hold, scan) = gated(&[]);
        let f = s.get(&key("a"), scan);
        assert!(f.scanning && f.repos.is_empty());
        // Past first_wait, a caller doesn't wait at all.
        let t = Instant::now();
        let f = s.get(&key("a"), |_| vec![]);
        assert!(f.scanning);
        assert!(t.elapsed() < Duration::from_millis(40));
    }

    #[test]
    fn retry_result_wins_over_late_stuck_one() {
        let s = Scanner::with_timing(Timing {
            first_wait: Duration::from_secs(2),
            ..stuck_timing()
        });
        let (release, scan) = gated(&["/stale"]);
        s.get(&key("a"), scan); // waits first_wait: past `stuck` by then
        let f = s.get(&key("a"), |_| vec!["/fresh".into()]);
        assert_eq!(f.repos, vec!["/fresh".to_string()]);
        // The stuck one finishes later: it must not replace the retry's.
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let f = s.get(&key("a"), |_| panic!("rescanned a fresh result"));
        assert_eq!(f.repos, vec!["/fresh".to_string()]);
    }

    #[test]
    fn stuck_scan_finishing_first_still_counts() {
        let s = Scanner::with_timing(stuck_timing());
        let (release_a, scan_a) = gated(&["/a1"]);
        s.get(&key("a"), scan_a);
        std::thread::sleep(Duration::from_millis(350));
        let (_hold_b, scan_b) = gated(&["/a2"]);
        s.get(&key("a"), scan_b); // the retry, stuck too
        release_a.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let f = s.get(&key("a"), |_| vec![]);
        assert_eq!(f.repos, vec!["/a1".to_string()]);
    }

    #[test]
    fn retry_result_replaces_stuck_one_that_finished_first() {
        let s = Scanner::with_timing(stuck_timing());
        let (release_a, scan_a) = gated(&["/old"]);
        s.get(&key("a"), scan_a);
        std::thread::sleep(Duration::from_millis(350));
        let (release_b, scan_b) = gated(&["/new"]);
        s.get(&key("a"), scan_b);
        release_a.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        release_b.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let f = s.get(&key("a"), |_| panic!("rescanned a fresh result"));
        assert_eq!(f.repos, vec!["/new".to_string()]);
    }

    #[test]
    fn prefetch_fills_the_first_look_up() {
        let s = Scanner::with_timing(stuck_timing());
        s.prefetch(&key("a"), |_| vec!["/a/x".into()]);
        std::thread::sleep(Duration::from_millis(200));
        let f = s.get(&key("a"), |_| panic!("rescanned a fresh result"));
        assert_eq!(f.repos, vec!["/a/x".to_string()]);
    }

    #[test]
    fn stuck_scans_are_capped_but_new_roots_still_scan() {
        let s = Scanner::with_timing(stuck_timing());
        let started = Arc::new(Mutex::new(0));
        let mut holds = Vec::new();
        for _ in 0..5 {
            let (hold, scan) = gated(&[]);
            holds.push(hold);
            let started = started.clone();
            s.get(&key("a"), move |k| {
                *started.lock().unwrap() += 1;
                scan(k)
            });
            std::thread::sleep(Duration::from_millis(350));
        }
        assert_eq!(*started.lock().unwrap(), MAX_RETRY);
        let f = s.get(&key("b"), |_| vec!["/b/y".into()]);
        assert_eq!(f.repos, vec!["/b/y".to_string()]);
    }

    #[test]
    fn panicking_scan_frees_its_slot() {
        let s = Scanner::with_timing(stuck_timing());
        s.get(&key("a"), |_| panic!("scan blew up"));
        std::thread::sleep(Duration::from_millis(100));
        let f = s.get(&key("a"), |_| vec!["/a/x".into()]);
        assert_eq!(f.repos, vec!["/a/x".to_string()]);
    }

    #[test]
    fn changed_roots_scan_anew() {
        let s = Scanner::with_timing(quick());
        s.get(&key("a"), |_| vec!["/a/x".into()]);
        let f = s.get(&key("b"), |_| vec!["/b/y".into()]);
        assert_eq!(f.repos, vec!["/b/y".to_string()]);
    }
}
