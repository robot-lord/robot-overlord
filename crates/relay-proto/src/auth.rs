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

/// Something that signs as a relay identity: a software [`Identity`], or a
/// platform key that never leaves its keystore (the phone's Secure Enclave
/// or Android Keystore). ECDSA P-256 over SHA-256 of the message.
pub trait TranscriptSigner {
    /// SEC1 uncompressed public key.
    fn public_key(&self) -> Vec<u8>;
    /// A signature over `msg`: 64 bytes `r || s`, or ASN.1 DER (what a
    /// platform keystore returns). High S is fine: the verifier takes it.
    fn sign_message(&self, msg: &[u8]) -> std::result::Result<Vec<u8>, String>;
}

impl TranscriptSigner for Identity {
    fn public_key(&self) -> Vec<u8> {
        self.public().to_vec()
    }

    fn sign_message(&self, msg: &[u8]) -> std::result::Result<Vec<u8>, String> {
        Ok(self.sign(msg).to_vec())
    }
}

/// Signs `msg` with `signer` and returns the wire form (64 bytes `r || s`),
/// converting DER and checking that the signature verifies under the
/// signer's own public key (a keystore that signed with another key, or
/// garbage, is an error here rather than a refusal at the relay).
pub fn sign_with(signer: &dyn TranscriptSigner, msg: &[u8]) -> Result<[u8; SIG_LEN]> {
    let raw = signer.sign_message(msg).map_err(|_| Error::Key)?;
    let sig = raw_signature(&raw)?;
    if !verify(&signer.public_key(), msg, &sig) {
        return Err(Error::Key);
    }
    Ok(sig)
}

/// A P-256 signature as 64 bytes `r || s`: taken as is when it is 64
/// bytes, else parsed as ASN.1 DER (`SEQUENCE { INTEGER r, INTEGER s }`).
/// No low-S normalising.
pub fn raw_signature(sig: &[u8]) -> Result<[u8; SIG_LEN]> {
    if sig.len() == SIG_LEN {
        return Ok(sig.try_into().expect("64 bytes"));
    }
    der_signature(sig).ok_or(Error::Key)
}

fn der_signature(der: &[u8]) -> Option<[u8; SIG_LEN]> {
    // SEQUENCE, short-form length covering the rest exactly.
    let (&tag, rest) = der.split_first()?;
    let (&len, body) = rest.split_first()?;
    if tag != 0x30 || len & 0x80 != 0 || len as usize != body.len() {
        return None;
    }
    let (r, body) = der_integer(body)?;
    let (s, body) = der_integer(body)?;
    if !body.is_empty() {
        return None;
    }
    let mut out = [0u8; SIG_LEN];
    out[32 - r.len()..32].copy_from_slice(r);
    out[64 - s.len()..].copy_from_slice(s);
    Some(out)
}

/// One positive DER INTEGER of at most 32 bytes (its leading zero, if
/// any, dropped); the rest of the input.
fn der_integer(b: &[u8]) -> Option<(&[u8], &[u8])> {
    let (&tag, rest) = b.split_first()?;
    let (&len, rest) = rest.split_first()?;
    let len = len as usize;
    if tag != 0x02 || len == 0 || len > 33 || rest.len() < len {
        return None;
    }
    let (mut v, rest) = rest.split_at(len);
    if v[0] & 0x80 != 0 {
        return None; // negative
    }
    if v.len() > 1 && v[0] == 0 {
        if v[1] & 0x80 == 0 {
            return None; // not minimal
        }
        v = &v[1..];
    }
    if v.len() > 32 {
        return None;
    }
    Some((v, rest))
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

    /// DER of `r || s` the way a keystore writes it.
    fn to_der(sig: &[u8; 64]) -> Vec<u8> {
        let int = |v: &[u8]| {
            let mut v: Vec<u8> = v.iter().copied().skip_while(|&b| b == 0).collect();
            if v.is_empty() || v[0] & 0x80 != 0 {
                v.insert(0, 0);
            }
            let mut out = vec![0x02, v.len() as u8];
            out.extend(v);
            out
        };
        let mut body = int(&sig[..32]);
        body.extend(int(&sig[32..]));
        let mut out = vec![0x30, body.len() as u8];
        out.extend(body);
        out
    }

    struct Der(Identity);
    impl TranscriptSigner for Der {
        fn public_key(&self) -> Vec<u8> {
            self.0.public().to_vec()
        }
        fn sign_message(&self, msg: &[u8]) -> std::result::Result<Vec<u8>, String> {
            Ok(to_der(&self.0.sign(msg)))
        }
    }

    struct Wrong(Identity, Identity);
    impl TranscriptSigner for Wrong {
        fn public_key(&self) -> Vec<u8> {
            self.0.public().to_vec()
        }
        fn sign_message(&self, msg: &[u8]) -> std::result::Result<Vec<u8>, String> {
            Ok(self.1.sign(msg).to_vec())
        }
    }

    #[test]
    fn a_signer_may_answer_in_der() {
        let a = id(1);
        let raw = sign_with(&a, b"t").unwrap();
        assert!(verify(&a.public(), b"t", &raw));
        let d = Der(id(1));
        let s = sign_with(&d, b"t").unwrap();
        assert!(verify(&a.public(), b"t", &s));
        // Small r and s (leading zeros) round-trip.
        let mut small = [0u8; 64];
        small[31] = 5;
        small[63] = 0x80;
        assert_eq!(raw_signature(&to_der(&small)).unwrap(), small);
        // A signature by another key, garbage, and non-minimal DER fail.
        assert!(sign_with(&Wrong(id(1), id(2)), b"t").is_err());
        assert!(raw_signature(&[1, 2, 3]).is_err());
        assert!(raw_signature(&[0x30, 6, 2, 2, 0, 1, 2, 1, 1]).is_err());
        assert!(raw_signature(&[0x30, 6, 2, 1, 0x80, 2, 1, 1]).is_err());
    }

    #[test]
    fn a_high_s_signature_verifies_unchanged() {
        // n - s for a valid signature is the other valid one (high S when
        // s was low); the verifier takes both, and nothing normalises.
        const N: [u8; 32] = [
            0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2,
            0xfc, 0x63, 0x25, 0x51,
        ];
        let a = id(7);
        let sig = a.sign(b"m");
        let mut flipped = sig;
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let v = N[i] as i16 - sig[32 + i] as i16 - borrow;
            borrow = (v < 0) as i16;
            flipped[32 + i] = v.rem_euclid(256) as u8;
        }
        assert_ne!(flipped, sig);
        assert!(verify(&a.public(), b"m", &flipped));
        assert_eq!(raw_signature(&to_der(&flipped)).unwrap(), flipped);
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
