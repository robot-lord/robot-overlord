//! Deterministic test vectors: fixed keys, ephemerals and KEM seeds in, every
//! byte the protocol produces out. The same function runs natively and in
//! WASM; both must print the committed JSON (`crates/e2e/vectors/v1.json`,
//! written by `cargo xtask e2e vectors`) byte for byte.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::handshake::FixedRandom;
use crate::session::{pad_into, padme_len};
use crate::stream::{self, Reassembler, CHUNK, CONTROL};
use crate::{
    channel_id, device_id, sha256, Error, Established, Initiator, Kind, MailboxSession, QrPayload,
    RelayId, Responder, Session, StaticKey, MAX_PLAINTEXT, TAG_LEN,
};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Inputs {
    pub host_private: String,
    pub device_private: String,
    pub relay_origin: String,
    pub relay_fingerprint: String,
    pub rogue_relay_fingerprint: String,
    pub rendezvous_id: String,
    pub pairing_secret: String,
    pub pair_device_ephemeral: String,
    pub pair_host_ephemeral: String,
    pub pair_kem_keygen_seed: String,
    pub pair_kem_encaps_seed: String,
    pub kk_device_ephemeral: String,
    pub kk_host_ephemeral: String,
    pub kk_kem_keygen_seed: String,
    pub kk_kem_encaps_seed: String,
    pub mb_device_ephemeral: String,
    pub mb_host_ephemeral: String,
    pub mb_kem_keygen_seed: String,
    pub mb_kem_encaps_seed: String,
    /// Plaintexts (hex).
    pub frames: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Outputs {
    pub host_public: String,
    pub device_public: String,
    pub device_id: String,
    pub qr: String,
    pub pair_msg1: String,
    pub pair_msg2: String,
    pub pair_handshake_hash: String,
    pub pair_short_code: u32,
    /// Live frames on the pairing session, device to host, then host to device.
    pub pair_frames_d2h: Vec<String>,
    pub pair_frames_h2d: Vec<String>,
    pub channel_id: String,
    pub kk_msg1: String,
    pub kk_msg2: String,
    pub kk_handshake_hash: String,
    /// The plaintext frames as one batch, device to host.
    pub kk_batch_d2h: String,
    /// SHA-256 of one live frame carrying `MAX_PLAINTEXT` bytes.
    pub kk_max_frame_sha256: String,
    /// Live session with a rekey every 2 frames.
    pub rekey_frames: Vec<String>,
    pub mb_msg1: String,
    pub mb_msg2: String,
    pub mb_handshake_hash: String,
    /// Mailbox frames device to host (rekey after every message; identical
    /// to snow's transport for the same keys, and with the state exported
    /// and imported between every message), then host to device.
    pub mb_frames_d2h: Vec<String>,
    pub mb_frames_h2d: Vec<String>,
    /// The device's mailbox exported after three messages each way.
    pub mb_export_device: String,
    /// `stream id | flags | chunk length` of each fragment of a message of
    /// `2 * CHUNK + 5` bytes.
    pub fragments: Vec<String>,
    /// Failure cases: name to the error each side reports.
    pub failures: BTreeMap<String, String>,
}

fn h<const N: usize>(s: &str) -> Result<[u8; N], String> {
    let v = hex::decode(s).map_err(|e| e.to_string())?;
    v.try_into().map_err(|_| format!("expected {N} bytes"))
}

fn x(b: &[u8]) -> String {
    hex::encode(b)
}

fn fixed(e: &str, kem: &str) -> Result<FixedRandom, String> {
    let kem_bytes = hex::decode(kem).map_err(|e| e.to_string())?;
    let mut k = [0u8; 64];
    k[..kem_bytes.len()].copy_from_slice(&kem_bytes);
    Ok(FixedRandom {
        ephemeral: h(e)?,
        kem: k,
    })
}

fn err<T>(r: Result<T, Error>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => e.to_string(),
    }
}

struct Parties {
    host: StaticKey,
    device: StaticKey,
    relay: RelayId,
    kd: FixedRandom,
    kh: FixedRandom,
    md: FixedRandom,
    mh: FixedRandom,
}

