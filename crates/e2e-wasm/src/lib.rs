//! The `wasm-bindgen` surface of `rlord_e2e`, for a device running in a
//! WebView or a browser (the phone slice), and for the WASM tests (`cargo
//! xtask e2e test-wasm`).
//!
//! The hot path is `E2eSession`: it owns one fixed buffer in WASM memory,
//! split into an input region and an output region. JS writes into a
//! `Uint8Array` view of it (re-made if memory grows), calls `encrypt(len)` /
//! `decrypt(len)` (in place, input region) or the batch calls (input region
//! to output region), and copies the result out. No allocation per call.
//! The spike measured batching as the lever (`e2e-crypto.md`); the buffer
//! costs nothing extra.
//!
//! Every type here is a thin owned wrapper over the core's, which keeps to
//! the UniFFI-ready rule (`crates/e2e/src/lib.rs`).
#![forbid(unsafe_code)]

use rlord_e2e as core;
use wasm_bindgen::prelude::*;

fn js(e: core::Error) -> JsError {
    JsError::new(&e.to_string())
}

fn key32(b: &[u8], what: &str) -> Result<[u8; 32], JsError> {
    b.try_into()
        .map_err(|_| JsError::new(&format!("{what}: expected 32 bytes")))
}

fn relay(origin: &str, fingerprint: &[u8]) -> Result<core::RelayId, JsError> {
    Ok(core::RelayId::new(
        origin,
        key32(fingerprint, "relay fingerprint")?,
    ))
}

#[wasm_bindgen]
pub fn protocol_info() -> String {
    format!(
        "rlord-e2e v{} kem={} max_plaintext={} max_wire={}",
        core::VERSION,
        core::KEM_BACKEND,
        core::MAX_PLAINTEXT,
        core::MAX_WIRE
    )
}

/// 64 bytes: private (32) then public (32).
#[wasm_bindgen]
pub fn generate_static_key() -> Result<Vec<u8>, JsError> {
    let k = core::StaticKey::generate().map_err(js)?;
    Ok([k.private().as_slice(), k.public().as_slice()].concat())
}

#[wasm_bindgen]
pub fn public_key(private: &[u8]) -> Result<Vec<u8>, JsError> {
    Ok(
        core::StaticKey::from_private(key32(private, "private key")?)
            .public()
            .to_vec(),
    )
}

#[wasm_bindgen]
pub fn device_id(device_public: &[u8]) -> Result<String, JsError> {
    Ok(core::device_id(&key32(device_public, "device key")?))
}

/// The fields of a QR the client needs before the handshake.
#[wasm_bindgen]
pub struct QrInfo {
    relay_origin: String,
    relay_fingerprint: Vec<u8>,
    rendezvous_id: Vec<u8>,
    host_static: Vec<u8>,
}

#[wasm_bindgen]
impl QrInfo {
    #[wasm_bindgen(getter)]
    pub fn relay_origin(&self) -> String {
        self.relay_origin.clone()
    }
    #[wasm_bindgen(getter)]
    pub fn relay_fingerprint(&self) -> Vec<u8> {
        self.relay_fingerprint.clone()
    }
    #[wasm_bindgen(getter)]
    pub fn rendezvous_id(&self) -> Vec<u8> {
        self.rendezvous_id.clone()
    }
    #[wasm_bindgen(getter)]
    pub fn host_static(&self) -> Vec<u8> {
        self.host_static.clone()
    }
}

#[wasm_bindgen]
pub fn parse_qr(qr: &str) -> Result<QrInfo, JsError> {
    let q = core::QrPayload::decode(qr).map_err(js)?;
    Ok(QrInfo {
        relay_origin: q.relay.origin.clone(),
        relay_fingerprint: q.relay.fingerprint.to_vec(),
        rendezvous_id: q.rendezvous_id.to_vec(),
        host_static: q.host_static.to_vec(),
    })
}

/// A finished handshake, before it becomes a session.
#[wasm_bindgen]
pub struct Established {
    inner: Option<core::Established>,
    short_code: u32,
    remote_static: Vec<u8>,
    handshake_hash: Vec<u8>,
}

impl Established {
    fn new(e: core::Established) -> Established {
        Established {
            short_code: e.short_code(),
            remote_static: e.remote_static.to_vec(),
            handshake_hash: e.handshake_hash.to_vec(),
            inner: Some(e),
        }
    }

    fn take(&mut self) -> Result<core::Established, JsError> {
        self.inner
            .take()
            .ok_or_else(|| JsError::new("already used"))
    }
}

