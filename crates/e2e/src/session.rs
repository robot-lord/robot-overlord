//! The live session after a handshake: padded frames, encrypted and
//! decrypted in a caller's buffer, singly or in batches, with rekeying.
//!
//! Wire frame: `u16 BE ciphertext length | ciphertext`. The plaintext inside
//! is `u16 BE content length | content | zero fill`, padded to a multiple of
//! 32 bytes (live) or a Padmé bucket (mailbox, `mailbox.rs`).
//!
//! Batch formats: plaintexts as `u16 BE length | bytes`, repeated; wire
//! frames concatenated as they are.
//!
//! **Any decrypt failure ends the session.** snow does not advance its nonce
//! after a failed decrypt, so carrying on would make a tampered frame an
//! oracle. The session poisons itself and every later call returns
//! [`Error::Decrypt`]; the owner closes the link and re-handshakes.

use snow::TransportState;

use crate::{Error, Result, LIVE_PAD, MAX_PLAINTEXT, TAG_LEN};

pub const fn live_padded_len(content: usize) -> usize {
    content.div_ceil(LIVE_PAD) * LIVE_PAD
}

/// Padmé (Nikitin et al. 2019): at most about 12% overhead, leaks
/// O(log log L) bits of the length.
pub const fn padme_len(l: usize) -> usize {
    if l < 2 {
        return l;
    }
    let e = usize::BITS - 1 - l.leading_zeros(); // floor(log2 l)
    let s = u32::BITS - e.leading_zeros(); // floor(log2 e) + 1
    let last_bits = e - s;
    let mask = (1usize << last_bits) - 1;
    (l + mask) & !mask
}

/// Wire length of a live frame carrying `plain_len` bytes.
pub fn live_wire_len(plain_len: usize) -> usize {
    2 + live_padded_len(2 + plain_len) + TAG_LEN
}

/// Wire length of a mailbox frame carrying `plain_len` bytes.
pub fn mailbox_wire_len(plain_len: usize) -> usize {
    2 + padme_len(2 + plain_len) + TAG_LEN
}

/// Largest padded plaintext either kind of session produces.
pub(crate) const MAX_PADDED: usize = {
    let a = live_padded_len(2 + MAX_PLAINTEXT);
    let b = padme_len(2 + MAX_PLAINTEXT);
    if a > b {
        a
    } else {
        b
    }
};

/// Writes `u16 len | content | zero fill` into `out[..padded]`.
pub(crate) fn pad_into(out: &mut [u8], content: &[u8], padded: usize) {
    out[..2].copy_from_slice(&(content.len() as u16).to_be_bytes());
    out[2..2 + content.len()].copy_from_slice(content);
    out[2 + content.len()..padded].fill(0);
}

/// Checks a decrypted padded plaintext of length `p` and returns the content
/// length (the content is at `[2..2 + n]`).
pub(crate) fn unpad(plain: &[u8], padded_for: impl Fn(usize) -> usize) -> Result<usize> {
    let p = plain.len();
    if p < 2 {
        return Err(Error::Format("inner length"));
    }
    let n = u16::from_be_bytes([plain[0], plain[1]]) as usize;
    if 2 + n > p || padded_for(2 + n) != p || plain[2 + n..].iter().any(|&b| b != 0) {
        return Err(Error::Format("padding"));
    }
    Ok(n)
}

/// Checks a wire frame's outer length and returns the ciphertext.
pub(crate) fn ciphertext(frame: &[u8]) -> Result<&[u8]> {
    if frame.len() < 2 + TAG_LEN {
        return Err(Error::Format("frame too short"));
    }
    let ct = u16::from_be_bytes([frame[0], frame[1]]) as usize;
    if ct + 2 != frame.len() || ct > MAX_PADDED + TAG_LEN {
        return Err(Error::Format("frame length"));
    }
    Ok(&frame[2..])
}

/// A live session: fresh keys per connection, in memory only.
pub struct Session {
    ts: TransportState,
    rekey_every: u64,
    sent: u64,
    received: u64,
    poisoned: bool,
    scratch: Box<[u8]>,
}

impl Session {
    pub(crate) fn new(ts: TransportState, rekey_every: u64) -> Self {
        Self {
            ts,
            rekey_every: rekey_every.max(1),
            sent: 0,
            received: 0,
            poisoned: false,
            scratch: vec![0u8; MAX_PADDED + TAG_LEN].into_boxed_slice(),
        }
    }

