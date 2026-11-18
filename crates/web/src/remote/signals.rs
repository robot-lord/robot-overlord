//! When to send a device the relay's `signal` ("something for you"): at
//! most once per [`SIGNAL_EVERY`], and once more when that window ends if
//! a change came inside it, so a device never misses a change until some
//! later one (`docs/working/e2e-protocol.md` §4, rule 5).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// At most one "something for you" per device per this long.
const SIGNAL_EVERY: Duration = Duration::from_secs(30);

/// Per device: when it was last signalled, and whether a change came in
/// the window since (owed a signal when the window ends).
#[derive(Default)]
pub struct Signals {
    last: HashMap<String, Instant>,
    owed: HashSet<String>,
}

impl Signals {
    /// The daemon's state moved and `id` has no live session: signal it
    /// now? If not (inside its window), it is owed one when the window ends.
    pub fn change(&mut self, id: &str, now: Instant) -> bool {
        if self.open(id, now) {
            self.sent(id, now);
            true
        } else {
            self.owed.insert(id.to_string());
            false
        }
    }

    /// The devices owed a signal whose window has ended, counted as
    /// signalled now. `skip`: a device that needs none any more (it is
    /// live, so it caught up, or it is gone) is forgotten instead.
    pub fn due(&mut self, now: Instant, skip: impl Fn(&str) -> bool) -> Vec<String> {
        self.owed.retain(|id| !skip(id));
        let due: Vec<String> = self
            .owed
            .iter()
            .filter(|id| self.open(id, now))
            .cloned()
            .collect();
        for id in &due {
            self.sent(id, now);
        }
        due
    }

    /// Its window is over (or it was never signalled).
    fn open(&self, id: &str, now: Instant) -> bool {
        self.last
            .get(id)
            .is_none_or(|t| now.duration_since(*t) >= SIGNAL_EVERY)
    }

    fn sent(&mut self, id: &str, now: Instant) {
        self.last.insert(id.to_string(), now);
        self.owed.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_inside_the_window_is_signalled_when_it_ends() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let none = |_: &str| false;
        let mut s = Signals::default();
        // The first change signals at once; nothing is owed.
        assert!(s.change("a", t0));
        assert!(s.due(at(31), none).is_empty());
        // Two more inside the next window: one signal when it ends.
        assert!(s.change("a", at(40)));
        assert!(!s.change("a", at(45)));
        assert!(!s.change("a", at(50)));
        assert!(s.due(at(69), none).is_empty());
        assert_eq!(s.due(at(70), none), vec!["a".to_string()]);
        assert!(s.due(at(71), none).is_empty());
        // That signal opens a window of its own.
        assert!(!s.change("a", at(80)));
        assert!(s.due(at(99), none).is_empty());
        assert_eq!(s.due(at(100), none), vec!["a".to_string()]);
        // Each device has its own window.
        assert!(s.change("b", at(100)));
        assert!(!s.change("b", at(101)));
        assert!(s.change("c", at(101)));
        assert_eq!(s.due(at(130), none), vec!["b".to_string()]);
    }

    #[test]
    fn a_device_that_caught_up_or_went_is_owed_nothing() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut s = Signals::default();
        assert!(s.change("a", t0));
        assert!(!s.change("a", at(5)));
        // Live at the window's end: forgotten, not signalled later either.
        assert!(s.due(at(30), |id| id == "a").is_empty());
        assert!(s.due(at(31), |_| false).is_empty());
        // Due while it can't be sent (the caller doesn't ask): still owed.
        assert!(s.change("a", at(32)));
        assert!(!s.change("a", at(35)));
        assert_eq!(s.due(at(90), |_| false), vec!["a".to_string()]);
    }
}
