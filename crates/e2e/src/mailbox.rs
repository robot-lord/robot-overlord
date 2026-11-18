//! The mailbox session: a saveable, one-message-at-a-time pipe for when the
//! other end is away.
//!
//! Keyed by a kind-3 handshake (`KKpsk2`, run over a live link). The two raw
//! keys come from snow's `dangerously_get_raw_split`, and the mailbox runs
//! its own Noise cipher state over them: ChaCha20-Poly1305 (the version snow
//! resolves), the nonce a 64-bit counter (4 zero bytes, then the counter
//! little-endian), empty associated data, and Noise's `REKEY` after every
//! message (the key becomes the first 32 bytes of encrypting 32 zero bytes
//! under nonce 2^64-1; the counter carries on). That is exactly what snow's
//! `TransportState` does with `rekey_outgoing`/`rekey_incoming` after each
//! message, which the vectors check; owning the state is what makes it
//! saveable: [`MailboxSession::export`] and [`MailboxSession::import`].
//!
//! Frames are the live session's shape (`u16 BE length | ciphertext`, the
//! plaintext `u16 BE length | content | zero fill`) padded to a Padmé bucket,
//! since a mailbox message's size would otherwise name its type.
//!
//! **Order and failure.** The relay's queue is ordered, so the counters stay
//! in step. A frame that fails to decrypt (tampered, replayed, a gap) poisons
//! the session; both ends discard it and the next live link keys a new one.
//!
//! **Save order** (the owner's job): the sender saves the advanced state
//! before handing the frame out; the receiver saves after it has acted on
//! the message. A crash in between loses at most that message, which the
//! sender resends by id.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::session::{ciphertext, pad_into, padme_len, unpad};
use crate::{Error, Result, MAX_PLAINTEXT, TAG_LEN, VERSION};

/// Length of [`MailboxSession::export`]'s bytes: version, the id, then key
/// and counter for each direction.
pub const MAILBOX_EXPORT_LEN: usize = 1 + 8 + 2 * (32 + 8);

/// One direction's Noise cipher state.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
struct CipherState {
    k: [u8; 32],
    n: u64,
}

fn nonce(n: u64) -> Nonce {
    let mut b = [0u8; 12];
    b[4..].copy_from_slice(&n.to_le_bytes());
    b.into()
}

impl CipherState {
    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(Key::from_slice(&self.k))
    }

    /// Noise's ENCRYPTWITHAD with empty ad: `buf` is replaced by the
    /// ciphertext, the tag appended.
    fn encrypt(&mut self, buf: &mut Vec<u8>) -> Result<()> {
        if self.n == u64::MAX {
            return Err(Error::Noise("nonce exhausted".into()));
        }
        let tag = self
            .cipher()
            .encrypt_in_place_detached(&nonce(self.n), &[], buf)
            .map_err(|_| Error::Noise("encrypt".into()))?;
        buf.extend_from_slice(&tag);
        self.n += 1;
        Ok(())
    }

    fn decrypt(&mut self, ct: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        if self.n == u64::MAX {
            return Err(Error::Decrypt);
        }
        if ct.len() < TAG_LEN {
            return Err(Error::Format("frame too short"));
        }
        let (body, tag) = ct.split_at(ct.len() - TAG_LEN);
        let mut out = Zeroizing::new(body.to_vec());
        self.cipher()
            .decrypt_in_place_detached(&nonce(self.n), &[], &mut out, Tag::from_slice(tag))
            .map_err(|_| Error::Decrypt)?;
        self.n += 1;
        Ok(out)
    }

    /// Noise's REKEY: k = ENCRYPT(k, 2^64-1, empty, 32 zero bytes)[..32].
    fn rekey(&mut self) {
        let mut z = Zeroizing::new([0u8; 32]);
        // Encrypting a fixed-size buffer cannot fail.
        let _ = self
            .cipher()
            .encrypt_in_place_detached(&nonce(u64::MAX), &[], &mut z[..]);
        self.k.copy_from_slice(&z[..]);
    }
}

/// The mailbox session of one host and one device.
pub struct MailboxSession {
    id: [u8; 8],
    send: CipherState,
    recv: CipherState,
    poisoned: bool,
}

impl MailboxSession {
    pub(crate) fn from_keys(id: [u8; 8], send: [u8; 32], recv: [u8; 32]) -> Self {
        Self {
            id,
            send: CipherState { k: send, n: 0 },
            recv: CipherState { k: recv, n: 0 },
            poisoned: false,
        }
    }

    /// Which mailbox this is: both ends derive the same 8 bytes from the
    /// keying handshake. The owner sends it in clear beside each frame, so
    /// a frame queued under a mailbox that has since been replaced is
    /// dropped by id instead of poisoning the new one. Not secret.
    pub fn id(&self) -> [u8; 8] {
        self.id
    }

