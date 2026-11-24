//! Streams inside a live session, and fragmenting.
//!
//! The first plaintext byte of a frame is a stream id: [`CONTROL`] (0) is
//! the control stream (the local control socket's JSON messages);
//! [`PANE_FIRST`]..=[`PANE_LAST`] (1 to 250) are pane streams (a terminal's
//! bytes, opened by the `pane.open` op on the control stream); 251 to 255
//! are reserved and refused. The second byte is a flags byte: bit 0
//! ([`MORE`]) means "more of this message follows", so a control message
//! larger than one frame (a snapshot) is sent as fragments and reassembled,
//! up to [`MAX_MESSAGE`]. A pane frame stands alone: "more" on a pane
//! stream is refused, and its bytes are a message by themselves (at most
//! [`CHUNK`]). Other flag bits must be zero.
//!
//! The relay cannot tell streams apart: the header is inside the
//! ciphertext.

use std::collections::BTreeMap;

use crate::{Error, Result, MAX_PLAINTEXT};

/// The control stream.
pub const CONTROL: u8 = 0;
/// The first pane stream.
pub const PANE_FIRST: u8 = 1;
/// The last pane stream (251 to 255 are reserved).
pub const PANE_LAST: u8 = 250;
/// Flags bit 0: more of this message follows.
pub const MORE: u8 = 1;
/// Stream id and flags.
pub const HEADER_LEN: usize = 2;
/// Largest message after reassembly (the local control socket's frame
/// limit).
pub const MAX_MESSAGE: usize = 1 << 20;
/// Largest message chunk one frame carries.
pub const CHUNK: usize = MAX_PLAINTEXT - HEADER_LEN;

/// One whole message on a stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub stream: u8,
    pub body: Vec<u8>,
}

/// Split `msg` into frame plaintexts for `stream`, each at most
/// `MAX_PLAINTEXT` bytes. An empty message is one frame with only a header.
pub fn fragment(stream: u8, msg: &[u8]) -> Result<Vec<Vec<u8>>> {
    if msg.len() > MAX_MESSAGE {
        return Err(Error::Format("message too large"));
    }
    let mut out = Vec::with_capacity(msg.len() / CHUNK + 1);
    let mut chunks = msg.chunks(CHUNK).peekable();
    if chunks.peek().is_none() {
        return Ok(vec![vec![stream, 0]]);
    }
    while let Some(c) = chunks.next() {
        let more = chunks.peek().is_some();
        let mut f = Vec::with_capacity(HEADER_LEN + c.len());
        f.push(stream);
        f.push(if more { MORE } else { 0 });
        f.extend_from_slice(c);
        out.push(f);
    }
    Ok(out)
}

/// Whether `stream` is a pane stream.
pub fn is_pane(stream: u8) -> bool {
    (PANE_FIRST..=PANE_LAST).contains(&stream)
}

/// One pane frame's plaintext: `stream | 0 | bytes`, at most [`CHUNK`]
/// bytes (a pane frame stands alone; split longer input yourself).
pub fn pane_frame(stream: u8, bytes: &[u8]) -> Result<Vec<u8>> {
    if !is_pane(stream) {
        return Err(Error::Format("not a pane stream"));
    }
    if bytes.len() > CHUNK {
        return Err(Error::Format("pane frame too long"));
    }
    let mut f = Vec::with_capacity(HEADER_LEN + bytes.len());
    f.push(stream);
    f.push(0);
    f.extend_from_slice(bytes);
    Ok(f)
}

/// Reads one frame plaintext that must hold a whole message (the mailbox:
/// one message per frame).
pub fn single(frame: &[u8]) -> Result<Message> {
    let mut r = Reassembler::new();
    r.push(frame)?
        .ok_or(Error::Format("a fragment where a whole message was due"))
}

/// Puts fragments back together, per stream.
#[derive(Default)]
pub struct Reassembler {
    partial: BTreeMap<u8, Vec<u8>>,
}

impl Reassembler {
    pub fn new() -> Reassembler {
        Reassembler::default()
    }