impl Parties {
    /// A reconnect or mailbox handshake with the fixed randomness:
    /// (device, host, msg1, msg2).
    fn known(&self, kind: Kind) -> Result<(Established, Established, Vec<u8>, Vec<u8>), String> {
        let e = |e: Error| e.to_string();
        let (d, hh) = match kind {
            Kind::Mailbox => (&self.md, &self.mh),
            _ => (&self.kd, &self.kh),
        };
        let (ini, m1) = match kind {
            Kind::Mailbox => {
                Initiator::mailbox(&self.device, self.host.public(), &self.relay, Some(d))
            }
            _ => Initiator::reconnect(&self.device, self.host.public(), &self.relay, Some(d)),
        }
        .map_err(e)?;
        let (he, m2) = match kind {
            Kind::Mailbox => {
                Responder::mailbox(&self.host, self.device.public(), &self.relay, &m1, Some(hh))
            }
            _ => Responder::reconnect(&self.host, self.device.public(), &self.relay, &m1, Some(hh)),
        }
        .map_err(e)?;
        let de = ini.finish(&m2).map_err(e)?;
        if de.handshake_hash != he.handshake_hash {
            return Err(format!("{kind:?} handshake mismatch"));
        }
        Ok((de, he, m1, m2))
    }

    fn live(&self, rekey_every: u64) -> Result<(Session, Session), String> {
        let e = |e: Error| e.to_string();
        let (d, h, _, _) = self.known(Kind::Reconnect)?;
        Ok((
            d.into_session_rekey_every(rekey_every).map_err(e)?,
            h.into_session_rekey_every(rekey_every).map_err(e)?,
        ))
    }

    fn mailbox(&self) -> Result<(MailboxSession, MailboxSession), String> {
        let e = |e: Error| e.to_string();
        let (d, h, _, _) = self.known(Kind::Mailbox)?;
        Ok((d.into_mailbox().map_err(e)?, h.into_mailbox().map_err(e)?))
    }
}

