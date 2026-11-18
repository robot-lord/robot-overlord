//! The pairing offer the host shows as a QR code.

use base64::Engine as _;
use zeroize::Zeroize;

use crate::{random, Error, RelayId, Result, MAX_ORIGIN, VERSION};

const QR_PREFIX: &str = "RLORD1:";
const QR_FIXED: usize = 1 + 16 + 32 + 32 + 32;

/// What the host shows as a QR code: the relay pin, a one-use rendezvous id,
/// the host's static key and the one-use pairing secret.
#[derive(Clone, PartialEq, Eq)]
pub struct QrPayload {
    pub relay: RelayId,
    pub rendezvous_id: [u8; 16],
    pub host_static: [u8; 32],
    pub pairing_secret: [u8; 32],
}

impl Drop for QrPayload {
    fn drop(&mut self) {
        self.pairing_secret.zeroize();
    }
}

impl core::fmt::Debug for QrPayload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("QrPayload")
            .field("relay", &self.relay)
            .field("rendezvous_id", &crate::to_hex(&self.rendezvous_id))
            .finish_non_exhaustive()
    }
}

impl QrPayload {
    /// A fresh pairing offer: random rendezvous id and pairing secret.
    pub fn new(relay: RelayId, host_static: [u8; 32]) -> Result<Self> {
        if relay.origin.is_empty() || relay.origin.len() > MAX_ORIGIN {
            return Err(Error::Format("relay origin length"));
        }
        Ok(Self {
            relay,
            rendezvous_id: random()?,
            host_static,
            pairing_secret: random()?,
        })
    }

    /// `RLORD1:` + base64url (no padding) of
    /// version | rendezvous id (16) | host static (32) | pairing secret (32) |
    /// relay fingerprint (32) | relay origin (UTF-8, rest).
    pub fn encode(&self) -> String {
        let mut b = Vec::with_capacity(QR_FIXED + self.relay.origin.len());
        b.push(VERSION);
        b.extend_from_slice(&self.rendezvous_id);
        b.extend_from_slice(&self.host_static);
        b.extend_from_slice(&self.pairing_secret);
        b.extend_from_slice(&self.relay.fingerprint);
        b.extend_from_slice(self.relay.origin.as_bytes());
        let s = format!(
            "{QR_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&b)
        );
        b.zeroize();
        s
    }

    pub fn decode(s: &str) -> Result<Self> {
        let body = s
            .trim()
            .strip_prefix(QR_PREFIX)
            .ok_or(Error::Format("qr prefix"))?;
        let mut b = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| Error::Format("qr base64"))?;
        let r = Self::from_bytes(&b);
        b.zeroize();
        r
    }

    fn from_bytes(b: &[u8]) -> Result<Self> {
        if b.len() <= QR_FIXED || b.len() > QR_FIXED + MAX_ORIGIN {
            return Err(Error::Format("qr length"));
        }
        if b[0] != VERSION {
            return Err(Error::Format("qr version"));
        }
        let origin = core::str::from_utf8(&b[QR_FIXED..])
            .map_err(|_| Error::Format("qr origin"))?
            .to_owned();
        Ok(Self {
            rendezvous_id: b[1..17].try_into().unwrap(),
            host_static: b[17..49].try_into().unwrap(),
            pairing_secret: b[49..81].try_into().unwrap(),
            relay: RelayId {
                origin,
                fingerprint: b[81..113].try_into().unwrap(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> RelayId {
        RelayId::new("https://relay.example.com", [3; 32])
    }

    #[test]
    fn round_trip() {
        let qr = QrPayload::new(relay(), [4; 32]).unwrap();
        let s = qr.encode();
        assert!(s.starts_with("RLORD1:"));
        assert!(s.len() < 200, "{} chars", s.len());
        assert_eq!(QrPayload::decode(&s).unwrap(), qr);
        assert_eq!(QrPayload::decode(&format!("  {s}\n")).unwrap(), qr);
    }

    #[test]
    fn rejects() {
        assert!(QrPayload::decode("RLORD1:AAAA").is_err());
        assert!(QrPayload::decode("nope").is_err());
        assert!(QrPayload::decode("RLORD1:!!!").is_err());
        let qr = QrPayload::new(relay(), [4; 32]).unwrap();
        let mut b = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(qr.encode().strip_prefix("RLORD1:").unwrap())
            .unwrap();
        b[0] = 2;
        let s = format!(
            "RLORD1:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&b)
        );
        assert_eq!(QrPayload::decode(&s), Err(Error::Format("qr version")));
        // No origin at all.
        b[0] = 1;
        b.truncate(QR_FIXED);
        let s = format!(
            "RLORD1:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&b)
        );
        assert_eq!(QrPayload::decode(&s), Err(Error::Format("qr length")));
        assert!(QrPayload::new(RelayId::new("", [0; 32]), [0; 32]).is_err());
        assert!(QrPayload::new(RelayId::new("x".repeat(256), [0; 32]), [0; 32]).is_err());
    }
}