    /// True once a decrypt has failed: the session is over.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Encrypt `buf[..len]` and write the wire frame to `buf[..returned]`.
    /// `buf` must hold `live_wire_len(len)` bytes.
    pub fn encrypt_in_place(&mut self, buf: &mut [u8], len: usize) -> Result<usize> {
        if self.poisoned {
            return Err(Error::Decrypt);
        }
        if len > MAX_PLAINTEXT {
            return Err(Error::Format("frame too large"));
        }
        let padded = live_padded_len(2 + len);
        let wire = 2 + padded + TAG_LEN;
        if buf.len() < wire {
            return Err(Error::Format("buffer too small"));
        }
        pad_into(&mut self.scratch, &buf[..len], padded);
        let ct = self
            .ts
            .write_message(&self.scratch[..padded], &mut buf[2..wire])?;
        debug_assert_eq!(ct, padded + TAG_LEN);
        buf[..2].copy_from_slice(&(ct as u16).to_be_bytes());
        self.sent += 1;
        if self.sent.is_multiple_of(self.rekey_every) {
            self.ts.rekey_outgoing();
        }
        Ok(wire)
    }

    /// Decrypt the wire frame in `buf[..len]`; the plaintext ends up in
    /// `buf[..returned]`.
    pub fn decrypt_in_place(&mut self, buf: &mut [u8], len: usize) -> Result<usize> {
        let n = self.decrypt_to_scratch(&buf[..len])?;
        buf[..n].copy_from_slice(&self.scratch[2..2 + n]);
        Ok(n)
    }

    fn decrypt_to_scratch(&mut self, frame: &[u8]) -> Result<usize> {
        if self.poisoned {
            return Err(Error::Decrypt);
        }
        let r = self.try_decrypt(frame);
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    fn try_decrypt(&mut self, frame: &[u8]) -> Result<usize> {
        let ct = ciphertext(frame)?;
        let p = self.ts.read_message(ct, &mut self.scratch)?;
        self.received += 1;
        if self.received.is_multiple_of(self.rekey_every) {
            self.ts.rekey_incoming();
        }
        unpad(&self.scratch[..p], live_padded_len)
    }

    /// Encrypt every `u16 len | bytes` record of `input` into wire frames in
    /// `output`. Returns bytes written.
    pub fn encrypt_batch(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let (mut i, mut o) = (0, 0);
        while i < input.len() {
            if i + 2 > input.len() {
                return Err(Error::Format("batch record header"));
            }
            let n = u16::from_be_bytes([input[i], input[i + 1]]) as usize;
            i += 2;
            if i + n > input.len() {
                return Err(Error::Format("batch record length"));
            }
            let wire = live_wire_len(n);
            if o + wire > output.len() {
                return Err(Error::Format("batch output full"));
            }
            output[o..o + n].copy_from_slice(&input[i..i + n]);
            o += self.encrypt_in_place(&mut output[o..o + wire], n)?;
            i += n;
        }
        Ok(o)
    }

    /// Decrypt concatenated wire frames from `input` into `u16 len | bytes`
    /// records in `output`. Returns bytes written. Stops at the first
    /// failure, which ends the session.
    pub fn decrypt_batch(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let r = self.try_decrypt_batch(input, output);
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    fn try_decrypt_batch(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let (mut i, mut o) = (0, 0);
        while i < input.len() {
            if i + 2 > input.len() {
                return Err(Error::Format("batch frame header"));
            }
            let ct = u16::from_be_bytes([input[i], input[i + 1]]) as usize;
            if i + 2 + ct > input.len() {
                return Err(Error::Format("batch frame length"));
            }
            let n = self.decrypt_to_scratch(&input[i..i + 2 + ct])?;
            if o + 2 + n > output.len() {
                return Err(Error::Format("batch output full"));
            }
            output[o..o + 2].copy_from_slice(&(n as u16).to_be_bytes());
            output[o + 2..o + 2 + n].copy_from_slice(&self.scratch[2..2 + n]);
            o += 2 + n;
            i += 2 + ct;
        }
        Ok(o)
    }

    /// Allocating convenience form.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; live_wire_len(plaintext.len())];
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(Error::Format("frame too large"));
        }
        buf[..plaintext.len()].copy_from_slice(plaintext);
        let n = self.encrypt_in_place(&mut buf, plaintext.len())?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Allocating convenience form.
    pub fn decrypt(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        let n = self.decrypt_to_scratch(frame)?;
        Ok(self.scratch[2..2 + n].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padme_values() {
        assert_eq!(padme_len(0), 0);
        assert_eq!(padme_len(1), 1);
        assert_eq!(padme_len(100), 104);
        assert_eq!(padme_len(1000), 1024);
        assert_eq!(padme_len(16386), 17408);
        for l in 2..20000 {
            let p = padme_len(l);
            assert!(p >= l && (p - l) * 100 <= l * 12 + 100, "{l} -> {p}");
        }
    }

    #[test]
    fn live_padding() {
        assert_eq!(live_padded_len(3), 32);
        assert_eq!(live_padded_len(32), 32);
        assert_eq!(live_padded_len(33), 64);
        assert_eq!(live_wire_len(1), 2 + 32 + 16);
    }
}