/// Run every case. Errors are strings so the WASM side can pass them out.
pub fn run(i: &Inputs) -> Result<Outputs, String> {
    let e = |e: Error| e.to_string();
    let p = Parties {
        host: StaticKey::from_private(h(&i.host_private)?),
        device: StaticKey::from_private(h(&i.device_private)?),
        relay: RelayId::new(i.relay_origin.clone(), h(&i.relay_fingerprint)?),
        kd: fixed(&i.kk_device_ephemeral, &i.kk_kem_keygen_seed)?,
        kh: fixed(&i.kk_host_ephemeral, &i.kk_kem_encaps_seed)?,
        md: fixed(&i.mb_device_ephemeral, &i.mb_kem_keygen_seed)?,
        mh: fixed(&i.mb_host_ephemeral, &i.mb_kem_encaps_seed)?,
    };
    let (host, device, relay) = (&p.host, &p.device, &p.relay);
    let rogue = RelayId::new(i.relay_origin.clone(), h(&i.rogue_relay_fingerprint)?);
    let qr = QrPayload {
        relay: relay.clone(),
        rendezvous_id: h(&i.rendezvous_id)?,
        host_static: *host.public(),
        pairing_secret: h(&i.pairing_secret)?,
    };
    let qr_s = qr.encode();
    let qr2 = QrPayload::decode(&qr_s).map_err(e)?;
    if qr2 != qr {
        return Err("qr round trip".into());
    }
    let frames: Vec<Vec<u8>> = i
        .frames
        .iter()
        .map(hex::decode)
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let pd = fixed(&i.pair_device_ephemeral, &i.pair_kem_keygen_seed)?;
    let ph = fixed(&i.pair_host_ephemeral, &i.pair_kem_encaps_seed)?;

    // Pairing.
    let (ini, m1) = Initiator::pair(&qr2, device, relay, Some(&pd)).map_err(e)?;
    let (h_est, m2) = Responder::pair(host, &qr, relay, &m1, Some(&ph)).map_err(e)?;
    let d_est = ini.finish(&m2).map_err(e)?;
    if d_est.handshake_hash != h_est.handshake_hash || h_est.remote_static != *device.public() {
        return Err("pairing mismatch".into());
    }
    let pair_hash = d_est.handshake_hash;
    let code = d_est.short_code();
    let (mut ds, mut hs) = (
        d_est.into_session().map_err(e)?,
        h_est.into_session().map_err(e)?,
    );
    let (d2h, h2d) = exchange(&mut ds, &mut hs, &frames)?;

    // Reconnect.
    let (d_est, h_est, k1, k2) = p.known(Kind::Reconnect)?;
    let kk_hash = d_est.handshake_hash;
    let (mut ds, mut hs) = (
        d_est.into_session().map_err(e)?,
        h_est.into_session().map_err(e)?,
    );
    let mut batch_in = Vec::new();
    for f in &frames {
        batch_in.extend_from_slice(&(f.len() as u16).to_be_bytes());
        batch_in.extend_from_slice(f);
    }
    let mut wire = vec![0u8; frames.len() * crate::MAX_WIRE];
    let wn = ds.encrypt_batch(&batch_in, &mut wire).map_err(e)?;
    let mut back = vec![0u8; batch_in.len()];
    let bn = hs.decrypt_batch(&wire[..wn], &mut back).map_err(e)?;
    if back[..bn] != batch_in[..] {
        return Err("batch round trip".into());
    }
    let batch_hex = x(&wire[..wn]);
    let max: Vec<u8> = (0..MAX_PLAINTEXT).map(|i| (i * 13) as u8).collect();
    let max_frame = ds.encrypt(&max).map_err(e)?;
    if hs.decrypt(&max_frame).map_err(e)? != max {
        return Err("max frame round trip".into());
    }

    // Rekey every 2 frames.
    let (mut a, mut b) = p.live(2)?;
    let (rekey, _) = exchange(&mut a, &mut b, &frames[..frames.len().min(5)])?;

    // Mailbox: keyed by kind 3, its own cipher state over the raw split.
    let (mb_d, mb_h, mb1, mb2) = p.known(Kind::Mailbox)?;
    let mb_hash = mb_d.handshake_hash;
    let (mut md, mut mh) = (
        mb_d.into_mailbox().map_err(e)?,
        mb_h.into_mailbox().map_err(e)?,
    );
    let mut mb_d2h = Vec::new();
    let mut mb_h2d = Vec::new();
    let mut export = String::new();
    for (n, f) in frames.iter().enumerate() {
        let w = md.encrypt(f).map_err(e)?;
        if mh.decrypt(&w).map_err(e)? != *f {
            return Err("mailbox d2h mismatch".into());
        }
        mb_d2h.push(x(&w));
        let w = mh.encrypt(f).map_err(e)?;
        if md.decrypt(&w).map_err(e)? != *f {
            return Err("mailbox h2d mismatch".into());
        }
        mb_h2d.push(x(&w));
        if n == 2 {
            export = x(&md.export().map_err(e)?);
        }
    }
    // The same keys through snow's own transport, rekeying after every
    // message, give the same frames.
    let (sd, _, _, _) = p.known(Kind::Mailbox)?;
    let mut snow_d = sd.into_snow_transport().map_err(e)?;
    for (f, want) in frames.iter().zip(&mb_d2h) {
        let padded = padme_len(2 + f.len());
        let mut plain = vec![0u8; padded];
        pad_into(&mut plain, f, padded);
        let mut out = vec![0u8; 2 + padded + TAG_LEN];
        let n = snow_d
            .write_message(&plain, &mut out[2..])
            .map_err(|x| e(x.into()))?;
        out[..2].copy_from_slice(&(n as u16).to_be_bytes());
        snow_d.rekey_outgoing();
        if x(&out) != *want {
            return Err("mailbox frame differs from snow's transport".into());
        }
    }
    // Exported and imported between every message: the same frames.
    let (mut sd, mut sh) = p.mailbox()?;
    for (f, want) in frames.iter().zip(&mb_d2h) {
        sd = MailboxSession::import(&sd.export().map_err(e)?).map_err(e)?;
        let w = sd.encrypt(f).map_err(e)?;
        if x(&w) != *want {
            return Err("mailbox frame differs after import".into());
        }
        sh = MailboxSession::import(&sh.export().map_err(e)?).map_err(e)?;
        if sh.decrypt(&w).map_err(e)? != *f {
            return Err("mailbox import decrypt".into());
        }
    }

    // Fragments.
    let msg: Vec<u8> = (0..2 * CHUNK + 5).map(|i| i as u8).collect();
    let parts = stream::fragment(CONTROL, &msg).map_err(e)?;
    let mut r = Reassembler::new();
    let mut whole = None;
    for f in &parts {
        whole = r.push(f).map_err(e)?.or(whole);
    }
    if whole.map(|m| m.body) != Some(msg) {
        return Err("fragments round trip".into());
    }
    let fragments = parts
        .iter()
        .map(|f| format!("{:02x} {:02x} {}", f[0], f[1], f.len() - 2))
        .collect();

    let failures = failures(&p, &qr, &qr2, &rogue, &frames, &pd, &ph)?;

    Ok(Outputs {
        host_public: x(host.public()),
        device_public: x(device.public()),
        device_id: device_id(device.public()),
        qr: qr_s,
        pair_msg1: x(&m1),
        pair_msg2: x(&m2),
        pair_handshake_hash: x(&pair_hash),
        pair_short_code: code,
        pair_frames_d2h: d2h,
        pair_frames_h2d: h2d,
        channel_id: x(&channel_id(host.public(), device.public())),
        kk_msg1: x(&k1),
        kk_msg2: x(&k2),
        kk_handshake_hash: x(&kk_hash),
        kk_batch_d2h: batch_hex,
        kk_max_frame_sha256: x(&sha256(&[&max_frame])),
        rekey_frames: rekey,
        mb_msg1: x(&mb1),
        mb_msg2: x(&mb2),
        mb_handshake_hash: x(&mb_hash),
        mb_frames_d2h: mb_d2h,
        mb_frames_h2d: mb_h2d,
        mb_export_device: export,
        fragments,
        failures,
    })
}