    /// One frame's plaintext: a whole message once its last fragment is in.
    pub fn push(&mut self, frame: &[u8]) -> Result<Option<Message>> {
        let [stream, flags, chunk @ ..] = frame else {
            return Err(Error::Format("stream header"));
        };
        if flags & !MORE != 0 {
            return Err(Error::Format("stream flags"));
        }
        if *stream > PANE_LAST {
            return Err(Error::Format("reserved stream"));
        }
        if is_pane(*stream) {
            if flags & MORE != 0 {
                return Err(Error::Format("more on a pane stream"));
            }
            return Ok(Some(Message {
                stream: *stream,
                body: chunk.to_vec(),
            }));
        }
        let buf = self.partial.entry(*stream).or_default();
        if buf.len() + chunk.len() > MAX_MESSAGE {
            self.partial.remove(stream);
            return Err(Error::Format("message too large"));
        }
        buf.extend_from_slice(chunk);
        if flags & MORE != 0 {
            return Ok(None);
        }
        let body = self.partial.remove(stream).unwrap_or_default();
        Ok(Some(Message {
            stream: *stream,
            body,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for n in [
            0,
            1,
            CHUNK - 1,
            CHUNK,
            CHUNK + 1,
            3 * CHUNK + 7,
            MAX_MESSAGE,
        ] {
            let msg: Vec<u8> = (0..n).map(|i| i as u8).collect();
            let frames = fragment(CONTROL, &msg).unwrap();
            assert!(frames.iter().all(|f| f.len() <= MAX_PLAINTEXT));
            assert_eq!(frames.len(), n.div_ceil(CHUNK).max(1));
            let mut r = Reassembler::new();
            let mut got = None;
            for (i, f) in frames.iter().enumerate() {
                let m = r.push(f).unwrap();
                assert_eq!(m.is_some(), i == frames.len() - 1);
                got = m.or(got);
            }
            assert_eq!(
                got.unwrap(),
                Message {
                    stream: CONTROL,
                    body: msg
                }
            );
        }
    }

    #[test]
    fn rejects() {
        assert!(fragment(CONTROL, &vec![0; MAX_MESSAGE + 1]).is_err());
        let mut r = Reassembler::new();
        assert!(r.push(&[0]).is_err());
        assert!(r.push(&[0, 2, 1]).is_err());
        assert!(r.push(&[0, 0x81]).is_err());
        // Over the limit by fragments.
        let mut r = Reassembler::new();
        let big = vec![0u8; CHUNK + HEADER_LEN];
        let mut f = big.clone();
        f[1] = MORE;
        let mut err = false;
        for _ in 0..(MAX_MESSAGE / CHUNK + 2) {
            if r.push(&f).is_err() {
                err = true;
                break;
            }
        }
        assert!(err);
        assert!(single(&[0, MORE, 1]).is_err());
        assert_eq!(single(&[0, 0, 7]).unwrap().body, vec![7]);
    }

    #[test]
    fn control_fragments_and_pane_frames_interleave() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&[0, MORE, 1]).unwrap(), None);
        assert_eq!(
            r.push(&[1, 0, 9]).unwrap(),
            Some(Message {
                stream: 1,
                body: vec![9]
            })
        );
        assert_eq!(
            r.push(&[0, 0, 2]).unwrap(),
            Some(Message {
                stream: 0,
                body: vec![1, 2]
            })
        );
    }

    #[test]
    fn pane_frames_stand_alone() {
        let mut r = Reassembler::new();
        for s in [PANE_FIRST, 7, PANE_LAST] {
            let bytes = vec![s; CHUNK];
            let f = pane_frame(s, &bytes).unwrap();
            assert_eq!(f.len(), crate::MAX_PLAINTEXT);
            assert_eq!(&f[..2], &[s, 0]);
            assert_eq!(
                r.push(&f).unwrap(),
                Some(Message {
                    stream: s,
                    body: bytes
                })
            );
        }
        // An empty pane frame is a message too (nothing to write).
        assert_eq!(r.push(&[3, 0]).unwrap().unwrap().body, Vec::<u8>::new());
        assert!(pane_frame(1, &vec![0; CHUNK + 1]).is_err());
        assert!(pane_frame(CONTROL, b"x").is_err());
        assert!(pane_frame(PANE_LAST + 1, b"x").is_err());
        assert!(is_pane(1) && is_pane(250) && !is_pane(0) && !is_pane(251));
    }

    #[test]
    fn pane_more_is_refused() {
        let mut r = Reassembler::new();
        assert!(r.push(&[1, MORE, 1]).is_err());
        assert!(r.push(&[PANE_LAST, MORE]).is_err());
    }

    #[test]
    fn reserved_streams_refused() {
        let mut r = Reassembler::new();
        for s in 251..=255u8 {
            assert!(r.push(&[s, 0, 1]).is_err(), "{s}");
            assert!(r.push(&[s, MORE, 1]).is_err(), "{s}");
        }
    }
}
