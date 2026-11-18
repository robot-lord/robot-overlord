use rlord_e2e::vectors::{self, Outputs};
use rlord_e2e::*;

fn relay() -> RelayId {
    RelayId::new("http://127.0.0.1:9000", [9u8; 32])
}

fn paired() -> (StaticKey, StaticKey, Session, Session) {
    let host = StaticKey::generate().unwrap();
    let device = StaticKey::generate().unwrap();
    let qr = QrPayload::new(relay(), *host.public()).unwrap();
    let scanned = QrPayload::decode(&qr.encode()).unwrap();
    let (ini, m1) = Initiator::pair(&scanned, &device, &scanned.relay, None).unwrap();
    let (h, m2) = Responder::pair(&host, &qr, &relay(), &m1, None).unwrap();
    let d = ini.finish(&m2).unwrap();
    assert_eq!(d.short_code(), h.short_code());
    assert_eq!(h.remote_static, *device.public());
    assert_eq!(d.remote_static, *host.public());
    (
        host,
        device,
        d.into_session().unwrap(),
        h.into_session().unwrap(),
    )
}

fn reconnect(host: &StaticKey, device: &StaticKey) -> (Session, Session) {
    let (ini, k1) = Initiator::reconnect(device, host.public(), &relay(), None).unwrap();
    let (he, k2) = Responder::reconnect(host, device.public(), &relay(), &k1, None).unwrap();
    let de = ini.finish(&k2).unwrap();
    (de.into_session().unwrap(), he.into_session().unwrap())
}

fn mailbox(host: &StaticKey, device: &StaticKey) -> (MailboxSession, MailboxSession) {
    let (ini, m1) = Initiator::mailbox(device, host.public(), &relay(), None).unwrap();
    assert_eq!(Kind::of_message(&m1).unwrap(), Kind::Mailbox);
    let (he, m2) = Responder::mailbox(host, device.public(), &relay(), &m1, None).unwrap();
    let de = ini.finish(&m2).unwrap();
    assert_eq!(de.kind(), Kind::Mailbox);
    (de.into_mailbox().unwrap(), he.into_mailbox().unwrap())
}

#[test]
fn pair_then_reconnect_random_keys() {
    let (host, device, mut d, mut h) = paired();
    let w = d.encrypt(b"\0hello").unwrap();
    assert_eq!(h.decrypt(&w).unwrap(), b"\0hello");
    let (mut d, mut h) = reconnect(&host, &device);
    for n in [0, 1, 31, 32, 33, 1000, MAX_PLAINTEXT] {
        let p: Vec<u8> = (0..n).map(|i| i as u8).collect();
        let w = h.encrypt(&p).unwrap();
        assert_eq!(w.len(), live_wire_len(n));
        assert_eq!((w.len() - 2 - TAG_LEN) % LIVE_PAD, 0);
        assert_eq!(d.decrypt(&w).unwrap(), p);
    }
    assert!(h.encrypt(&vec![0; MAX_PLAINTEXT + 1]).is_err());
    // A too-large plaintext is the caller's error, not the session's end.
    assert!(!h.is_poisoned());
}

#[test]
fn message_sizes() {
    let host = StaticKey::generate().unwrap();
    let device = StaticKey::generate().unwrap();
    let qr = QrPayload::new(relay(), *host.public()).unwrap();
    let (ini, m1) = Initiator::pair(&qr, &device, &relay(), None).unwrap();
    let (_, m2) = Responder::pair(&host, &qr, &relay(), &m1, None).unwrap();
    ini.finish(&m2).unwrap();
    let (ini, k1) = Initiator::reconnect(&device, host.public(), &relay(), None).unwrap();
    let (_, k2) = Responder::reconnect(&host, device.public(), &relay(), &k1, None).unwrap();
    ini.finish(&k2).unwrap();
    // 2 header + e 32 + s 32+16 + payload 1184+16
    assert_eq!(m1.len(), 2 + 32 + 48 + KEM_EK_LEN + 16);
    assert_eq!(m2.len(), 2 + KEM_CT_LEN + 32 + 16);
    assert_eq!(k1.len(), 2 + 32 + KEM_EK_LEN + 16);
}

