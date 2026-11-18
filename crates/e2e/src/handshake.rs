//! The three handshakes, with the ML-KEM-768 step mixed in through the PSK
//! slot. The device always initiates.
//!
//! | Kind | Pattern | Id in the prologue | Becomes |
//! | --- | --- | --- | --- |
//! | 1 pairing | `IKpsk2` | rendezvous id | a live [`Session`] |
//! | 2 live reconnect | `KKpsk2` | `channel_id` | a live [`Session`] |
//! | 3 mailbox keying | `KKpsk2` | `channel_id` | a [`MailboxSession`] |
//!
//! Wire format (every message starts with the protocol version and kind):
//!
//! - message 1, device to host: `VERSION | kind | noise msg 1`, whose
//!   encrypted payload is the device's fresh ML-KEM encapsulation key.
//! - message 2, host to device: `VERSION | kind | KEM ciphertext | noise msg 2`
//!   (empty payload). The ciphertext is in clear; the PSK for the `psk` token
//!   at the end of message 2 is a hash of the KEM shared secret and the
//!   ciphertext (and, for pairing, the QR's pairing secret).
//!
//! The host treats a handshake as a session only once the device's first
//! transport frame decrypts: a replayed message 1 gets a message 2 and
//! nothing else.

use snow::{params::NoiseParams, Builder, HandshakeState};
use zeroize::Zeroizing;

use crate::kem::{self, KEM_CT_LEN};
use crate::mailbox::MailboxSession;
use crate::session::Session;
use crate::{
    channel_id, sha256, short_code, Error, QrPayload, RelayId, Result, StaticKey, LIVE_REKEY_EVERY,
    MAX_ORIGIN, VERSION,
};

/// Which handshake: the second byte of both messages, and in the prologue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Pair = 1,
    Reconnect = 2,
    Mailbox = 3,
}

impl Kind {
    pub fn from_byte(b: u8) -> Result<Kind> {
        match b {
            1 => Ok(Kind::Pair),
            2 => Ok(Kind::Reconnect),
            3 => Ok(Kind::Mailbox),
            _ => Err(Error::Format("handshake kind")),
        }
    }

    /// The kind of a handshake message, read from its header.
    pub fn of_message(msg: &[u8]) -> Result<Kind> {
        match msg {
            [v, k, ..] if *v == VERSION => Kind::from_byte(*k),
            [_, _, ..] => Err(Error::Format("handshake version")),
            _ => Err(Error::Format("handshake too short")),
        }
    }
}

/// `psk2`: the PSK is mixed at the end of message 2.
const PSK_SLOT: usize = 2;
const MAX_HS_MSG: usize = 4096;

fn params(kind: Kind) -> NoiseParams {
    match kind {
        Kind::Pair => "Noise_IKpsk2_25519_ChaChaPoly_SHA256".parse().unwrap(),
        Kind::Reconnect | Kind::Mailbox => "Noise_KKpsk2_25519_ChaChaPoly_SHA256".parse().unwrap(),
    }
}

/// Binds the protocol version, the direction of roles, the kind of
/// handshake, the relay the bytes travel through, and the pairing or
/// channel id. Anything replayed into another context fails.
fn prologue(kind: Kind, relay: &RelayId, id: &[u8; 16]) -> Result<Vec<u8>> {
    let origin = relay.origin.as_bytes();
    if origin.is_empty() || origin.len() > MAX_ORIGIN {
        return Err(Error::Format("relay origin length"));
    }
    let mut p = Vec::with_capacity(32 + origin.len() + 32 + 16);
    p.extend_from_slice(b"rlord-e2e/1 device->host");
    p.push(VERSION);
    p.push(kind as u8);
    p.push(origin.len() as u8);
    p.extend_from_slice(origin);
    p.extend_from_slice(&relay.fingerprint);
    p.extend_from_slice(id);
    Ok(p)
}

fn pair_psk(secret: &[u8; 32], ss: &[u8; 32], ct: &[u8]) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(sha256(&[b"rlord-e2e/1 pair psk", secret, ss, ct]))
}

fn kk_psk(ss: &[u8; 32], ct: &[u8]) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(sha256(&[b"rlord-e2e/1 kk psk", ss, ct]))
}

/// Fixed randomness for test vectors. Production passes `None`: snow draws
/// the ephemeral key and the core draws the KEM seeds from the OS.
#[derive(Clone)]
pub struct FixedRandom {
    pub ephemeral: [u8; 32],
    /// Initiator: ML-KEM keygen seed (d || z). Responder: the first 32 bytes
    /// are the encapsulation randomness m.
    pub kem: [u8; 64],
}

fn build(
    kind: Kind,
    local: &StaticKey,
    remote: Option<&[u8; 32]>,
    prologue: &[u8],
    fixed: Option<&FixedRandom>,
    initiator: bool,
) -> Result<HandshakeState> {
    let p = params(kind);
    let mut b = Builder::new(p)
        .local_private_key(local.private())?
        .prologue(prologue)?;
    if let Some(r) = remote {
        b = b.remote_public_key(r)?;
    }
    if let Some(f) = fixed {
        b = b.fixed_ephemeral_key_for_testing_only(&f.ephemeral);
    }
    Ok(if initiator {
        b.build_initiator()?
    } else {
        b.build_responder()?
    })
}

