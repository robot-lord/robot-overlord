//! The `rlord-relay` binary itself: the commands, `serve` as a process
//! (its stdout lines, SIGTERM), `files` mode with a certificate re-read on
//! SIGHUP, and that no payload reaches `relay.db` or the log.

mod common;

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use rlord_relay_proto::frame::{FromDevice, FromHost, ToDevice, ToHost};
use rlord_relay_proto::msg::{FromRelay, LinkKind, ToRelay};
use rlord_relay_proto::{check_challenge, RelayAddress, B64};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::Message;

const BIN: &str = env!("CARGO_BIN_EXE_rlord-relay");

fn run(config: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(BIN)
        .arg("--config")
        .arg(config)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn write_config(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("relay.toml");
    std::fs::write(
        &path,
        format!(
            "data_dir = {:?}\n{body}",
            dir.join("data").display().to_string()
        ),
    )
    .unwrap();
    path
}

#[test]
fn the_commands() {
    let (ok, out, _) = run(Path::new("/nonexistent"), &["--version"]);
    assert!(ok);
    assert_eq!(
        out.trim(),
        format!("rlord-relay {}", env!("CARGO_PKG_VERSION"))
    );

    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(
        dir.path(),
        "origin = \"https://relay.example.com\"\n[tls]\nmode = \"proxy\"\n",
    );
    let (ok, out, err) = run(&cfg, &["check"]);
    assert!(ok, "{err}");
    assert!(out.ends_with("ok\n"), "{out}");
    let data = dir.path().join("data");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&data), 0o700);
    assert_eq!(mode(&data.join("relay.key")), 0o600);
    assert_eq!(mode(&data.join("relay.db")), 0o600);

    // The address parses and stays the same (the key is kept).
    let (ok, out, _) = run(&cfg, &["address"]);
    assert!(ok);
    let a = RelayAddress::parse(out.trim()).unwrap();
    assert_eq!(a.origin, "https://relay.example.com");
    assert_eq!(run(&cfg, &["address"]).1, out);

    // Tokens and hosts.
    let (ok, token, _) = run(
        &cfg,
        &[
            "token",
            "new",
            "--uses",
            "2",
            "--ttl",
            "1h",
            "--account",
            "team",
        ],
    );
    assert!(ok);
    assert!(token.trim().starts_with("rlt_"));
    assert!(!run(&cfg, &["token", "new", "--ttl", "0"]).0);
    let fp = B64(vec![9; 32]).to_string();
    assert!(run(&cfg, &["host", "add", &fp, "--account", "team"]).0);
    assert!(!run(&cfg, &["host", "add", &fp]).0, "already registered");
    let list = run(&cfg, &["host", "list"]).1;
    assert!(
        list.contains(&fp) && list.contains("team") && list.contains("active"),
        "{list}"
    );
    assert!(run(&cfg, &["host", "suspend", &fp]).0);
    assert!(run(&cfg, &["host", "list"]).1.contains("suspended"));
    assert!(run(&cfg, &["host", "resume", &fp]).0);
    assert!(run(&cfg, &["host", "remove", &fp]).0);
    assert!(!run(&cfg, &["host", "remove", &fp]).0);
    assert_eq!(run(&cfg, &["host", "list"]).1, "");
    assert!(!run(&cfg, &["host", "add", "not-a-fingerprint"]).0);

    // A config that can't work fails `check`, saying why.
    let bad = write_config(
        dir.path(),
        "origin = \"https://relay.example.com/x\"\n[tls]\nmode = \"proxy\"\n",
    );
    let (ok, _, err) = run(&bad, &["check"]);
    assert!(!ok);
    assert!(err.contains("origin"), "{err}");
    let files = write_config(
        dir.path(),
        "origin = \"https://relay.example.com\"\n[tls]\ncert = \"/nonexistent.pem\"\nkey = \"/nonexistent.key\"\n",
    );
    let (ok, _, err) = run(&files, &["check"]);
    assert!(!ok);
    assert!(err.contains("nonexistent.pem"), "{err}");
}

