//! Relay identities (P-256 ECDSA) and the two signed transcripts of the
//! connect sequence.
//!
//! Every field of a transcript is length-prefixed (`u16` big-endian), so no
//! two different field lists give the same bytes. Signatures are RFC 6979
//! deterministic, fixed 64 bytes `r || s`, over SHA-256.

use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::{Error, Result, Role};

/// SEC1 uncompressed P-256 public key length.
pub const PUBLIC_LEN: usize = 65;
/// Signature length (`r || s`).
pub const SIG_LEN: usize = 64;
/// Nonce length, both sides.
pub const NONCE_LEN: usize = 32;

/// SHA-256 of a SEC1 uncompressed public key: what a pin and an allow-list
/// hold.
pub fn fingerprint(public: &[u8]) -> [u8; 32] {
    Sha256::digest(public).into()
}

/// A party's relay identity key (host, device or relay). Not a content key.
#[derive(Clone)]
pub struct Identity {
    sk: SigningKey,
}

impl Identity {
    /// A key from 32 random bytes the caller drew (sans-IO). `None` for the
    /// vanishingly rare seed that is not a valid scalar: draw again.
    pub fn from_seed(seed: [u8; 32]) -> Option<Identity> {
        SigningKey::from_slice(&seed).ok().map(|sk| Identity { sk })
    }

    /// Loads [`Identity::to_bytes`].
    pub fn from_bytes(b: &[u8]) -> Result<Identity> {
        if b.len() != 32 {
            return Err(Error::Key);
        }
        SigningKey::from_slice(b)
            .map(|sk| Identity { sk })
            .map_err(|_| Error::Key)
    }

    /// The private scalar (32 bytes): store it as a private key, 0600.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.sk.to_bytes().into()
    }

    /// SEC1 uncompressed public key.
    pub fn public(&self) -> [u8; PUBLIC_LEN] {
        let b = self.sk.verifying_key().to_sec1_bytes();
        b.as_ref().try_into().expect("uncompressed P-256 point")
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        fingerprint(&self.public())
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; SIG_LEN] {
        let s: Signature = self.sk.sign(msg);
        s.to_bytes().into()
    }
}

/// Checks that `public` is a valid SEC1 uncompressed P-256 point.
pub fn check_public(public: &[u8]) -> Result<()> {
    if public.len() != PUBLIC_LEN || public[0] != 4 {
        return Err(Error::Key);
    }
    VerifyingKey::from_sec1_bytes(public)
        .map(|_| ())
        .map_err(|_| Error::Key)
}

/// Does `sig` verify for `msg` under the SEC1 `public` key?
pub fn verify(public: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    if check_public(public).is_err() {
        return false;
    }
    let (Ok(vk), Ok(sig)) = (
        VerifyingKey::from_sec1_bytes(public),
        Signature::from_slice(sig),
    ) else {
        return false;
    };
    vk.verify(msg, &sig).is_ok()
}

fn transcript(label: &[u8], fields: &[&[u8]]) -> Vec<u8> {
    let mut t = Vec::with_capacity(label.len() + fields.iter().map(|f| 2 + f.len()).sum::<usize>());
    t.extend_from_slice(label);
    for f in fields {
        t.extend_from_slice(&(f.len() as u16).to_be_bytes());
        t.extend_from_slice(f);
    }
    t
}

/// What the relay signs in `challenge`: its origin, its key, the peer's key
/// and both nonces. A peer that checks it knows this relay holds the pinned
/// key and answered *this* hello.
pub fn relay_transcript(
    origin: &str,
    relay_key: &[u8],
    peer_key: &[u8],
    peer_nonce: &[u8],
    relay_nonce: &[u8],
) -> Vec<u8> {
    transcript(
        b"rlord-relay/1 relay",
        &[
            origin.as_bytes(),
            relay_key,
            peer_key,
            peer_nonce,
            relay_nonce,
        ],
    )
}

/// What the peer signs in `auth`: its role, the relay it proved, its own
/// key, both nonces, and the host fingerprint (a device) or rendezvous id
/// (a pairing device); empty for a host.
pub fn peer_transcript(
    role: Role,
    origin: &str,
    relay_key: &[u8],
    peer_key: &[u8],
    relay_nonce: &[u8],
    peer_nonce: &[u8],
    target: &[u8],
) -> Vec<u8> {
    transcript(
        b"rlord-relay/1 peer",
        &[
            role.as_str().as_bytes(),
            origin.as_bytes(),
            relay_key,
            peer_key,
            relay_nonce,
            peer_nonce,
            target,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(b: u8) -> Identity {
        Identity::from_seed([b; 32]).unwrap()
    }

    #[test]
    fn keys_round_trip_and_sign() {
        let a = id(1);
        assert_eq!(a.public().len(), PUBLIC_LEN);
        let b = Identity::from_bytes(&a.to_bytes()).unwrap();
        assert_eq!(a.public(), b.public());
        assert_eq!(a.fingerprint(), fingerprint(&a.public()));
        let sig = a.sign(b"hi");
        assert!(verify(&a.public(), b"hi", &sig));
        assert!(!verify(&a.public(), b"ho", &sig));
        assert!(!verify(&id(2).public(), b"hi", &sig));
        assert!(!verify(&a.public()[..64], b"hi", &sig));
        assert!(!verify(&[4; 65], b"hi", &sig));
        assert!(Identity::from_seed([0; 32]).is_none());
        assert!(Identity::from_bytes(&[1; 31]).is_err());
    }

    #[test]
    fn transcripts_bind_every_field() {
        let relay = id(1);
        let peer = id(2);
        let base = relay_transcript(
            "https://r.example",
            &relay.public(),
            &peer.public(),
            &[1; 32],
            &[2; 32],
        );
        let sig = relay.sign(&base);
        assert!(verify(&relay.public(), &base, &sig));
        for other in [
            relay_transcript(
                "https://s.example",
                &relay.public(),
                &peer.public(),
                &[1; 32],
                &[2; 32],
            ),
            relay_transcript(
                "https://r.example",
                &id(3).public(),
                &peer.public(),
                &[1; 32],
                &[2; 32],
            ),
            relay_transcript(
                "https://r.example",
                &relay.public(),
                &id(3).public(),
                &[1; 32],
                &[2; 32],
            ),
            relay_transcript(
                "https://r.example",
                &relay.public(),
                &peer.public(),
                &[9; 32],
                &[2; 32],
            ),
            relay_transcript(
                "https://r.example",
                &relay.public(),
                &peer.public(),
                &[1; 32],
                &[9; 32],
            ),
        ] {
            assert!(!verify(&relay.public(), &other, &sig));
        }
        let t = |role, target: &[u8]| {
            peer_transcript(
                role,
                "https://r.example",
                &relay.public(),
                &peer.public(),
                &[2; 32],
                &[1; 32],
                target,
            )
        };
        let sig = peer.sign(&t(Role::Device, &[5; 32]));
        assert!(verify(&peer.public(), &t(Role::Device, &[5; 32]), &sig));
        assert!(!verify(&peer.public(), &t(Role::Host, &[5; 32]), &sig));
        assert!(!verify(&peer.public(), &t(Role::Device, &[6; 32]), &sig));
        assert!(!verify(&peer.public(), &t(Role::Pair, &[5; 32]), &sig));
        // Length prefixes: moving a byte between fields changes the bytes.
        assert_ne!(
            transcript(b"x", &[b"ab", b"c"]),
            transcript(b"x", &[b"a", b"bc"])
        );
    }
}
