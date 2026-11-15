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
            first_wait: Duration::from_secs(5),
            stuck: Duration::from_secs(300),
        }
    }
}

/// Scans in flight at most: one, plus one retry beside a stuck one.
const MAX_RUNNING: usize = 2;

#[derive(Default)]
struct State {
    /// The last result kept: its key, when it finished, the repos.
    last: Option<(Key, Instant, Vec<String>)>,
    /// The newest scan started: its generation, key and start time. Only
    /// its result is kept; an older scan finishing late is dropped.
    newest: Option<(u64, Key, Instant)>,
    generation: u64,
    running: usize,
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
        let have = |st: &State| st.last.as_ref().is_some_and(|(k, _, _)| k == key);
        if let Some((_, at, repos)) = st.last.as_ref().filter(|(k, _, _)| k == key) {
            if at.elapsed() < t.fresh {
                return Found {
                    repos: repos.clone(),
                    scanning: false,
                };
            }
        }
        // Start a scan unless one for this key is under way and not stuck.
        let under_way = st
            .newest
            .as_ref()
            .is_some_and(|(_, k, at)| k == key && at.elapsed() < t.stuck);
        if !under_way && st.running < MAX_RUNNING {
            st.generation += 1;
            let generation = st.generation;
            st.newest = Some((generation, key.clone(), Instant::now()));
            st.running += 1;
            let state = self.state.clone();
            let key = key.clone();
            std::thread::spawn(move || {
                let repos = scan(&key);
                let (lock, done) = &*state;
                let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
                st.running -= 1;
                if st.newest.as_ref().is_some_and(|(g, _, _)| *g == generation) {
                    st.newest = None;
                    st.last = Some((key, Instant::now(), repos));
                    done.notify_all();
                }
            });
        }
        if !have(&st) {
            let started = st
                .newest
                .as_ref()
                .filter(|(_, k, _)| k == key)
                .map(|(_, _, at)| *at);
            let left = started.map_or(Duration::ZERO, |at| {
                t.first_wait.saturating_sub(at.elapsed())
            });
            st = done
                .wait_timeout_while(st, left, |st| !have(st))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        match st.last.as_ref().filter(|(k, _, _)| k == key) {
            Some((_, _, repos)) => Found {
                repos: repos.clone(),
                scanning: false,
            },
            None => Found {
                repos: Vec::new(),
                scanning: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn key(r: &str) -> Key {
        (vec![r.to_string()], 3)
    }

    fn quick() -> Timing {
        Timing {
            fresh: Duration::from_secs(60),
            first_wait: Duration::from_millis(200),
            stuck: Duration::from_millis(300),
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

    #[test]
    fn stuck_scan_blocks_no_one() {
        let s = Scanner::with_timing(Timing {
            stuck: Duration::from_secs(60),
            ..quick()
        });
        let (_hold, gate) = mpsc::channel::<()>();
        let t = Instant::now();
        let f = s.get(&key("a"), move |_| {
            let _ = gate.recv();
            vec![]
        });
        assert!(f.scanning && f.repos.is_empty());
        assert!(t.elapsed() < Duration::from_secs(2));
        // Past first_wait, a caller doesn't wait at all.
        std::thread::sleep(Duration::from_millis(250));
        let t = Instant::now();
        let f = s.get(&key("a"), |_| vec![]);
        assert!(f.scanning);
        assert!(t.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn old_scan_finishing_late_is_dropped() {
        let s = Scanner::with_timing(quick());
        let (release_a, gate) = mpsc::channel::<()>();
        s.get(&key("a"), move |_| {
            let _ = gate.recv();
            vec!["/stale".into()]
        });
        // Presumed stuck: a retry runs beside it and finishes first.
        std::thread::sleep(Duration::from_millis(350));
        let f = s.get(&key("a"), |_| vec!["/fresh".into()]);
        assert_eq!(f.repos, vec!["/fresh".to_string()]);
        // The stuck one finishes later: its result must not replace it.
        release_a.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let f = s.get(&key("a"), |_| panic!("rescanned a fresh result"));
        assert_eq!(f.repos, vec!["/fresh".to_string()]);
    }

    #[test]
    fn stuck_scans_are_capped() {
        let s = Scanner::with_timing(quick());
        let started = Arc::new(Mutex::new(0));
        let (_hold, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        for _ in 0..5 {
            let (started, gate) = (started.clone(), gate.clone());
            s.get(&key("a"), move |_| {
                *started.lock().unwrap() += 1;
                let _ = gate.lock().unwrap().recv();
                vec![]
            });
            std::thread::sleep(Duration::from_millis(350));
        }
        assert_eq!(*started.lock().unwrap(), MAX_RUNNING);
    }

    #[test]
    fn changed_roots_scan_anew() {
        let s = Scanner::with_timing(quick());
        s.get(&key("a"), |_| vec!["/a/x".into()]);
        let f = s.get(&key("b"), |_| vec!["/b/y".into()]);
        assert_eq!(f.repos, vec!["/b/y".to_string()]);
    }
}
