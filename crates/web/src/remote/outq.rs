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

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

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
        if q.frames.is_empty() {
            self.links.remove(&id);
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
}

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
        self.lock().links.remove(&link);
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

    fn next(&self) -> Option<Message> {
        self.lock().pop()
    }

    fn done(&self) -> bool {
        let g = self.lock();
        g.closed && g.control.is_empty()
    }
}

/// The writer: sends what is queued, pings every [`PING_EVERY`], and closes
/// the socket when the queues are shut or the socket fails.
pub async fn write(q: OutQ, mut sink: SplitSink<Ws, Message>) {
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if let Some(m) = q.next() {
            let close = matches!(m, Message::Close(_));
            if sink.send(m).await.is_err() || close {
                break;
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