/// `rlord-relay serve` as a child process.
struct Served {
    child: Child,
    addr: RelayAddress,
    log: PathBuf,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(cfg: &Path, rust_log: &str) -> Served {
    let log = cfg.with_file_name("relay.log");
    let mut child = Command::new(BIN)
        .arg("--config")
        .arg(cfg)
        .arg("serve")
        .env("RUST_LOG", rust_log)
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let listening = lines.next().unwrap().unwrap();
    assert!(
        listening.starts_with("listening on 127.0.0.1:"),
        "{listening}"
    );
    let address = lines.next().unwrap().unwrap();
    let addr = RelayAddress::parse(address.strip_prefix("address ").unwrap()).unwrap();
    assert!(addr.origin.ends_with(listening.rsplit(':').next().unwrap()));
    Served { child, addr, log }
}

fn signal(child: &Child, sig: i32) {
    unsafe { libc::kill(child.id() as i32, sig) };
}

#[tokio::test]
async fn no_payload_reaches_the_database_or_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(
        dir.path(),
        "listen = \"127.0.0.1:0\"\n[tls]\nmode = \"proxy\"\n",
    );
    // Every library at trace: the relay still logs no payload.
    let mut s = serve(&cfg, "trace");
    let token = run(&cfg, &["token", "new"]).1.trim().to_string();
    let url = s.addr.ws_url();
    let hk = id();
    let mut h = handshake(open(&url).await, &s.addr, &hk, &As::Host(Some(token)))
        .await
        .unwrap();
    let dk = id();
    h.allow(&[dk.fingerprint()]).await;
    let mut d = handshake(
        open(&url).await,
        &s.addr,
        &dk,
        &As::Device(hk.fingerprint()),
    )
    .await
    .unwrap();
    let FromRelay::Link {
        id: link,
        kind: LinkKind::Device,
        ..
    } = h.ctl().await
    else {
        panic!()
    };
    const MARKER: &[u8] = b"RLORD-PAYLOAD-MARKER-3f9a";
    // Live both ways, and mailbox both ways (queued in memory).
    d.send_bin(FromDevice::Live(MARKER.to_vec()).encode()).await;
    assert!(
        matches!(ToHost::decode(&h.bin().await).unwrap(), ToHost::Live { payload, .. } if payload == MARKER)
    );
    h.send_bin(
        FromHost::Live {
            link,
            payload: MARKER.to_vec(),
        }
        .encode(),
    )
    .await;
    assert_eq!(
        ToDevice::decode(&d.bin().await).unwrap(),
        ToDevice::Live(MARKER.to_vec())
    );
    h.send_bin(
        FromHost::Mailbox {
            device: dk.fingerprint(),
            payload: MARKER.to_vec(),
        }
        .encode(),
    )
    .await;
    assert!(matches!(h.ctl().await, FromRelay::Queued { .. }));
    assert!(matches!(
        ToDevice::decode(&d.bin().await).unwrap(),
        ToDevice::Mailbox { .. }
    ));
    d.send_bin(FromDevice::Mailbox(MARKER.to_vec()).encode())
        .await;
    assert!(matches!(d.ctl().await, FromRelay::Queued { .. }));
    assert!(matches!(
        ToHost::decode(&h.bin().await).unwrap(),
        ToHost::Mailbox { .. }
    ));
    // An unacknowledged message is still queued when the relay stops.
    h.close().await;
    assert!(matches!(d.ctl().await, FromRelay::Host { .. }));
    d.send_bin(FromDevice::Mailbox(MARKER.to_vec()).encode())
        .await;
    assert!(matches!(d.ctl().await, FromRelay::Queued { .. }));
    d.quiet(Duration::from_millis(200)).await;
    // Stopped by SIGTERM: clients get `bye` and 4503.
    signal(&s.child, libc::SIGTERM);
    let (bye, code) = d.bye_and_close().await;
    assert_eq!(code, Some(rlord_relay_proto::msg::close::OVERLOADED));
    assert!(bye.is_some());
    assert!(s.child.wait().unwrap().success());

