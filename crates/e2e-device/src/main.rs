//! `rlord-device`: pair a test device by hand and talk to the host through
//! the relay, without a phone. It runs the phone's own device engine
//! (`rlord_e2e_client`) with software keys kept in a folder (`--state`);
//! never use it for anything but testing.
//!
//! - `rlord-device pair <RLORD1:…> --state DIR [--name NAME]`
//! - `rlord-device check --state DIR`: the background check ("anything
//!   for me?": no handshake when nothing waits)
//! - `rlord-device snapshot --state DIR`
//! - `rlord-device pane <target> --state DIR`: a session's (or the
//!   overlord's) live pane in this terminal, in raw mode, through the
//!   engine's pane stream (as the phone's terminal view): every key goes
//!   to the pane; `Ctrl-]` leaves. `--take` takes control first.
//! - `rlord-device bridge --state DIR [--port N] [--ca FILE] [--qr RLORD1:…]
//!   [--name NAME] [--origin ORIGIN]…`: the engine behind a loopback
//!   websocket that plays the phone shell's message bridge, for the phone
//!   page in a desktop browser or in Node (`cargo xtask app dev`, `app
//!   test-e2e`; see `bridge.rs`)
//! - `rlord-device interop-host`: a native host core on stdin/stdout, one
//!   JSON object per line, for the WASM interop test (`cargo xtask e2e
//!   test-wasm`): it prints a QR, answers a pairing and a reconnect, and
//!   echoes every frame back encrypted.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_e2e::StaticKey;
use rlord_e2e_device::{b64, unb64, Phone};

const USAGE: &str = "usage: rlord-device pair <RLORD1:…> --state DIR [--name NAME]
       rlord-device check|snapshot --state DIR
       rlord-device pane <target> --state DIR [--take]
       rlord-device bridge --state DIR [--port N] [--ca FILE] [--qr RLORD1:…] [--name NAME] [--origin ORIGIN]…";

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    if let Err(e) = rt.block_on(run(std::env::args().skip(1).collect())) {
        eprintln!("rlord-device: {e:#}");
        std::process::exit(1);
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// The native half of the WASM interop test.
fn interop_host() -> Result<()> {
    use rlord_e2e::{QrPayload, RelayId, Responder, Session};
    use std::io::{BufRead, Write};
    let relay = RelayId::new("http://127.0.0.1:1", [7; 32]);
    let host = StaticKey::generate().map_err(|e| anyhow::anyhow!("{e}"))?;
    let qr = QrPayload::new(relay.clone(), *host.public()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut out = std::io::stdout();
    let mut say = |v: Value| -> Result<()> {
        writeln!(out, "{v}")?;
        out.flush()?;
        Ok(())
    };
    say(
        json!({"qr": qr.encode(), "origin": relay.origin, "fingerprint": b64(&relay.fingerprint)}),
    )?;
    let mut session: Option<Session> = None;
    for line in std::io::stdin().lock().lines() {
        let v: Value = serde_json::from_str(&line?)?;
        let e = |e: rlord_e2e::Error| anyhow::anyhow!("{e}");
        if let Some(m1) = v["pair"].as_str() {
            let (est, m2) = Responder::pair(&host, &qr, &relay, &unb64(m1)?, None).map_err(e)?;
            say(json!({"msg2": b64(&m2), "code": est.short_code()}))?;
            session = Some(est.into_session().map_err(e)?);
        } else if let Some(m1) = v["reconnect"].as_str() {
            let dev: [u8; 32] = unb64(v["device"].as_str().unwrap_or(""))?
                .try_into()
                .map_err(|_| anyhow::anyhow!("device key"))?;
            let (est, m2) =
                Responder::reconnect(&host, &dev, &relay, &unb64(m1)?, None).map_err(e)?;
            say(json!({"msg2": b64(&m2)}))?;
            session = Some(est.into_session().map_err(e)?);
        } else if let Some(f) = v["frame"].as_str() {
            let s = session.as_mut().context("no session")?;
            let p = s.decrypt(&unb64(f)?).map_err(e)?;
            say(json!({"frame": b64(&s.encrypt(&p).map_err(e)?)}))?;
        } else {
            break;
        }
    }
    Ok(())
}

async fn run(args: Vec<String>) -> Result<()> {
    if args.first().map(String::as_str) == Some("interop-host") {
        return interop_host();
    }
    if args.first().map(String::as_str) == Some("bridge") {
        return rlord_e2e_device::bridge::run(&args[1..]).await;
    }
    let state = PathBuf::from(flag(&args, "--state").context(USAGE)?);
    let mut phone = Phone::open(&state)?;
    let r = command(&mut phone, &args).await;
    phone.engine.shutdown();
    r
}

async fn command(phone: &mut Phone, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("pair") => {
            let qr = args.get(1).context(USAGE)?;
            let name = flag(args, "--name").unwrap_or_else(|| "rlord-device".into());
            let code = phone.start_pairing(qr, &name).await?;
            println!("code {code}: check that the computer shows the same, and confirm there");
            phone.pairing_outcome(Duration::from_secs(130)).await?;
            let s = phone.status().await?;
            println!(
                "paired with {} (device {})",
                s["host"].as_str().unwrap_or("?"),
                s["device_id"].as_str().unwrap_or("?")
            );
        }
        Some("check") => {
            println!("{:?}", phone.background_check(20_000).await);
        }
        Some("snapshot") => {
            let a = live_request(phone, "snapshot", json!({})).await?;
            println!("{}", serde_json::to_string_pretty(&a)?);
        }
        Some("pane") => {
            let target = args.get(1).context(USAGE)?;
            pane(phone, target, args.iter().any(|a| a == "--take")).await?;
        }
        _ => bail!(USAGE),
    }
    Ok(())
}

/// In the foreground until live, then one request.
async fn live_request(phone: &mut Phone, op: &str, args: Value) -> Result<Value> {
    phone.engine.lifecycle(true);
    phone.conn("live", Duration::from_secs(20)).await?;
    phone.request(op, args, Duration::from_secs(30)).await
}

/// This terminal's size (80x24 when it isn't one).
fn term_size() -> (u16, u16) {
    // Safety: TIOCGWINSZ into a struct we own.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ as _, &mut ws) == 0 && ws.ws_col > 1 && ws.ws_row > 1 {
            return (ws.ws_col, ws.ws_row);
        }
    }
    (80, 24)
}

