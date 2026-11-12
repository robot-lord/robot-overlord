//! One session at a time: a lock per session, held while rlord types into
//! its pane (`send::type_into`, via `send_to_session`, its delivery tick
//! and `overlord.ask`) and while the session is restarted, started fresh,
//! stopped or archived. A paste can't land in a pane that is being
//! replaced or killed, and the checks made before typing (its status, its
//! screen) still hold when the text arrives.
//!
//! Lock order, everywhere: `Daemon::overlord_send` (the overlord only),
//! then this, then the DB. Never take this while holding the DB: a caller
//! that has only a ref resolves it with a short DB hold, lets go, takes
//! this, then takes the DB again. `send`'s `TYPING` comes after all three
//! and is never held while taking any of them.

use std::collections::HashSet;
use std::sync::{Condvar, Mutex};

#[derive(Default)]
pub struct SessionLocks {
    held: Mutex<HashSet<String>>,
    freed: Condvar,
}

/// Held for one session; released when dropped.
pub struct SessionGuard<'a> {
    locks: &'a SessionLocks,
    id: String,
}

impl SessionLocks {
    /// Wait until no one holds session `id` (its row id, not a ref), then
    /// hold it.
    pub fn lock(&self, id: &str) -> SessionGuard<'_> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        while held.contains(id) {
            held = self.freed.wait(held).unwrap_or_else(|e| e.into_inner());
        }
        held.insert(id.to_string());
        SessionGuard {
            locks: self,
            id: id.to_string(),
        }
    }
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        let mut held = self.locks.held.lock().unwrap_or_else(|e| e.into_inner());
        held.remove(&self.id);
        drop(held);
        self.locks.freed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn one_holder_per_session() {
        let locks = Arc::new(SessionLocks::default());
        let g = locks.lock("a");
        // Another session isn't held up.
        drop(locks.lock("b"));
        let l = Arc::clone(&locks);
        let t = std::thread::spawn(move || {
            let start = Instant::now();
            let _g = l.lock("a");
            start.elapsed()
        });
        std::thread::sleep(Duration::from_millis(100));
        drop(g);
        assert!(t.join().unwrap() >= Duration::from_millis(90));
        // Free again.
        drop(locks.lock("a"));
    }
}
