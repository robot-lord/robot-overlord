//! The relay link (`docs/working/e2e-protocol.md`, "The relay link"): what
//! a host (`rlord-web`), a device and `rlord-relay` say to each other, and
//! nothing about content. One websocket per peer, `wss://<host>/v1`, opened
//! by the peer; JSON control messages (`msg`), binary frames (`frame`), the
//! signed connect sequence (`auth`) and the relay address with its pin
//! (`address`).
//!
//! Sans-IO: no sockets, no clock, no randomness of its own (callers pass
//! seeds and nonces), so it builds natively and to WASM.
#![forbid(unsafe_code)]

pub mod address;
pub mod auth;
pub mod frame;
pub mod msg;

pub use address::RelayAddress;
pub use auth::Identity;
pub use msg::{FromRelay, Role, ToRelay, B64};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A relay address or origin that doesn't parse.
    Address(&'static str),
    /// A key that isn't a valid P-256 key.
    Key,
    /// A binary frame that doesn't parse.
    Frame(&'static str),
    /// A control message that doesn't parse.
    Message(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Address(m) => write!(f, "relay address: {m}"),
            Error::Key => f.write_str("not a P-256 key"),
            Error::Frame(m) => write!(f, "frame: {m}"),
            Error::Message(m) => write!(f, "message: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Shortened fingerprint for logs: the first 8 hex characters.
pub fn short(fp: &[u8]) -> String {
    fp.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// What a peer checks in `challenge`, in this order and before it sends
/// anything else: the origin is byte for byte its pin, the key's
/// fingerprint is its pin, and the signature verifies over this
/// connection's hello. Any miss is "wrong relay": close, never retry
/// against anything else.
pub fn check_challenge(
    pin: &RelayAddress,
    peer_key: &[u8],
    peer_nonce: &[u8],
    origin: &str,
    relay_key: &[u8],
    relay_nonce: &[u8],
    sig: &[u8],
) -> std::result::Result<(), WrongRelay> {
    if origin != pin.origin {
        return Err(WrongRelay::Origin);
    }
    if auth::fingerprint(relay_key) != pin.fingerprint {
        return Err(WrongRelay::Key);
    }
    let t = auth::relay_transcript(origin, relay_key, peer_key, peer_nonce, relay_nonce);
    if relay_nonce.len() != auth::NONCE_LEN || !auth::verify(relay_key, &t, sig) {
        return Err(WrongRelay::Signature);
    }
    Ok(())
}

/// Why a relay was not the pinned one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrongRelay {
    Origin,
    Key,
    Signature,
}

impl std::fmt::Display for WrongRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            WrongRelay::Origin => "wrong relay: it names another origin than the pinned one",
            WrongRelay::Key => "wrong relay: its key is not the pinned key",
            WrongRelay::Signature => "wrong relay: it could not prove it holds the pinned key",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_check_the_pin() {
        let relay = Identity::from_seed([1; 32]).unwrap();
        let other = Identity::from_seed([2; 32]).unwrap();
        let peer = Identity::from_seed([3; 32]).unwrap();
        let pin = RelayAddress {
            origin: "https://r.example".into(),
            fingerprint: relay.fingerprint(),
        };
        let sign = |who: &Identity, origin: &str| {
            who.sign(&auth::relay_transcript(
                origin,
                &who.public(),
                &peer.public(),
                &[5; 32],
                &[6; 32],
            ))
        };
        let ok = sign(&relay, &pin.origin);
        let check = |origin: &str, key: &[u8], sig: &[u8]| {
            check_challenge(&pin, &peer.public(), &[5; 32], origin, key, &[6; 32], sig)
        };
        assert_eq!(check(&pin.origin, &relay.public(), &ok), Ok(()));
        assert_eq!(
            check(
                "https://s.example",
                &relay.public(),
                &sign(&relay, "https://s.example")
            ),
            Err(WrongRelay::Origin)
        );
        assert_eq!(
            check(&pin.origin, &other.public(), &sign(&other, &pin.origin)),
            Err(WrongRelay::Key)
        );
        // The pinned key claimed without its private key.
        assert_eq!(
            check(&pin.origin, &relay.public(), &sign(&other, &pin.origin)),
            Err(WrongRelay::Signature)
        );
        // A signature for another hello (another peer nonce).
        let stale = relay.sign(&auth::relay_transcript(
            &pin.origin,
            &relay.public(),
            &peer.public(),
            &[9; 32],
            &[6; 32],
        ));
        assert_eq!(
            check(&pin.origin, &relay.public(), &stale),
            Err(WrongRelay::Signature)
        );
        assert_eq!(short(&[0xab, 0xcd, 0xef, 0x01, 0x23]), "abcdef01");
    }
}