/// Raw mode on stdin until dropped.
struct Raw(Option<libc::termios>);

impl Raw {
    fn on() -> Raw {
        // Safety: tcgetattr/tcsetattr on stdin with structs we own.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Raw(None);
            }
            let old = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Raw(Some(old))
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            // Safety: as above.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }
}

/// A live pane in this terminal (by hand: the phone's terminal without
/// the phone).
async fn pane(phone: &mut Phone, target: &str, take: bool) -> Result<()> {
    use base64::Engine as _;
    use std::io::{Read, Write};
    let b64s = base64::engine::general_purpose::STANDARD;
    phone.engine.lifecycle(true);
    phone.conn("live", Duration::from_secs(20)).await?;
    let (cols, rows) = term_size();
    if take {
        let a = phone
            .request(
                "pane.take",
                json!({"target": target, "cols": cols, "rows": rows}),
                Duration::from_secs(10),
            )
            .await?;
        if a.get("error").is_some() {
            bail!("pane.take: {a}");
        }
    }
    let a = phone
        .request(
            "pane.open",
            json!({"stream": 1, "target": target, "cols": cols, "rows": rows}),
            Duration::from_secs(20),
        )
        .await?;
    if a.get("error").is_some() {
        bail!("pane.open: {a}");
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let raw = Raw::on();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let (mut bytes, mut frames) = (0u64, 0u64);
    let mut out = std::io::stdout();
    let why = loop {
        tokio::select! {
            k = rx.recv() => {
                let Some(k) = k else { break "stdin closed".to_string() };
                if k.contains(&0x1d) {
                    phone.say(json!({"pane": "close", "stream": 1}));
                    break "left".to_string();
                }
                phone.say(json!({"pane": "input", "stream": 1, "b": b64s.encode(&k)}));
            }
            ev = phone.wait("pane", Duration::from_millis(200), |v| v["event"] == "pane") => {
                let Ok(ev) = ev else { continue };
                match ev["type"].as_str() {
                    Some("data") => {
                        let d = b64s.decode(ev["b"].as_str().unwrap_or("")).unwrap_or_default();
                        bytes += d.len() as u64;
                        frames += 1;
                        let _ = out.write_all(&d);
                        let _ = out.flush();
                        phone.say(json!({"pane": "ack", "stream": 1, "bytes": bytes, "frames": frames}));
                    }
                    Some("closed") => break format!("closed: {} {}", ev["reason"], ev["message"]),
                    _ => {}
                }
            }
        }
    };
    drop(raw);
    println!("\r\nrlord-device: pane {why}");
    Ok(())
}
