//! `rlord-device`: pair a test device by hand and talk to the host through
//! the relay, without a phone. Its state (keys and the paired host) is a
//! JSON file; never use it for anything but testing.
//!
//! - `rlord-device pair <RLORD1:…> --state FILE [--name NAME]`
//! - `rlord-device check --state FILE`: the cheap "anything for me?"
//! - `rlord-device snapshot --state FILE`
//! - `rlord-device ask <text> --state FILE`
//! - `rlord-device interop-host`: a native host core on stdin/stdout, one
//!   JSON object per line, for the WASM interop test (`cargo xtask e2e
//!   test-wasm`): it prints a QR, answers a pairing and a reconnect, and
//!   echoes every frame back encrypted.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_e2e::StaticKey;
use rlord_e2e_device::{b64, unb64, Device, Host};
use rlord_relay_proto::address::{b64_fingerprint, fingerprint_from_b64};
use rlord_relay_proto::{Identity, RelayAddress};

const USAGE: &str = "usage: rlord-device pair <RLORD1:…> --state FILE [--name NAME]
       rlord-device check|snapshot --state FILE
       rlord-device ask <text> --state FILE";

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

fn save(path: &PathBuf, d: &Device) -> Result<()> {
    let h = d.host.as_ref();
    let v = json!({
        "name": d.name,
        "static": b64(d.static_key.private()),
        "relay_key": b64(&d.relay_key.to_bytes()),
        "host": h.map(|h| json!({
            "static": b64(&h.static_key),
            "pin": h.pin.to_string(),
            "fp": b64_fingerprint(&h.fp),
            "name": h.name,
        })),
    });
    std::fs::write(path, serde_json::to_vec_pretty(&v)?)?;
    Ok(())
}

fn load(path: &PathBuf) -> Result<Device> {
    let v: Value = serde_json::from_slice(&std::fs::read(path).context("reading the state")?)?;
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let static_key = StaticKey::from_private(
        unb64(&s("static"))?
            .try_into()
            .map_err(|_| anyhow::anyhow!("bad static key"))?,
    );
    let relay_key =
        Identity::from_bytes(&unb64(&s("relay_key"))?).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut d = Device::new(&s("name"))?;
    d.static_key = static_key;
    d.relay_key = relay_key;
    let h = &v["host"];
    if h.is_object() {
        let hs = |k: &str| h[k].as_str().unwrap_or("").to_string();
        d.host = Some(Host {
            static_key: unb64(&hs("static"))?
                .try_into()
                .map_err(|_| anyhow::anyhow!("bad host key"))?,
            pin: RelayAddress::parse(&hs("pin")).map_err(|e| anyhow::anyhow!("{e}"))?,
            fp: fingerprint_from_b64(&hs("fp")).map_err(|e| anyhow::anyhow!("{e}"))?,
            name: hs("name"),
        });
    }
    Ok(d)
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
    match args.first().map(String::as_str) {
        Some("pair") => {
            let qr = args.get(1).context(USAGE)?;
            let name = flag(&args, "--name").unwrap_or_else(|| "rlord-device".into());
            let mut d = Device::new(&name)?;
            let run = d.start_pairing(qr).await?;
            println!("code {} (the host shows {})", run.code, run.host_code);
            println!("waiting for the host to confirm…");
            let host = run.outcome(&mut d, Duration::from_secs(130)).await?;
            save(&state, &d)?;
            println!("paired with {} (device {})", host.name, d.id());
        }
        Some("check") => {
            let d = load(&state)?;
            let c = d.connect().await?;
            println!(
                "boot {} host {:?} signal {:?} queued {:?}",
                c.ready.boot, c.ready.host, c.ready.signal, c.ready.queued
            );
        }
        Some("snapshot") => {
            let d = load(&state)?;
            let mut c = d.connect().await?;
            let mut live = d.live(&mut c).await?;
            let a = live.request(&mut c, "snapshot", json!({})).await?;
            println!("{}", serde_json::to_string_pretty(&a)?);
        }
        Some("ask") => {
            let text = args.get(1).context(USAGE)?;
            let d = load(&state)?;
            let mut c = d.connect().await?;
            let mut live = d.live(&mut c).await?;
            let a = live
                .request(&mut c, "overlord.ask", json!({"text": text}))
                .await?;
            println!("{a}");
        }
        _ => bail!(USAGE),
    }
    Ok(())
}