    /// True once a decrypt has failed: discard this session.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// How many messages this side has sent and received.
    pub fn counters(&self) -> (u64, u64) {
        (self.send.n, self.recv.n)
    }

    /// Encrypt one message (at most `MAX_PLAINTEXT` bytes) into a wire frame.
    /// The state advances and rekeys: save it before handing the frame out.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        if self.poisoned {
            return Err(Error::Decrypt);
        }
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(Error::Format("frame too large"));
        }
        let padded = padme_len(2 + plaintext.len());
        let mut buf = vec![0u8; padded];
        pad_into(&mut buf, plaintext, padded);
        buf.reserve(2 + TAG_LEN);
        self.send.encrypt(&mut buf)?;
        self.send.rekey();
        let mut frame = Vec::with_capacity(2 + buf.len());
        frame.extend_from_slice(&(buf.len() as u16).to_be_bytes());
        frame.extend_from_slice(&buf);
        Ok(frame)
    }

    /// Decrypt the next message. Any failure poisons the session.
    pub fn decrypt(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        if self.poisoned {
            return Err(Error::Decrypt);
        }
        let r = self.try_decrypt(frame);
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    fn try_decrypt(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        let ct = ciphertext(frame)?;
        let plain = self.recv.decrypt(ct)?;
        self.recv.rekey();
        let n = unpad(&plain, padme_len)?;
        Ok(plain[2..2 + n].to_vec())
    }

    /// The state to store: version, then each direction's key and counter
    /// (send first). Secret: store it as a private key.
    pub fn export(&self) -> Result<Vec<u8>> {
        if self.poisoned {
            return Err(Error::Decrypt);
        }
        let mut b = Vec::with_capacity(MAILBOX_EXPORT_LEN);
        b.push(VERSION);
        b.extend_from_slice(&self.id);
        for c in [&self.send, &self.recv] {
            b.extend_from_slice(&c.k);
            b.extend_from_slice(&c.n.to_be_bytes());
        }
        Ok(b)
    }

    /// A session from [`MailboxSession::export`]'s bytes.
    pub fn import(bytes: &[u8]) -> Result<MailboxSession> {
        if bytes.len() != MAILBOX_EXPORT_LEN {
            return Err(Error::Format("mailbox length"));
        }
        if bytes[0] != VERSION {
            return Err(Error::Format("mailbox version"));
        }
        let state = |off: usize| CipherState {
            k: bytes[off..off + 32].try_into().unwrap(),
            n: u64::from_be_bytes(bytes[off + 32..off + 40].try_into().unwrap()),
        };
        Ok(MailboxSession {
            id: bytes[1..9].try_into().unwrap(),
            send: state(9),
            recv: state(49),
            poisoned: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_import_round_trip() {
        let mut a = MailboxSession::from_keys([5; 8], [1; 32], [2; 32]);
        let mut b = MailboxSession::from_keys([5; 8], [2; 32], [1; 32]);
        for i in 0..5u8 {
            let f = a.encrypt(&[i; 10]).unwrap();
            assert_eq!(b.decrypt(&f).unwrap(), vec![i; 10]);
            // Save and restore both ends between every message.
            a = MailboxSession::import(&a.export().unwrap()).unwrap();
            b = MailboxSession::import(&b.export().unwrap()).unwrap();
        }
        assert_eq!(a.counters(), (5, 0));
        assert_eq!(b.counters(), (0, 5));
        let f = b.encrypt(b"back").unwrap();
        assert_eq!(a.decrypt(&f).unwrap(), b"back");
        assert!(MailboxSession::import(&[1; 10]).is_err());
        let mut e = a.export().unwrap();
        e[0] = 9;
        assert!(MailboxSession::import(&e).is_err());
    }

    #[test]
    fn a_gap_poisons() {
        let mut a = MailboxSession::from_keys([5; 8], [1; 32], [2; 32]);
        let mut b = MailboxSession::from_keys([5; 8], [2; 32], [1; 32]);
        let _lost = a.encrypt(b"one").unwrap();
        let two = a.encrypt(b"two").unwrap();
        assert_eq!(b.decrypt(&two), Err(Error::Decrypt));
        assert!(b.is_poisoned());
        assert!(b.export().is_err());
        assert_eq!(b.encrypt(b"x"), Err(Error::Decrypt));
    }

    #[test]
    fn padme_sizes() {
        let mut a = MailboxSession::from_keys([5; 8], [1; 32], [2; 32]);
        let f = a.encrypt(&[0; 998]).unwrap();
        assert_eq!(f.len(), crate::mailbox_wire_len(998));
        assert_eq!(f.len(), 2 + 1024 + TAG_LEN);
    }
}
