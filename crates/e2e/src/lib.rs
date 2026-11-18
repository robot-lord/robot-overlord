//! rlord's end-to-end crypto core (`docs/working/e2e-crypto.md`; the
//! protocol in full: `docs/working/e2e-protocol.md`).
//!
//! Sans-IO: no sockets, no files, no clock. Every party that holds content
//! keys (the host in `rlord-web`, a device natively or in WASM) runs this
//! same code. The relay never links it.
//!
//! - Pairing (kind 1): `Noise_IKpsk2_25519_ChaChaPoly_SHA256`, PSK = hash of
//!   the QR's pairing secret, an ML-KEM-768 shared secret and its
//!   ciphertext.
//! - Live reconnect (kind 2) and mailbox keying (kind 3):
//!   `Noise_KKpsk2_…`, PSK = hash of the ML-KEM-768 shared secret and its
//!   ciphertext.
//! - The relay identity this connection proved (origin and key
//!   fingerprint) is in every prologue, so a handshake routed through
//!   another relay fails on both ends.
//! - A live [`Session`] carries [`stream`]s; a [`MailboxSession`] is the
//!   saveable one-message-at-a-time pipe.
//!
//! **UniFFI-ready.** The public surface keeps to what UniFFI's macros can
//! export later without a rewrite: owned arguments and returns (`Vec<u8>`,
//! `String`, fixed arrays), no lifetimes or generics in public signatures,
//! constructors returning `Result`, one flat [`Error`] enum, state machines
//! as owned objects consumed or mutated by methods. The WASM wrapper
//! (`crates/e2e-wasm`) is the first consumer held to it.
#![forbid(unsafe_code)]

mod handshake;
mod kem;
mod mailbox;
mod qr;
mod session;
pub mod stream;
#[cfg(feature = "vectors")]
pub mod vectors;

pub use handshake::{Established, FixedRandom, Initiator, Kind, Responder};
pub use kem::{KEM_BACKEND, KEM_CT_LEN, KEM_EK_LEN};
pub use mailbox::{MailboxSession, MAILBOX_EXPORT_LEN};
pub use qr::QrPayload;
pub use session::{live_padded_len, live_wire_len, mailbox_wire_len, padme_len, Session};

use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// First byte of every handshake message, of the QR payload and of an
/// exported mailbox.
pub const VERSION: u8 = 1;
/// Largest plaintext one frame carries.
pub const MAX_PLAINTEXT: usize = 16 * 1024;
/// Poly1305 tag.
pub const TAG_LEN: usize = 16;
/// Live frames are padded to a multiple of this (length inside the ciphertext).
pub const LIVE_PAD: usize = 32;
/// A live session rekeys each direction after this many frames.
pub const LIVE_REKEY_EVERY: u64 = 1 << 20;
/// Largest wire frame either kind of session makes: 2-byte length, padded
/// plaintext with its 2-byte inner length, tag.
pub const MAX_WIRE: usize = 2 + session::MAX_PADDED + TAG_LEN;
/// A relay origin is at most this many bytes (it is length-prefixed by one
/// byte in the prologue).
pub const MAX_ORIGIN: usize = 255;

/// Everything that can go wrong, flat (UniFFI-ready).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Malformed input (lengths, version, kind, encoding).
    Format(&'static str),
    /// Authentication failed: wrong key, wrong PSK, wrong prologue (relay),
    /// a tampered, replayed, reordered or missing frame; or the session was
    /// already ended by an earlier failure.
    Decrypt,
    /// The KEM rejected its input (bad encapsulation key length or encoding).
    Kem,
    /// Anything else snow reported.
    Noise(String),
    /// The OS random source failed.
    Rng,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Format(m) => write!(f, "format: {m}"),
            Error::Decrypt => f.write_str("decrypt failed"),
            Error::Kem => f.write_str("kem input rejected"),
            Error::Noise(m) => write!(f, "noise: {m}"),
            Error::Rng => f.write_str("rng failed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<snow::Error> for Error {
    fn from(e: snow::Error) -> Self {
        match e {
            snow::Error::Decrypt => Error::Decrypt,
            other => Error::Noise(format!("{other:?}")),
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

pub(crate) fn random<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|_| Error::Rng)?;
    Ok(b)
}

pub(crate) fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub(crate) fn to_hex(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(HEX[(x >> 4) as usize] as char);
        s.push(HEX[(x & 15) as usize] as char);
    }
    s
}

/// An X25519 static key pair (content identity of a host or a device).
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct StaticKey {
    private: [u8; 32],
    #[zeroize(skip)]
    public: [u8; 32],
}

impl StaticKey {
    pub fn generate() -> Result<Self> {
        Ok(Self::from_private(random::<32>()?))
    }

    pub fn from_private(private: [u8; 32]) -> Self {
        let public = curve25519_dalek::MontgomeryPoint::mul_base_clamped(private).to_bytes();
        Self { private, public }
    }

    pub fn public(&self) -> &[u8; 32] {
        &self.public
    }

    pub fn private(&self) -> &[u8; 32] {
        &self.private
    }
}

/// The relay a host is pinned to: its origin (`https://relay.example.com`)
/// and its identity key's fingerprint (SHA-256 of the SEC1 uncompressed
/// P-256 public key). In a prologue it is always the identity this
/// connection *proved*, checked against the pin first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayId {
    pub origin: String,
    pub fingerprint: [u8; 32],
}

impl RelayId {
    pub fn new(origin: impl Into<String>, fingerprint: [u8; 32]) -> Self {
        Self {
            origin: origin.into(),
            fingerprint,
        }
    }
}

/// Id of the channel between one host and one device (the reconnect and
/// mailbox prologues' id). Both sides can compute it; the relay never sees
/// it.
pub fn channel_id(host_static: &[u8; 32], device_static: &[u8; 32]) -> [u8; 16] {
    sha256(&[b"rlord-e2e/1 channel", host_static, device_static])[..16]
        .try_into()
        .unwrap()
}

/// Six-digit code both sides show after pairing; the host's user confirms
/// it before the device may act.
pub fn short_code(handshake_hash: &[u8]) -> u32 {
    let h = sha256(&[b"rlord-e2e/1 short code", handshake_hash]);
    u32::from_be_bytes(h[..4].try_into().unwrap()) % 1_000_000
}

/// A device's id: 16 hex characters (the first 8 bytes of a labelled hash
/// of its static key). It names the device in the CLI, the audit log and
/// `web.call`; it is not secret and never reaches the relay.
pub fn device_id(device_static: &[u8; 32]) -> String {
    to_hex(&sha256(&[b"rlord-e2e/1 device id", device_static])[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_are_short_and_stable() {
        let k = StaticKey::from_private([1; 32]);
        let id = device_id(k.public());
        assert_eq!(id.len(), 16);
        assert_eq!(id, device_id(k.public()));
        assert_ne!(id, device_id(StaticKey::from_private([2; 32]).public()));
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
