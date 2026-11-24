//! What the host sends its relay, queued per kind and per link, and the
//! task that writes it (`docs/working/e2e-protocol.md`, "Delivery and
//! recovery").
//!
//! One websocket link carries one Noise session per device link, with
//! implicit nonces: a frame that is never written breaks that session. So
//! nothing here is ever dropped quietly. Every queue is bounded, and
//! overflow fails safe: a link's queue that overflows is that link's end
//! (the actor drops it; only that device's session dies), and a control
//! message that can't be queued ends the whole connection (the actor closes
//! it and reconnects; every device does a fresh handshake).
//!
//! The writer sends control messages first, then mailbox frames, then one
//! frame per link in turn, so a device that asks for a lot can't hold up
//! the others (the relay's flow control and rate limit are per connection
//! on its side of the host).
//!
//! **Pacing.** The relay reads a host at its `ready.limits` rate and burst,
//! times one plus the host's links, and reads more slowly past that. Taking
//! turns only works while the backlog is here: written faster than the
//! relay reads, it moves into the sockets' buffers between us, first in,
//! first out, where one link's backlog holds up every other link's frames
//! for as long as those buffers take to drain at the relay's rate (several
//! seconds on Linux, whose buffers grow to megabytes). So the writer keeps
//! just under the relay's rate itself.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::stream::SplitSink;
use futures_util::SinkExt;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

use super::link::Ws;

/// Control messages queued before the connection counts as backed up.
pub const CONTROL_MAX: usize = 256;
/// Mailbox frames queued (one in flight per device, so this is plenty).
pub const MAILBOX_MAX: usize = 64;
/// Bytes queued for one link: four of the largest control messages
/// (1 MiB each) and change.
pub const LINK_BYTES: usize = 4 * 1024 * 1024 + 64 * 1024;
/// The host pings its relay this often (the relay pings too); any message
/// from the relay, a pong included, says it is still there.
pub const PING_EVERY: Duration = Duration::from_secs(15);
/// A pane frame is encrypted only while its link's queue holds less than
/// this (`remote/panes.rs`): frames are encrypted when queued, so a
/// link's queue is first in, first out, and this mark is what keeps an
/// answer from waiting behind a busy pane for long.
pub const LOW_MARK: usize = 32 * 1024;
/// The share of the relay's rate and burst the writer uses, so the relay's
/// own bucket never runs dry (it counts time a little differently).
const PACE: f64 = 0.9;

/// Which queue was full.
#[derive(Debug, PartialEq, Eq)]
pub enum Full {
    Control,
    Mailbox,
    Link(u32),
}

#[derive(Default)]
struct LinkQ {
    /// Its frames, and last perhaps the `drop` that ends it.
    frames: VecDeque<Message>,
    bytes: usize,
}

#[derive(Default)]
struct Inner {
    control: VecDeque<Message>,
    mailbox: VecDeque<Vec<u8>>,
    links: BTreeMap<u32, LinkQ>,
    /// The link served last (round robin).
    last: u32,
    /// No more sending: the writer ends once the control queue (a close
    /// frame, say) is out.
    closed: bool,
    /// The relay's rate and burst for this connection, per (1 + links)
    /// (None: it didn't say, so no pacing).
    limit: Option<(u64, u64)>,
    /// The links the relay counts for us.
    link_count: usize,
    /// Links whose queue someone waits to see fall below [`LOW_MARK`].
    want_low: std::collections::BTreeSet<u32>,
    /// Those that did, for the waker.
    lowered: Vec<u32>,
}

impl Inner {
    fn pop(&mut self) -> Option<Message> {
        if let Some(m) = self.control.pop_front() {
            return Some(m);
        }
        if self.closed {
            return None;
        }
        if let Some(b) = self.mailbox.pop_front() {
            return Some(Message::Binary(b.into()));
        }
        // The next link after the last one served, wrapping round.
        let id = self
            .links
            .range(self.last.saturating_add(1)..)
            .chain(self.links.range(..=self.last))
            .find(|(_, q)| !q.frames.is_empty())
            .map(|(id, _)| *id)?;
        self.last = id;
        let q = self.links.get_mut(&id)?;
        let f = q.frames.pop_front()?;
        if let Message::Binary(b) = &f {
            q.bytes -= b.len();
        }
        let bytes = q.bytes;
        if q.frames.is_empty() {
            self.links.remove(&id);
        }
        if bytes < LOW_MARK && self.want_low.remove(&id) {
            self.lowered.push(id);
        }
        Some(f)
    }
}