fn body(msg: &[u8], kind: Kind) -> Result<&[u8]> {
    if Kind::of_message(msg)? != kind {
        return Err(Error::Format("handshake kind"));
    }
    Ok(&msg[2..])
}

/// A finished handshake, not yet a session.
pub struct Established {
    hs: HandshakeState,
    kind: Kind,
    pub remote_static: [u8; 32],
    pub handshake_hash: [u8; 32],
}

impl Established {
    fn new(hs: HandshakeState, kind: Kind) -> Result<Self> {
        if !hs.is_handshake_finished() {
            return Err(Error::Format("handshake not finished"));
        }
        let remote_static: [u8; 32] = hs
            .get_remote_static()
            .ok_or(Error::Format("no remote static"))?
            .try_into()
            .map_err(|_| Error::Format("remote static length"))?;
        let handshake_hash: [u8; 32] = hs
            .get_handshake_hash()
            .try_into()
            .map_err(|_| Error::Format("handshake hash length"))?;
        Ok(Self {
            hs,
            kind,
            remote_static,
            handshake_hash,
        })
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn short_code(&self) -> u32 {
        short_code(&self.handshake_hash)
    }

    /// A live session (pairing or reconnect).
    pub fn into_session(self) -> Result<Session> {
        self.into_session_rekey_every(LIVE_REKEY_EVERY)
    }

    /// A live session that rekeys every `n` frames (tests and vectors).
    pub fn into_session_rekey_every(self, n: u64) -> Result<Session> {
        if self.kind == Kind::Mailbox {
            return Err(Error::Format("a mailbox handshake keys a mailbox"));
        }
        Ok(Session::new(self.hs.into_transport_mode()?, n))
    }

    /// The saveable mailbox session (kind 3 only), over the handshake's two
    /// raw keys.
    pub fn into_mailbox(mut self) -> Result<MailboxSession> {
        if self.kind != Kind::Mailbox {
            return Err(Error::Format("only a mailbox handshake keys a mailbox"));
        }
        let initiator = self.hs.is_initiator();
        let id: [u8; 8] = sha256(&[b"rlord-e2e/1 mailbox id", &self.handshake_hash])[..8]
            .try_into()
            .unwrap();
        let (i2r, r2i) = self.hs.dangerously_get_raw_split();
        let (i2r, r2i) = (Zeroizing::new(i2r), Zeroizing::new(r2i));
        Ok(if initiator {
            MailboxSession::from_keys(id, *i2r, *r2i)
        } else {
            MailboxSession::from_keys(id, *r2i, *i2r)
        })
    }

    /// snow's own transport over the same handshake, for the vectors that
    /// hold the mailbox cipher to it.
    #[cfg(feature = "vectors")]
    pub(crate) fn into_snow_transport(self) -> Result<snow::TransportState> {
        Ok(self.hs.into_transport_mode()?)
    }
}

/// The device's side.
pub struct Initiator {
    hs: HandshakeState,
    kind: Kind,
    dk: kem::DecapKey,
    pairing_secret: Option<Zeroizing<[u8; 32]>>,
}

impl Initiator {
    /// Pairing from a scanned QR. `relay` is the relay this connection
    /// actually reached (its proven identity); the caller has already
    /// checked it against the QR's pin.
    pub fn pair(
        qr: &QrPayload,
        device: &StaticKey,
        relay: &RelayId,
        fixed: Option<&FixedRandom>,
    ) -> Result<(Self, Vec<u8>)> {
        let p = prologue(Kind::Pair, relay, &qr.rendezvous_id)?;
        let hs = build(Kind::Pair, device, Some(&qr.host_static), &p, fixed, true)?;
        Self::start(
            hs,
            Kind::Pair,
            Some(Zeroizing::new(qr.pairing_secret)),
            fixed,
        )
    }

    /// A live session with a paired host.
    pub fn reconnect(
        device: &StaticKey,
        host_static: &[u8; 32],
        relay: &RelayId,
        fixed: Option<&FixedRandom>,
    ) -> Result<(Self, Vec<u8>)> {
        Self::known(Kind::Reconnect, device, host_static, relay, fixed)
    }

    /// Keys a new mailbox with a paired host (run over a live link).
    pub fn mailbox(
        device: &StaticKey,
        host_static: &[u8; 32],
        relay: &RelayId,
        fixed: Option<&FixedRandom>,
    ) -> Result<(Self, Vec<u8>)> {
        Self::known(Kind::Mailbox, device, host_static, relay, fixed)
    }

    fn known(
        kind: Kind,
        device: &StaticKey,
        host_static: &[u8; 32],
        relay: &RelayId,
        fixed: Option<&FixedRandom>,
    ) -> Result<(Self, Vec<u8>)> {
        let p = prologue(kind, relay, &channel_id(host_static, device.public()))?;
        let hs = build(kind, device, Some(host_static), &p, fixed, true)?;
        Self::start(hs, kind, None, fixed)
    }