#[allow(clippy::too_many_arguments)]
fn failures(
    p: &Parties,
    qr: &QrPayload,
    qr2: &QrPayload,
    rogue: &RelayId,
    frames: &[Vec<u8>],
    pd: &FixedRandom,
    ph: &FixedRandom,
) -> Result<BTreeMap<String, String>, String> {
    let e = |e: Error| e.to_string();
    let (host, device, relay) = (&p.host, &p.device, &p.relay);
    let (kd, kh) = (&p.kd, &p.kh);
    let mut failures = BTreeMap::new();
    {
        // Device has a different pairing secret than the host: the device
        // fails on message 2 (the host cannot tell until no frame arrives).
        let mut wrong = qr2.clone();
        wrong.pairing_secret[0] ^= 1;
        let (ini, m1) = Initiator::pair(&wrong, device, relay, Some(pd)).map_err(e)?;
        let (_, m2) = Responder::pair(host, qr, relay, &m1, Some(ph)).map_err(e)?;
        failures.insert("pair_wrong_secret_device".into(), err(ini.finish(&m2)));
    }
    {
        // KEM ciphertext tampered in flight.
        let (ini, m1) = Initiator::pair(qr2, device, relay, Some(pd)).map_err(e)?;
        let (_, mut m2) = Responder::pair(host, qr, relay, &m1, Some(ph)).map_err(e)?;
        m2[2 + 100] ^= 1;
        failures.insert("pair_wrong_kem_ct_device".into(), err(ini.finish(&m2)));
        let (ini, k1) = Initiator::reconnect(device, host.public(), relay, Some(kd)).map_err(e)?;
        let (_, mut k2) =
            Responder::reconnect(host, device.public(), relay, &k1, Some(kh)).map_err(e)?;
        k2[2 + 7] ^= 0x80;
        failures.insert("kk_wrong_kem_ct_device".into(), err(ini.finish(&k2)));
    }
    {
        // A flipped bit in each message 1 and in message 2's Noise part.
        let (_, mut m1) = Initiator::pair(qr2, device, relay, Some(pd)).map_err(e)?;
        m1[40] ^= 1;
        failures.insert(
            "pair_tampered_msg1_host".into(),
            err(Responder::pair(host, qr, relay, &m1, Some(ph))),
        );
        let (ini, k1) = Initiator::reconnect(device, host.public(), relay, Some(kd)).map_err(e)?;
        let mut t1 = k1.clone();
        t1[2 + 32 + 5] ^= 1;
        failures.insert(
            "kk_tampered_msg1_host".into(),
            err(Responder::reconnect(
                host,
                device.public(),
                relay,
                &t1,
                Some(kh),
            )),
        );
        let (_, mut k2) =
            Responder::reconnect(host, device.public(), relay, &k1, Some(kh)).map_err(e)?;
        let last = k2.len() - 1;
        k2[last] ^= 1;
        failures.insert("kk_tampered_msg2_device".into(), err(ini.finish(&k2)));
    }
    {
        // Relay pin: the device's connection reached a relay with another
        // identity. The host (on the real relay) rejects message 1; had a
        // message 2 come back, the device would reject it too.
        let (_, m1) = Initiator::pair(qr2, device, rogue, Some(pd)).map_err(e)?;
        failures.insert(
            "pair_other_relay_host".into(),
            err(Responder::pair(host, qr, relay, &m1, Some(ph))),
        );
        let (_, m1) = Initiator::pair(qr2, device, relay, Some(pd)).map_err(e)?;
        let (_, m2) = Responder::pair(host, qr, relay, &m1, Some(ph)).map_err(e)?;
        let (ini_rogue, _) = Initiator::pair(qr2, device, rogue, Some(pd)).map_err(e)?;
        failures.insert("pair_other_relay_device".into(), err(ini_rogue.finish(&m2)));
        let (_, k1) = Initiator::reconnect(device, host.public(), rogue, Some(kd)).map_err(e)?;
        failures.insert(
            "kk_other_relay_host".into(),
            err(Responder::reconnect(
                host,
                device.public(),
                relay,
                &k1,
                Some(kh),
            )),
        );
        let (_, k1) = Initiator::reconnect(device, host.public(), relay, Some(kd)).map_err(e)?;
        let (_, k2) =
            Responder::reconnect(host, device.public(), relay, &k1, Some(kh)).map_err(e)?;
        let (ini_rogue, _) =
            Initiator::reconnect(device, host.public(), rogue, Some(kd)).map_err(e)?;
        failures.insert("kk_other_relay_device".into(), err(ini_rogue.finish(&k2)));
    }
    {
        // Revoked device: the host no longer knows its key, so it answers
        // only for another device key.
        let other = StaticKey::from_private([7u8; 32]);
        let (_, k1) = Initiator::reconnect(device, host.public(), relay, Some(kd)).map_err(e)?;
        failures.insert(
            "kk_revoked_device_host".into(),
            err(Responder::reconnect(
                host,
                other.public(),
                relay,
                &k1,
                Some(kh),
            )),
        );
    }
    {
        // Wrong kind: a mailbox message 1 given to the reconnect answer, and
        // a reconnect message 2 given to a mailbox initiator; a mailbox
        // handshake made into a live session and the other way round.
        let (_, mb1) = Initiator::mailbox(device, host.public(), relay, Some(&p.md)).map_err(e)?;
        failures.insert(
            "kk_wrong_kind_host".into(),
            err(Responder::reconnect(
                host,
                device.public(),
                relay,
                &mb1,
                Some(kh),
            )),
        );
        let (_, _, _, k2) = p.known(Kind::Reconnect)?;
        let (ini, _) = Initiator::mailbox(device, host.public(), relay, Some(&p.md)).map_err(e)?;
        failures.insert("mailbox_wrong_kind_device".into(), err(ini.finish(&k2)));
        let (d, _, _, _) = p.known(Kind::Mailbox)?;
        failures.insert("mailbox_as_live".into(), err(d.into_session()));
        let (d, _, _, _) = p.known(Kind::Reconnect)?;
        failures.insert("live_as_mailbox".into(), err(d.into_mailbox()));
    }
    {
        let (mut a, mut b) = p.live(crate::LIVE_REKEY_EVERY)?;
        let f1 = a.encrypt(&frames[0]).map_err(e)?;
        let f2 = a.encrypt(&frames[1]).map_err(e)?;
        failures.insert("frame_reordered".into(), err(b.decrypt(&f2)));
        let (mut a, mut b) = p.live(crate::LIVE_REKEY_EVERY)?;
        let f1b = a.encrypt(&frames[0]).map_err(e)?;
        b.decrypt(&f1b).map_err(e)?;
        failures.insert("frame_replayed".into(), err(b.decrypt(&f1b)));
        let mut t = f1.clone();
        let last = t.len() - 1;
        t[last] ^= 1;
        let (_, mut b) = p.live(crate::LIVE_REKEY_EVERY)?;
        failures.insert("frame_tampered".into(), err(b.decrypt(&t)));
        // The session is over: the genuine frame fails too.
        failures.insert("frame_after_poison".into(), err(b.decrypt(&f1)));
        let (_, mut b) = p.live(crate::LIVE_REKEY_EVERY)?;
        failures.insert(
            "frame_truncated".into(),
            err(b.decrypt(&f1[..f1.len() - 1])),
        );
        let mut long = f1.clone();
        long.push(0);
        let (_, mut b) = p.live(crate::LIVE_REKEY_EVERY)?;
        failures.insert("frame_oversized".into(), err(b.decrypt(&long)));
    }
    {
        let (mut d, mut h) = p.mailbox()?;
        let w1 = d.encrypt(&frames[1]).map_err(e)?;
        let w2 = d.encrypt(&frames[2]).map_err(e)?;
        failures.insert("mailbox_gap".into(), err(h.decrypt(&w2)));
        let (_, mut h) = p.mailbox()?;
        h.decrypt(&w1).map_err(e)?;
        failures.insert("mailbox_replayed".into(), err(h.decrypt(&w1)));
        let mut t = w1.clone();
        t[5] ^= 1;
        let (_, mut h) = p.mailbox()?;
        failures.insert("mailbox_tampered".into(), err(h.decrypt(&t)));
        failures.insert("mailbox_after_poison".into(), err(h.decrypt(&w1)));
        failures.insert("mailbox_export_after_poison".into(), err(h.export()));
    }
    {
        let mut r = Reassembler::new();
        failures.insert("stream_bad_flags".into(), err(r.push(&[0, 2, 1])));
        failures.insert("stream_too_short".into(), err(r.push(&[0])));
    }
    Ok(failures)
}