#[wasm_bindgen]
impl Established {
    pub fn short_code(&self) -> u32 {
        self.short_code
    }
    pub fn remote_static(&self) -> Vec<u8> {
        self.remote_static.clone()
    }
    pub fn handshake_hash(&self) -> Vec<u8> {
        self.handshake_hash.clone()
    }
    /// A live session (pairing or reconnect).
    pub fn into_session(&mut self) -> Result<E2eSession, JsError> {
        Ok(E2eSession::new(self.take()?.into_session().map_err(js)?))
    }
    /// The mailbox session (a mailbox-keying handshake only).
    pub fn into_mailbox(&mut self) -> Result<E2eMailbox, JsError> {
        Ok(E2eMailbox {
            m: self.take()?.into_mailbox().map_err(js)?,
        })
    }
}

/// The device's side of a handshake.
#[wasm_bindgen]
pub struct DeviceHandshake {
    ini: Option<core::Initiator>,
    msg1: Vec<u8>,
}

#[wasm_bindgen]
impl DeviceHandshake {
    /// `relay_origin` / `relay_fingerprint`: the relay this connection
    /// proved. The caller checks them against the QR's pin first.
    pub fn pair(
        qr: &str,
        device_private: &[u8],
        relay_origin: &str,
        relay_fingerprint: &[u8],
    ) -> Result<DeviceHandshake, JsError> {
        let q = core::QrPayload::decode(qr).map_err(js)?;
        let device = core::StaticKey::from_private(key32(device_private, "device key")?);
        let (ini, msg1) =
            core::Initiator::pair(&q, &device, &relay(relay_origin, relay_fingerprint)?, None)
                .map_err(js)?;
        Ok(DeviceHandshake {
            ini: Some(ini),
            msg1,
        })
    }

    pub fn reconnect(
        device_private: &[u8],
        host_public: &[u8],
        relay_origin: &str,
        relay_fingerprint: &[u8],
    ) -> Result<DeviceHandshake, JsError> {
        let device = core::StaticKey::from_private(key32(device_private, "device key")?);
        let (ini, msg1) = core::Initiator::reconnect(
            &device,
            &key32(host_public, "host key")?,
            &relay(relay_origin, relay_fingerprint)?,
            None,
        )
        .map_err(js)?;
        Ok(DeviceHandshake {
            ini: Some(ini),
            msg1,
        })
    }

    pub fn mailbox(
        device_private: &[u8],
        host_public: &[u8],
        relay_origin: &str,
        relay_fingerprint: &[u8],
    ) -> Result<DeviceHandshake, JsError> {
        let device = core::StaticKey::from_private(key32(device_private, "device key")?);
        let (ini, msg1) = core::Initiator::mailbox(
            &device,
            &key32(host_public, "host key")?,
            &relay(relay_origin, relay_fingerprint)?,
            None,
        )
        .map_err(js)?;
        Ok(DeviceHandshake {
            ini: Some(ini),
            msg1,
        })
    }

    pub fn message(&self) -> Vec<u8> {
        self.msg1.clone()
    }

    pub fn finish(&mut self, msg2: &[u8]) -> Result<Established, JsError> {
        let ini = self
            .ini
            .take()
            .ok_or_else(|| JsError::new("already finished"))?;
        Ok(Established::new(ini.finish(msg2).map_err(js)?))
    }
}

/// The host's answer to a message 1 (the interop tests play the host).
#[wasm_bindgen]
pub struct HostAnswer {
    est: Option<core::Established>,
    msg2: Vec<u8>,
}

#[wasm_bindgen]
impl HostAnswer {
    pub fn pair(
        host_private: &[u8],
        qr: &str,
        relay_origin: &str,
        relay_fingerprint: &[u8],
        msg1: &[u8],
    ) -> Result<HostAnswer, JsError> {
        let host = core::StaticKey::from_private(key32(host_private, "host key")?);
        let q = core::QrPayload::decode(qr).map_err(js)?;
        let (est, msg2) = core::Responder::pair(
            &host,
            &q,
            &relay(relay_origin, relay_fingerprint)?,
            msg1,
            None,
        )
        .map_err(js)?;
        Ok(HostAnswer {
            est: Some(est),
            msg2,
        })
    }

    pub fn reconnect(
        host_private: &[u8],
        device_public: &[u8],
        relay_origin: &str,
        relay_fingerprint: &[u8],
        msg1: &[u8],
    ) -> Result<HostAnswer, JsError> {
        let host = core::StaticKey::from_private(key32(host_private, "host key")?);
        let (est, msg2) = core::Responder::reconnect(
            &host,
            &key32(device_public, "device key")?,
            &relay(relay_origin, relay_fingerprint)?,
            msg1,
            None,
        )
        .map_err(js)?;
        Ok(HostAnswer {
            est: Some(est),
            msg2,
        })
    }

    pub fn message(&self) -> Vec<u8> {
        self.msg2.clone()
    }

    pub fn established(&mut self) -> Result<Established, JsError> {
        Ok(Established::new(
            self.est
                .take()
                .ok_or_else(|| JsError::new("already taken"))?,
        ))
    }
}