/// The queues of one relay connection, shared by the actor (which pushes)
/// and the writer task (which pops).
#[derive(Clone, Default)]
pub struct OutQ {
    inner: Arc<Mutex<Inner>>,
    wake: Arc<Notify>,
    /// Told the link whose queue fell below [`LOW_MARK`] (`want_low`).
    on_low: Arc<Mutex<Option<LowHook>>>,
}

/// What `OutQ` calls when a watched link's queue falls below the mark.
pub type LowHook = Box<dyn Fn(u32) + Send>;

impl OutQ {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, f: impl FnOnce(&mut Inner) -> Result<(), Full>) -> Result<(), Full> {
        let r = {
            let mut g = self.lock();
            if g.closed {
                // The connection is going: what it would carry is lost
                // with it, and the next one starts every session afresh.
                return Ok(());
            }
            f(&mut g)
        };
        if r.is_ok() {
            self.wake.notify_one();
        }
        r
    }

    /// A control message (text).
    pub fn control(&self, m: Message) -> Result<(), Full> {
        self.push(|g| {
            if g.control.len() >= CONTROL_MAX {
                return Err(Full::Control);
            }
            g.control.push_back(m);
            Ok(())
        })
    }

    /// A lane-2 frame.
    pub fn mailbox(&self, frame: Vec<u8>) -> Result<(), Full> {
        self.push(|g| {
            if g.mailbox.len() >= MAILBOX_MAX {
                return Err(Full::Mailbox);
            }
            g.mailbox.push_back(frame);
            Ok(())
        })
    }

    /// A lane-1 frame for `link`.
    pub fn link(&self, link: u32, frame: Vec<u8>) -> Result<(), Full> {
        self.push(|g| {
            let q = g.links.entry(link).or_default();
            if q.bytes + frame.len() > LINK_BYTES {
                return Err(Full::Link(link));
            }
            q.bytes += frame.len();
            q.frames.push_back(Message::Binary(frame.into()));
            Ok(())
        })
    }

    /// A control message that must follow what is queued for `link` (its
    /// `drop`: a pairing's last answer goes out before the link closes).
    pub fn after_link(&self, link: u32, m: Message) -> Result<(), Full> {
        self.push(|g| {
            match g.links.get_mut(&link) {
                Some(q) => q.frames.push_back(m),
                None if g.control.len() >= CONTROL_MAX => return Err(Full::Control),
                None => g.control.push_back(m),
            }
            Ok(())
        })
    }

    /// The link failed: what was queued for it goes (its session is over).
    pub fn forget(&self, link: u32) {
        let mut g = self.lock();
        g.links.remove(&link);
        g.want_low.remove(&link);
    }

    /// Bytes queued for `link` now.
    #[cfg(test)]
    pub fn link_bytes(&self, link: u32) -> usize {
        self.lock().links.get(&link).map_or(0, |q| q.bytes)
    }

    /// Who to tell when a watched link's queue falls below the mark.
    pub fn set_on_low(&self, hook: LowHook) {
        *self.on_low.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// Whether `link`'s queue is below [`LOW_MARK`]; if not, the hook is
    /// called once when it falls below it.
    pub fn below_mark(&self, link: u32) -> bool {
        let mut g = self.lock();
        if g.links.get(&link).map_or(0, |q| q.bytes) < LOW_MARK {
            return true;
        }
        g.want_low.insert(link);
        false
    }

    /// Send `m` (a close frame), then stop.
    pub fn close_with(&self, m: Message) {
        {
            let mut g = self.lock();
            if !g.closed {
                g.control.push_back(m);
                g.closed = true;
            }
        }
        self.wake.notify_one();
    }

    /// Stop now: nothing queued is sent.
    pub fn shut(&self) {
        {
            let mut g = self.lock();
            *g = Inner {
                closed: true,
                ..Inner::default()
            };
        }
        self.wake.notify_one();
    }

    /// The relay's limit for this connection, from its `ready`.
    pub fn set_limit(&self, bytes_per_sec: Option<u64>, burst_bytes: Option<u64>) {
        self.lock().limit = match (bytes_per_sec, burst_bytes) {
            (Some(r), Some(b)) if r > 0 && b > 0 => Some((r, b)),
            _ => None,
        };
    }

    /// How many links this connection carries now (the relay's limit
    /// scales with them).
    pub fn set_links(&self, n: usize) {
        self.lock().link_count = n;
    }

    fn next(&self) -> Option<Message> {
        let (m, lowered) = {
            let mut g = self.lock();
            let m = g.pop();
            (m, std::mem::take(&mut g.lowered))
        };
        if !lowered.is_empty() {
            if let Some(hook) = &*self.on_low.lock().unwrap_or_else(|e| e.into_inner()) {
                for id in lowered {
                    hook(id);
                }
            }
        }
        m
    }

    /// Count `n` bytes sent; how long to wait before sending more.
    fn spent(&self, bucket: &mut Bucket, n: usize) -> Duration {
        let (limit, links) = {
            let g = self.lock();
            (g.limit, g.link_count)
        };
        match limit {
            Some((rate, burst)) => bucket.take(n, rate, burst, 1 + links, Instant::now()),
            None => Duration::ZERO,
        }
    }

    fn done(&self) -> bool {
        let g = self.lock();
        g.closed && g.control.is_empty()
    }
}

