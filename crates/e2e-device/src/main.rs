//! `rlord-device`: pair a test device by hand and talk to the host through
//! the relay, without a phone. It runs the phone's own device engine
//! (`rlord_e2e_client`) with software keys kept in a folder (`--state`);
//! never use it for anything but testing.
//!
//! - `rlord-device pair <RLORD1:…> --state DIR [--name NAME]`
//! - `rlord-device check --state DIR`: the background check ("anything
//!   for me?": no handshake when nothing waits)
//! - `rlord-device snapshot --state DIR`
//! - `rlord-device ask <text> --state DIR`
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
       rlord-device ask <text> --state DIR";

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
        Some("ask") => {
            let text = args.get(1).context(USAGE)?;
            let a = live_request(phone, "overlord.ask", json!({"text": text})).await?;
            println!("{a}");
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