/// Frames per batch the buffer is sized for, at the largest frame size.
const BATCH_FRAMES: usize = 64;
const REGION: usize = BATCH_FRAMES * core::MAX_WIRE;

/// A live session with its buffer.
#[wasm_bindgen]
pub struct E2eSession {
    s: core::Session,
    buf: Vec<u8>,
}

#[wasm_bindgen]
impl E2eSession {
    fn new(s: core::Session) -> Self {
        Self {
            s,
            buf: vec![0u8; 2 * REGION],
        }
    }

    /// Address of the buffer in WASM memory. Input region at +0, output
    /// region at +region_size().
    pub fn buffer_ptr(&self) -> u32 {
        self.buf.as_ptr() as usize as u32
    }

    pub fn region_size(&self) -> u32 {
        REGION as u32
    }

    /// True once a decrypt failed: close the link and re-handshake.
    pub fn is_poisoned(&self) -> bool {
        self.s.is_poisoned()
    }

    /// In place: plaintext in input[..len], wire frame out in input[..ret].
    pub fn encrypt(&mut self, len: u32) -> Result<u32, JsError> {
        self.s
            .encrypt_in_place(&mut self.buf[..REGION], len as usize)
            .map(|n| n as u32)
            .map_err(js)
    }

    /// In place: wire frame in input[..len], plaintext out in input[..ret].
    pub fn decrypt(&mut self, len: u32) -> Result<u32, JsError> {
        self.s
            .decrypt_in_place(&mut self.buf[..REGION], len as usize)
            .map(|n| n as u32)
            .map_err(js)
    }

    /// `u16 len | bytes` records in input[..len] to concatenated wire frames
    /// in output[..ret].
    pub fn encrypt_batch(&mut self, len: u32) -> Result<u32, JsError> {
        let (i, o) = self.buf.split_at_mut(REGION);
        self.s
            .encrypt_batch(&i[..len as usize], o)
            .map(|n| n as u32)
            .map_err(js)
    }

    /// Concatenated wire frames in input[..len] to `u16 len | bytes` records
    /// in output[..ret].
    pub fn decrypt_batch(&mut self, len: u32) -> Result<u32, JsError> {
        let (i, o) = self.buf.split_at_mut(REGION);
        self.s
            .decrypt_batch(&i[..len as usize], o)
            .map(|n| n as u32)
            .map_err(js)
    }
}

/// A mailbox session: one message per call, saveable.
#[wasm_bindgen]
pub struct E2eMailbox {
    m: core::MailboxSession,
}

#[wasm_bindgen]
impl E2eMailbox {
    pub fn import(bytes: &[u8]) -> Result<E2eMailbox, JsError> {
        Ok(E2eMailbox {
            m: core::MailboxSession::import(bytes).map_err(js)?,
        })
    }
    /// Save before sending what `encrypt` returned; after acting on what
    /// `decrypt` returned.
    pub fn export(&self) -> Result<Vec<u8>, JsError> {
        self.m.export().map_err(js)
    }
    pub fn id(&self) -> Vec<u8> {
        self.m.id().to_vec()
    }
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, JsError> {
        self.m.encrypt(plaintext).map_err(js)
    }
    pub fn decrypt(&mut self, frame: &[u8]) -> Result<Vec<u8>, JsError> {
        self.m.decrypt(frame).map_err(js)
    }
}

/// Splits a control message into frame plaintexts, as `u16 len | bytes`
/// records (the batch input format).
#[wasm_bindgen]
pub fn fragment(stream: u8, msg: &[u8]) -> Result<Vec<u8>, JsError> {
    let mut out = Vec::new();
    for f in core::stream::fragment(stream, msg).map_err(js)? {
        out.extend_from_slice(&(f.len() as u16).to_be_bytes());
        out.extend_from_slice(&f);
    }
    Ok(out)
}

/// Puts fragments back together.
#[wasm_bindgen]
pub struct Reassembler {
    r: core::stream::Reassembler,
    stream: u8,
}

#[wasm_bindgen]
impl Reassembler {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Reassembler {
        Reassembler {
            r: core::stream::Reassembler::new(),
            stream: 0,
        }
    }

    /// One frame's plaintext; the whole message once its last fragment is
    /// in (its stream in `stream()`), else undefined.
    pub fn push(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>, JsError> {
        Ok(self.r.push(frame).map_err(js)?.map(|m| {
            self.stream = m.stream;
            m.body
        }))
    }

    pub fn stream(&self) -> u8 {
        self.stream
    }
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

/// The test-vector runner: the inputs JSON in, the outputs JSON out (the
/// WASM test compares it with `crates/e2e/vectors/v1.json`).
#[wasm_bindgen]
pub fn run_vectors(inputs_json: &str) -> Result<String, JsError> {
    core::vectors::run_json(inputs_json).map_err(|e| JsError::new(&e))
}