#[test]
fn in_place_and_batch() {
    let (_, _, mut d, mut h) = paired();
    let mut buf = vec![0u8; MAX_WIRE];
    buf[..5].copy_from_slice(b"\x01abcd");
    let n = d.encrypt_in_place(&mut buf, 5).unwrap();
    assert_eq!(n, 2 + 32 + 16);
    let m = h.decrypt_in_place(&mut buf, n).unwrap();
    assert_eq!(&buf[..m], b"\x01abcd");

    let mut input = Vec::new();
    for i in 0..300u16 {
        let len = (i as usize * 37) % 2000;
        input.extend_from_slice(&(len as u16).to_be_bytes());
        input.extend((0..len).map(|j| (j + i as usize) as u8));
    }
    let mut wire = vec![0u8; 300 * MAX_WIRE];
    let wn = d.encrypt_batch(&input, &mut wire).unwrap();
    let mut out = vec![0u8; input.len()];
    let on = h.decrypt_batch(&wire[..wn], &mut out).unwrap();
    assert_eq!(&out[..on], &input[..]);
}

#[test]
fn rekey_keeps_both_sides_in_step() {
    let (host, device, _, _) = paired();
    let (ini, k1) = Initiator::reconnect(&device, host.public(), &relay(), None).unwrap();
    let (he, k2) = Responder::reconnect(&host, device.public(), &relay(), &k1, None).unwrap();
    let (mut d, mut h) = (
        ini.finish(&k2)
            .unwrap()
            .into_session_rekey_every(3)
            .unwrap(),
        he.into_session_rekey_every(3).unwrap(),
    );
    for i in 0..20u8 {
        let w = d.encrypt(&[i; 40]).unwrap();
        assert_eq!(h.decrypt(&w).unwrap(), vec![i; 40]);
    }
}

#[test]
fn any_decrypt_failure_ends_the_session() {
    let (host, device, _, _) = paired();
    let (mut d, mut h) = reconnect(&host, &device);
    let good = d.encrypt(b"\0one").unwrap();
    let mut bad = good.clone();
    bad[4] ^= 1;
    assert_eq!(h.decrypt(&bad), Err(Error::Decrypt));
    assert!(h.is_poisoned());
    // The genuine frame no longer decrypts, and nothing can be sent.
    assert_eq!(h.decrypt(&good), Err(Error::Decrypt));
    assert_eq!(h.encrypt(b"x"), Err(Error::Decrypt));
    // A malformed frame ends it too.
    let (mut d, mut h) = reconnect(&host, &device);
    let good = d.encrypt(b"\0one").unwrap();
    assert!(h.decrypt(&good[..3]).is_err());
    assert_eq!(h.decrypt(&good), Err(Error::Decrypt));
}

#[test]
fn mailbox_round_trip_and_persistence() {
    let (host, device, _, _) = paired();
    let (mut d, mut h) = mailbox(&host, &device);
    let req = br#"{"id":"r1","op":"overlord.ask","args":{"text":"hi"}}"#;
    let w = d.encrypt(req).unwrap();
    assert_eq!(w.len(), mailbox_wire_len(req.len()));
    // The host saves after acting; reloads; answers.
    assert_eq!(h.decrypt(&w).unwrap(), req);
    let mut h = MailboxSession::import(&h.export().unwrap()).unwrap();
    let w = h.encrypt(b"answer").unwrap();
    let mut d = MailboxSession::import(&d.export().unwrap()).unwrap();
    assert_eq!(d.decrypt(&w).unwrap(), b"answer");
    // Live and mailbox handshakes don't mix.
    let (ini, m1) = Initiator::mailbox(&device, host.public(), &relay(), None).unwrap();
    assert!(Responder::reconnect(&host, device.public(), &relay(), &m1, None).is_err());
    let (he, m2) = Responder::mailbox(&host, device.public(), &relay(), &m1, None).unwrap();
    assert!(he.into_session().is_err());
    assert!(ini.finish(&m2).unwrap().into_mailbox().is_ok());
}

#[test]
fn a_revoked_device_cannot_key_a_mailbox() {
    let (host, device, _, _) = paired();
    let other = StaticKey::generate().unwrap();
    let (_, m1) = Initiator::mailbox(&device, host.public(), &relay(), None).unwrap();
    assert!(Responder::mailbox(&host, other.public(), &relay(), &m1, None).is_err());
}

fn committed() -> serde_json::Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/vectors/v1.json");
    let s = std::fs::read_to_string(path)
        .expect("crates/e2e/vectors/v1.json: run `cargo xtask e2e vectors`");
    serde_json::from_str(&s).unwrap()
}

#[test]
fn vectors_match_committed() {
    let v = committed();
    let i: vectors::Inputs = serde_json::from_value(v["inputs"].clone()).unwrap();
    let o: Outputs = serde_json::from_value(v["outputs"].clone()).unwrap();
    assert_eq!(i, vectors::default_inputs());
    assert_eq!(vectors::run(&i).unwrap(), o);
    for (case, result) in &o.failures {
        assert_ne!(result, "ok", "{case} must fail");
    }
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/vectors/v1.json");
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        vectors::document().unwrap(),
        "stale: run `cargo xtask e2e vectors`"
    );
}