    let hex: String = MARKER.iter().map(|b| format!("{b:02x}")).collect();
    let b64 = B64(MARKER.to_vec()).to_string();
    let mut checked = 0;
    for entry in std::fs::read_dir(dir.path().join("data")).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        assert!(!contains(&bytes, MARKER), "payload in {}", path.display());
        checked += 1;
    }
    assert!(checked >= 2, "relay.key and relay.db at least");
    let log = std::fs::read(&s.log).unwrap();
    assert!(!log.is_empty());
    for needle in [
        MARKER,
        hex.as_bytes(),
        hex.to_uppercase().as_bytes(),
        b64.as_bytes(),
    ] {
        assert!(!contains(&log, needle), "payload in the log");
    }
    // The log names keys by 8 hex characters only.
    let log = String::from_utf8_lossy(&log);
    let full = hk
        .fingerprint()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert!(log.contains(&full[..8]), "{log}");
    assert!(!log.contains(&full[..12]));
    assert!(!log.contains(&B64(hk.fingerprint().to_vec()).to_string()));
    s.child.kill().ok();
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The TLS client a peer uses, trusting only `ca`.
fn tls(ca: &str) -> tokio_rustls::TlsConnector {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from_pem_slice(ca.as_bytes()).unwrap())
        .unwrap();
    let c = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(c))
}

/// Over TLS: the upgrade, then the relay's challenge checked against the
/// pin. Fails when the certificate isn't the one `ca` signed.
async fn challenge_over_tls(addr: &RelayAddress, ca: &str) -> Result<(), String> {
    let port: u16 = addr.origin.rsplit(':').next().unwrap().parse().unwrap();
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
    let stream = tls(ca)
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (ws, _) = tokio_tungstenite::client_async(addr.ws_url(), stream)
        .await
        .map_err(|e| e.to_string())?;
    challenge(ws, addr).await;
    Ok(())
}

async fn challenge<S: AsyncRead + AsyncWrite + Unpin>(
    mut ws: tokio_tungstenite::WebSocketStream<S>,
    addr: &RelayAddress,
) {
    let key = id();
    let n = nonce();
    ws.send(Message::text(
        ToRelay::Hello(As::Host(None).hello(&key, n)).to_json(),
    ))
    .await
    .unwrap();
    let Some(Ok(Message::Text(t))) = ws.next().await else {
        panic!()
    };
    let FromRelay::Challenge {
        origin,
        key: rk,
        nonce: rn,
        sig,
    } = FromRelay::parse(&t).unwrap()
    else {
        panic!()
    };
    check_challenge(addr, &key.public(), &n, &origin, &rk.0, &rn.0, &sig.0).unwrap();
}

#[tokio::test]
async fn files_mode_terminates_tls_and_rereads_on_sighup() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = (
        dir.path().join("fullchain.pem"),
        dir.path().join("privkey.pem"),
    );
    let make = || {
        let c = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
        (c.cert.pem(), c.signing_key.serialize_pem())
    };
    let (ca1, key1) = make();
    std::fs::write(&cert, &ca1).unwrap();
    std::fs::write(&key, &key1).unwrap();
    let cfg = write_config(
        dir.path(),
        &format!(
            "listen = \"127.0.0.1:0\"\n[tls]\nmode = \"files\"\ncert = {:?}\nkey = {:?}\n",
            cert.display().to_string(),
            key.display().to_string()
        ),
    );
    let s = serve(&cfg, "info");
    assert!(s.addr.origin.starts_with("https://127.0.0.1:"));
    challenge_over_tls(&s.addr, &ca1).await.unwrap();

    // Renewed on disk; SIGHUP; the new certificate is served, no restart.
    let (ca2, key2) = make();
    std::fs::write(&cert, &ca2).unwrap();
    std::fs::write(&key, &key2).unwrap();
    signal(&s.child, libc::SIGHUP);
    let mut served = false;
    for _ in 0..100 {
        if challenge_over_tls(&s.addr, &ca2).await.is_ok() {
            served = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(served, "the new certificate was not served after SIGHUP");
    assert!(challenge_over_tls(&s.addr, &ca1).await.is_err());

    // A broken file on reload keeps the old certificate.
    std::fs::write(&cert, "not a certificate").unwrap();
    signal(&s.child, libc::SIGHUP);
    tokio::time::sleep(Duration::from_millis(200)).await;
    challenge_over_tls(&s.addr, &ca2).await.unwrap();
    // Plain HTTP to a TLS port goes nowhere.
    assert!(
        tokio_tungstenite::connect_async(s.addr.ws_url().replace("wss://", "ws://"))
            .await
            .is_err()
    );
}