/// Device sends every frame, host decrypts and checks; then the reverse.
fn exchange(
    d: &mut Session,
    hs: &mut Session,
    frames: &[Vec<u8>],
) -> Result<(Vec<String>, Vec<String>), String> {
    let e = |e: Error| e.to_string();
    let mut d2h = Vec::new();
    let mut h2d = Vec::new();
    for f in frames {
        let w = d.encrypt(f).map_err(e)?;
        if hs.decrypt(&w).map_err(e)? != *f {
            return Err("d2h mismatch".into());
        }
        d2h.push(x(&w));
    }
    for f in frames {
        let w = hs.encrypt(f).map_err(e)?;
        if d.decrypt(&w).map_err(e)? != *f {
            return Err("h2d mismatch".into());
        }
        h2d.push(x(&w));
    }
    Ok((d2h, h2d))
}

/// JSON in, JSON out (for WASM).
pub fn run_json(inputs: &str) -> Result<String, String> {
    let i: Inputs = serde_json::from_str(inputs).map_err(|e| e.to_string())?;
    let o = run(&i)?;
    serde_json::to_string_pretty(&o).map_err(|e| e.to_string())
}

/// The committed inputs: arbitrary fixed bytes, frame sizes across the
/// padding edges. (The largest frame is covered by its hash only, to keep
/// the file small.)
pub fn default_inputs() -> Inputs {
    let b = |v: u8, n: usize| hex::encode(vec![v; n]);
    let sizes = [0usize, 1, 2, 29, 30, 31, 32, 100, 1000, 4096];
    Inputs {
        host_private: b(0x11, 32),
        device_private: b(0x22, 32),
        relay_origin: "https://relay.example.test".into(),
        relay_fingerprint: b(0x33, 32),
        rogue_relay_fingerprint: b(0x34, 32),
        rendezvous_id: b(0x44, 16),
        pairing_secret: b(0x55, 32),
        pair_device_ephemeral: b(0x61, 32),
        pair_host_ephemeral: b(0x62, 32),
        pair_kem_keygen_seed: b(0x63, 64),
        pair_kem_encaps_seed: b(0x64, 32),
        kk_device_ephemeral: b(0x71, 32),
        kk_host_ephemeral: b(0x72, 32),
        kk_kem_keygen_seed: b(0x73, 64),
        kk_kem_encaps_seed: b(0x74, 32),
        mb_device_ephemeral: b(0x81, 32),
        mb_host_ephemeral: b(0x82, 32),
        mb_kem_keygen_seed: b(0x83, 64),
        mb_kem_encaps_seed: b(0x84, 32),
        frames: sizes
            .iter()
            .map(|&n| hex::encode((0..n).map(|i| (i * 7 + n) as u8).collect::<Vec<_>>()))
            .collect(),
    }
}

/// The committed file's text: what `cargo xtask e2e vectors` writes and a
/// test compares.
pub fn document() -> Result<String, String> {
    let inputs = default_inputs();
    let outputs = run(&inputs)?;
    let doc = serde_json::json!({
        "about": "rlord-e2e v1 test vectors. Written by `cargo xtask e2e vectors`; checked by native tests and by the WASM build in Node.",
        "inputs": inputs,
        "outputs": outputs,
    });
    serde_json::to_string_pretty(&doc)
        .map(|s| s + "\n")
        .map_err(|e| e.to_string())
}