/// The relay's token bucket for a host, as the writer keeps it (`PACE`
/// of the rate and burst): it starts full, like the relay's.
#[derive(Default)]
struct Bucket {
    /// Tokens and when they were counted; None before the first send.
    state: Option<(f64, Instant)>,
}

impl Bucket {
    /// Take `n` bytes at `scale` times `rate` and `burst`; how long to
    /// wait before the next send.
    fn take(&mut self, n: usize, rate: u64, burst: u64, scale: usize, now: Instant) -> Duration {
        let scale = scale as f64 * PACE;
        let (rate, burst) = (rate as f64 * scale, burst as f64 * scale);
        let tokens = match self.state {
            Some((t, at)) => (t + now.duration_since(at).as_secs_f64() * rate).min(burst),
            None => burst,
        } - n as f64;
        self.state = Some((tokens, now));
        if tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-tokens / rate)
        }
    }
}

/// The relay counts these bytes against a host's rate.
fn counted(m: &Message) -> usize {
    match m {
        Message::Text(t) => t.len(),
        Message::Binary(b) => b.len(),
        _ => 0,
    }
}

/// The writer: sends what is queued, no faster than the relay reads it,
/// pings every [`PING_EVERY`], and closes the socket when the queues are
/// shut or the socket fails.
pub async fn write(q: OutQ, mut sink: SplitSink<Ws, Message>) {
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut bucket = Bucket::default();
    loop {
        if let Some(m) = q.next() {
            let close = matches!(m, Message::Close(_));
            let n = counted(&m);
            if sink.send(m).await.is_err() || close {
                break;
            }
            // Wait here, not with the next frame popped: whichever link's
            // turn it is then goes next.
            let wait = q.spent(&mut bucket, n);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            continue;
        }
        if q.done() {
            break;
        }
        tokio::select! {
            _ = q.wake.notified() => {}
            _ = ping.tick() => {
                if sink.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = sink.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin(m: Option<Message>) -> Vec<u8> {
        match m {
            Some(Message::Binary(b)) => b.to_vec(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn control_first_then_mailbox_then_links_in_turn() {
        let q = OutQ::default();
        for i in 0..3u8 {
            q.link(1, vec![10 + i]).unwrap();
        }
        q.link(2, vec![20]).unwrap();
        q.link(3, vec![30]).unwrap();
        q.mailbox(vec![99]).unwrap();
        q.control(Message::Text("c".into())).unwrap();
        assert!(matches!(q.next(), Some(Message::Text(_))));
        assert_eq!(bin(q.next()), vec![99]);
        let order: Vec<u8> = std::iter::from_fn(|| q.next())
            .map(|m| bin(Some(m))[0])
            .collect();
        // One frame per link in turn: link 1's burst doesn't hold up 2 or 3.
        assert_eq!(order, vec![10, 20, 30, 11, 12]);
        // A link's drop goes after its frames, not ahead with control.
        q.link(4, vec![40]).unwrap();
        q.after_link(4, Message::Text("drop 4".into())).unwrap();
        q.after_link(5, Message::Text("drop 5".into())).unwrap();
        assert!(matches!(q.next(), Some(Message::Text(t)) if t.as_str() == "drop 5"));
        assert_eq!(bin(q.next()), vec![40]);
        assert!(matches!(q.next(), Some(Message::Text(t)) if t.as_str() == "drop 4"));
    }

    #[test]
    fn overflow_names_the_queue() {
        let q = OutQ::default();
        let big = vec![0u8; 1024 * 1024];
        for _ in 0..4 {
            q.link(7, big.clone()).unwrap();
        }
        assert_eq!(q.link(7, big.clone()), Err(Full::Link(7)));
        // Another link is untouched by link 7's backlog.
        q.link(8, big.clone()).unwrap();
        q.forget(7);
        q.link(7, big).unwrap();
        for _ in 0..CONTROL_MAX {
            q.control(Message::Text("x".into())).unwrap();
        }
        assert_eq!(q.control(Message::Text("x".into())), Err(Full::Control));
        for _ in 0..MAILBOX_MAX {
            q.mailbox(vec![1]).unwrap();
        }
        assert_eq!(q.mailbox(vec![1]), Err(Full::Mailbox));
    }

    #[test]
    fn the_writer_keeps_under_the_relays_rate() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut b = Bucket::default();
        // It starts full: 90% of a burst of 1000 goes at once.
        assert_eq!(b.take(800, 1000, 1000, 1, t0), Duration::ZERO);
        // 100 bytes over 900: a wait at 900 bytes a second.
        let w = b.take(200, 1000, 1000, 1, t0);
        assert!((w.as_secs_f64() - 100.0 / 900.0).abs() < 1e-9, "{w:?}");
        // Refilled meanwhile, but never past the burst.
        assert_eq!(b.take(900, 1000, 1000, 1, at(10_000)), Duration::ZERO);
        assert!(b.take(1, 1000, 1000, 1, at(10_000)) > Duration::ZERO);
        // More links: the relay reads faster, and so does the writer.
        let mut one = Bucket::default();
        let mut three = Bucket::default();
        one.take(900, 1000, 1000, 1, t0);
        three.take(2700, 1000, 1000, 3, t0);
        assert_eq!(
            one.take(900, 1000, 1000, 1, t0),
            three.take(2700, 1000, 1000, 3, t0)
        );
        // No limit from the relay: no pacing.
        let q = OutQ::default();
        assert_eq!(q.spent(&mut Bucket::default(), 1 << 30), Duration::ZERO);
        q.set_limit(Some(1000), Some(1000));
        q.set_links(1);
        assert!(q.spent(&mut Bucket::default(), 1 << 20) > Duration::from_secs(500));
    }

    #[test]
    fn a_links_queue_reports_its_bytes() {
        let q = OutQ::default();
        assert_eq!(q.link_bytes(1), 0);
        q.link(1, vec![0; 1000]).unwrap();
        q.link(1, vec![0; 24]).unwrap();
        q.link(2, vec![0; 5]).unwrap();
        assert_eq!(q.link_bytes(1), 1024);
        q.next();
        q.next();
        assert_eq!(q.link_bytes(1), 24);
        q.forget(1);
        assert_eq!(q.link_bytes(1), 0);
    }

    #[test]
    fn the_low_mark_wakes_once() {
        let q = OutQ::default();
        let woke = Arc::new(Mutex::new(Vec::new()));
        let w = woke.clone();
        q.set_on_low(Box::new(move |id| w.lock().unwrap().push(id)));
        // Below the mark: no wait, no wake.
        assert!(q.below_mark(3));
        for _ in 0..3 {
            q.link(3, vec![0; LOW_MARK / 2]).unwrap();
        }
        assert!(!q.below_mark(3));
        assert!(!q.below_mark(3));
        q.next(); // 1 frame out, 1.0 mark left: still at the mark
        assert!(woke.lock().unwrap().is_empty());
        q.next(); // below it now
        assert_eq!(*woke.lock().unwrap(), vec![3]);
        q.next();
        // Once: nobody asked again.
        assert_eq!(*woke.lock().unwrap(), vec![3]);
    }

    #[test]
    fn closing_sends_the_close_frame_and_nothing_after() {
        let q = OutQ::default();
        q.link(1, vec![1]).unwrap();
        q.close_with(Message::Close(None));
        q.link(1, vec![2]).unwrap();
        assert!(matches!(q.next(), Some(Message::Close(None))));
        assert!(q.next().is_none() && q.done());
        let q = OutQ::default();
        q.control(Message::Text("x".into())).unwrap();
        q.shut();
        assert!(q.next().is_none() && q.done());
    }
}