    fn start(
        mut hs: HandshakeState,
        kind: Kind,
        pairing_secret: Option<Zeroizing<[u8; 32]>>,
        fixed: Option<&FixedRandom>,
    ) -> Result<(Self, Vec<u8>)> {
        let (dk, payload) = kem::start_initiator(fixed.map(|f| f.kem))?;
        let mut msg = vec![0u8; MAX_HS_MSG];
        msg[0] = VERSION;
        msg[1] = kind as u8;
        let n = hs.write_message(&payload, &mut msg[2..])?;
        msg.truncate(2 + n);
        Ok((
            Self {
                hs,
                kind,
                dk,
                pairing_secret,
            },
            msg,
        ))
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn finish(mut self, msg2: &[u8]) -> Result<Established> {
        let rest = body(msg2, self.kind)?;
        if rest.len() < KEM_CT_LEN {
            return Err(Error::Format("handshake 2 too short"));
        }
        let (ct, noise) = rest.split_at(KEM_CT_LEN);
        let ss = kem::finish_initiator(&self.dk, ct)?;
        let psk = match &self.pairing_secret {
            Some(secret) => pair_psk(secret, &ss, ct),
            None => kk_psk(&ss, ct),
        };
        self.hs.set_psk(PSK_SLOT, &psk[..])?;
        let mut payload = [0u8; MAX_HS_MSG];
        let n = self.hs.read_message(noise, &mut payload)?;
        if n != 0 {
            return Err(Error::Format("handshake 2 payload must be empty"));
        }
        Established::new(self.hs, self.kind)
    }
}

/// The host's side.
pub struct Responder;

impl Responder {
    /// Answer a pairing message 1 for the offer `qr`. The caller registers
    /// the device only after its first transport frame decrypts.
    pub fn pair(
        host: &StaticKey,
        qr: &QrPayload,
        relay: &RelayId,
        msg1: &[u8],
        fixed: Option<&FixedRandom>,
    ) -> Result<(Established, Vec<u8>)> {
        let p = prologue(Kind::Pair, relay, &qr.rendezvous_id)?;
        let hs = build(Kind::Pair, host, None, &p, fixed, false)?;
        Self::answer(hs, Kind::Pair, Some(&qr.pairing_secret), msg1, fixed)
    }

    /// Answer a live reconnect from a known device. A revoked device's key
    /// is no longer passed here, so its handshake cannot complete.
    pub fn reconnect(
        host: &StaticKey,
        device_static: &[u8; 32],
        relay: &RelayId,
        msg1: &[u8],
        fixed: Option<&FixedRandom>,
    ) -> Result<(Established, Vec<u8>)> {
        Self::known(Kind::Reconnect, host, device_static, relay, msg1, fixed)
    }

    /// Answer a mailbox keying from a known device.
    pub fn mailbox(
        host: &StaticKey,
        device_static: &[u8; 32],
        relay: &RelayId,
        msg1: &[u8],
        fixed: Option<&FixedRandom>,
    ) -> Result<(Established, Vec<u8>)> {
        Self::known(Kind::Mailbox, host, device_static, relay, msg1, fixed)
    }

    fn known(
        kind: Kind,
        host: &StaticKey,
        device_static: &[u8; 32],
        relay: &RelayId,
        msg1: &[u8],
        fixed: Option<&FixedRandom>,
    ) -> Result<(Established, Vec<u8>)> {
        let p = prologue(kind, relay, &channel_id(host.public(), device_static))?;
        let hs = build(kind, host, Some(device_static), &p, fixed, false)?;
        Self::answer(hs, kind, None, msg1, fixed)
    }

    fn answer(
        mut hs: HandshakeState,
        kind: Kind,
        pairing_secret: Option<&[u8; 32]>,
        msg1: &[u8],
        fixed: Option<&FixedRandom>,
    ) -> Result<(Established, Vec<u8>)> {
        let noise = body(msg1, kind)?;
        let mut payload = vec![0u8; MAX_HS_MSG];
        let n = hs.read_message(noise, &mut payload)?;
        let m = match fixed {
            Some(f) => f.kem[..32].try_into().unwrap(),
            None => crate::random::<32>()?,
        };
        let (ct, ss) = kem::respond(&payload[..n], m)?;
        let psk = match pairing_secret {
            Some(secret) => pair_psk(secret, &ss, &ct),
            None => kk_psk(&ss, &ct),
        };
        hs.set_psk(PSK_SLOT, &psk[..])?;
        let mut msg = vec![0u8; MAX_HS_MSG];
        msg[0] = VERSION;
        msg[1] = kind as u8;
        msg[2..2 + ct.len()].copy_from_slice(&ct);
        let off = 2 + ct.len();
        let w = hs.write_message(&[], &mut msg[off..])?;
        msg.truncate(off + w);
        Ok((Established::new(hs, kind)?, msg))
    }
}
