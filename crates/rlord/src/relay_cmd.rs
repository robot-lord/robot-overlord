//! `rlord relay …`: remote access through one relay, by asking the daemon
//! (`relay.*`, crates/daemon/src/relay.rs), which asks the `rlord-web`
//! plugin. This binary holds no keys and makes no connection: it draws the
//! pairing QR code from the plugin's string (`rlord_core::qr`) and asks
//! the user.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use rlord_core::plugin;
use rlord_core::qr::{Qr, ANSI_OFF, ANSI_ON};
use rlord_core::timefmt::{fmt_ts, rel_age};

#[derive(Subcommand)]
pub enum RelayCmd {
    /// Use this relay (the address `rlord-relay address` prints): makes
    /// this machine's keys for it and turns remote access on
    Setup {
        /// `https://relay.example.com#k=<key fingerprint>`
        address: String,
        /// A registration token from the relay's owner (`rlord-relay token
        /// new`), for this machine's first connection
        #[arg(long, value_name = "T")]
        token: Option<String>,
        /// Move to another relay: deletes this machine's keys and every
        /// paired device first (each pairs again)
        #[arg(long)]
        reset: bool,
        /// With --reset: don't ask
        #[arg(long)]
        yes: bool,
    },
    /// Pair a device: shows a QR code to scan (for 5 minutes, one device),
    /// then asks you to confirm the device by its code
    Pair {
        /// Print the pairing string instead of drawing the code
        #[arg(long)]
        text: bool,
    },
    /// Confirm the device waiting to pair (check its code first)
    Confirm,
    /// Turn away the device waiting to pair
    Reject,
    /// The paired devices
    Devices,
    /// Unpair a device (by its id, or its name)
    Revoke { device: String },
    /// Remote access: on or off, the relay, the connection, the devices
    Status,
    /// Turn remote access off (closes the relay connection; devices stay
    /// paired, and `setup` turns it back on)
    Disable,
}

/// `rlord relay <cmd>`, with `call` (the CLI's daemon call).
pub fn run(cmd: RelayCmd, call: fn(&str, Value) -> Result<Value>) -> Result<()> {
    need_plugin()?;
    match cmd {
        RelayCmd::Setup {
            address,
            token,
            reset,
            yes,
        } => {
            if reset
                && !yes
                && !ask_yes(
                    "--reset deletes this machine's remote-access keys and unpairs every \
                     device. Go on? [y/N] ",
                    "--reset unpairs every device: add --yes to go on",
                )?
            {
                println!("nothing changed");
                return Ok(());
            }
            let mut p = json!({"address": address, "reset": reset});
            if let Some(t) = token {
                p["token"] = json!(t);
            }
            let r = call("relay.setup", p)?;
            println!(
                "remote access is on, through {} (key {})",
                r["relay"].as_str().unwrap_or("?"),
                short_key(r["relay_key"].as_str().unwrap_or(""))
            );
            println!("pair a device: rlord relay pair");
            Ok(())
        }
        RelayCmd::Pair { text } => pair(text, call),
        RelayCmd::Confirm => confirm(call),
        RelayCmd::Reject => {
            call("relay.reject", json!({}))?;
            println!("turned the device away");
            Ok(())
        }
        RelayCmd::Devices => {
            let r = call("relay.devices", json!({}))?;
            print!("{}", devices_table(&r["devices"]));
            Ok(())
        }
        RelayCmd::Revoke { device } => {
            let r = call("relay.devices", json!({}))?;
            let id = pick_device(&r["devices"], &device)?;
            call("relay.revoke", json!({"device": id}))?;
            println!("unpaired {id}");
            Ok(())
        }
        RelayCmd::Status => {
            print!("{}", status_text(&call("relay.status", json!({}))?));
            Ok(())
        }
        RelayCmd::Disable => {
            call("relay.disable", json!({}))?;
            println!(
                "remote access is off; devices stay paired \
                 (`rlord relay setup <address>` turns it back on)"
            );
            Ok(())
        }
    }
}

/// Remote access lives in the web plugin: without it, say how to get it
/// (before asking the daemon anything).
fn need_plugin() -> Result<()> {
    match plugin::locate_with(plugin::install_hash().as_deref()) {
        Ok(_) => Ok(()),
        Err(e) => bail!("remote access needs the web plugin: {e}"),
    }
}

/// Ask on a terminal; without one, fail with `no_terminal`.
fn ask_yes(question: &str, no_terminal: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("{no_terminal}");
    }
    print!("{question}");
    std::io::stdout().flush()?;
    let mut a = String::new();
    std::io::stdin().read_line(&mut a)?;
    Ok(matches!(
        a.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// How often `pair` asks whether a device has come.
const POLL: Duration = Duration::from_millis(500);

/// `rlord relay pair`: the offer, then the device's name and code, then
/// the user's answer.
fn pair(text: bool, call: fn(&str, Value) -> Result<Value>) -> Result<()> {
    let offer = call("relay.pair", json!({}))?;
    let qr = offer["qr"].as_str().context("no pairing offer came back")?;
    let expires = offer["expires"].as_i64();
    if text {
        println!("{qr}");
    } else {
        let colour = std::io::stdout().is_terminal();
        for line in Qr::new(qr)?.half_blocks(2) {
            if colour {
                println!("{ANSI_ON}{line}{ANSI_OFF}");
            } else {
                println!("{line}");
            }
        }
        println!("scan this on the device you are pairing (one device, for 5 minutes)");
    }
    eprintln!("waiting for the device… (Ctrl-C stops waiting; the offer runs out by itself)");
    // Until the offer's end (and a little), whatever the plugin says.
    let deadline = Instant::now()
        + expires
            .map(|e| e - rlord_core::types::now_ms() / 1000)
            .map_or(Duration::from_secs(300), |s| {
                Duration::from_secs(s.clamp(0, 600) as u64)
            })
        + Duration::from_secs(5);
    loop {
        let st = call("relay.status", json!({}))?;
        if st["pending"].is_object() {
            return answer(&st["pending"], call);
        }
        if Instant::now() >= deadline {
            bail!("no device came before the offer ran out: `rlord relay pair` makes another");
        }
        std::thread::sleep(POLL);
    }
}

/// The device waiting: show it, ask, and confirm or reject it. Without a
/// terminal it is left waiting, with the commands that answer it.
fn answer(pending: &Value, call: fn(&str, Value) -> Result<Value>) -> Result<()> {
    println!("{}", pending_text(pending));
    if !std::io::stdin().is_terminal() {
        println!("answer with `rlord relay confirm` or `rlord relay reject`");
        return Ok(());
    }
    if ask_yes("Pair it? [y/N] ", "")? {
        let r = call("relay.confirm", json!({}))?;
        println!(
            "paired: {} ({})",
            r["device"]["name"].as_str().unwrap_or("?"),
            r["device"]["id"].as_str().unwrap_or("?")
        );
    } else {
        call("relay.reject", json!({}))?;
        println!("turned the device away");
    }
    Ok(())
}

/// `rlord relay confirm`: the waiting device, shown, then (on a terminal,
/// after a yes) paired.
fn confirm(call: fn(&str, Value) -> Result<Value>) -> Result<()> {
    let st = call("relay.status", json!({}))?;
    if !st["pending"].is_object() {
        bail!("no device is waiting to pair (`rlord relay pair` starts a pairing)");
    }
    if std::io::stdin().is_terminal() {
        return answer(&st["pending"], call);
    }
    println!("{}", pending_text(&st["pending"]));
    let r = call("relay.confirm", json!({}))?;
    println!(
        "paired: {} ({})",
        r["device"]["name"].as_str().unwrap_or("?"),
        r["device"]["id"].as_str().unwrap_or("?")
    );
    Ok(())
}

/// "device "phone" wants to pair, code 123 456".
fn pending_text(p: &Value) -> String {
    format!(
        "device \"{}\" wants to pair. Its code: {} (the device shows the same one: check it)",
        p["name"].as_str().unwrap_or("?"),
        spaced_code(p["code"].as_str().unwrap_or("?"))
    )
}

/// A six-digit code as two groups of three.
fn spaced_code(c: &str) -> String {
    if c.len() == 6 && c.is_ascii() {
        format!("{} {}", &c[..3], &c[3..])
    } else {
        c.to_string()
    }
}

/// A key fingerprint, shortened for people.
fn short_key(k: &str) -> String {
    if k.chars().count() > 12 {
        format!("{}…", k.chars().take(12).collect::<String>())
    } else {
        k.to_string()
    }
}

/// Unix seconds as milliseconds, for `timefmt`.
fn ms(v: &Value) -> Option<i64> {
    v.as_i64().map(|s| s * 1000)
}

/// `rlord relay devices`: name, id, paired, last seen, linked now.
fn devices_table(devices: &Value) -> String {
    let list = devices.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "no paired devices (`rlord relay pair` adds one)\n".into();
    }
    let mut out = format!(
        "{:<24} {:<18} {:<21} {:<10} {}\n",
        "NAME", "ID", "PAIRED", "LAST SEEN", "LINKED"
    );
    for d in &list {
        out.push_str(&format!(
            "{:<24} {:<18} {:<21} {:<10} {}\n",
            d["name"].as_str().unwrap_or(""),
            d["id"].as_str().unwrap_or(""),
            ms(&d["paired_at"]).map_or("-".into(), fmt_ts),
            match ms(&d["last_seen"]) {
                Some(t) => format!("{} ago", rel_age(Some(t))),
                None => "never".into(),
            },
            if d["linked"] == true { "yes" } else { "no" },
        ));
    }
    out
}

/// The device `given` names: an id, the start of one, or a name (each
/// matching one device only).
fn pick_device(devices: &Value, given: &str) -> Result<String> {
    let given = given.trim();
    let list = devices.as_array().cloned().unwrap_or_default();
    let id = |d: &Value| d["id"].as_str().unwrap_or("").to_string();
    let exact: Vec<String> = list
        .iter()
        .filter(|d| d["id"] == given || d["name"] == given)
        .map(id)
        .collect();
    let found: Vec<String> = if exact.is_empty() && !given.is_empty() {
        list.iter()
            .filter(|d| d["id"].as_str().is_some_and(|i| i.starts_with(given)))
            .map(id)
            .collect()
    } else {
        exact
    };
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("no paired device {given:?} (`rlord relay devices` lists them)"),
        _ => bail!("more than one device matches {given:?}: give its id"),
    }
}

/// `rlord relay status`.
fn status_text(st: &Value) -> String {
    let s = |k: &str| st[k].as_str().unwrap_or("").to_string();
    let mut out = String::new();
    let on = st["enabled"] == true;
    out.push_str(&format!(
        "remote access: {}\n",
        if on { "on" } else { "off" }
    ));
    if s("relay").is_empty() {
        out.push_str("relay:   none (`rlord relay setup <relay address>`)\n");
    } else {
        out.push_str(&format!(
            "relay:   {} (key {})\n",
            s("relay"),
            short_key(&s("relay_key"))
        ));
    }
    let p = &st["plugin"];
    let plugin = match p["state"].as_str().unwrap_or("") {
        _ if p["installed"] != true => format!(
            "not usable: {}",
            p["problem"].as_str().unwrap_or("not installed")
        ),
        "running" => "running".to_string(),
        other => format!("{other} (`rlord web status`)"),
    };
    out.push_str(&format!("plugin:  {plugin}\n"));
    if st["reported"] != true {
        if on {
            out.push_str("link:    unknown (the plugin hasn't reported)\n");
        }
        return out;
    }
    let link = if st["connected"] == true {
        "connected".to_string()
    } else {
        match st["error"].as_str() {
            Some(e) => format!("not connected: {e}"),
            None => "not connected".to_string(),
        }
    };
    out.push_str(&format!("link:    {link}\n"));
    let devices = st["devices"].as_array().cloned().unwrap_or_default();
    let linked = devices.iter().filter(|d| d["linked"] == true).count();
    out.push_str(&format!(
        "devices: {} paired, {linked} linked now (`rlord relay devices`)\n",
        devices.len()
    ));
    if let Some(e) = ms(&st["pairing"]["expires"]) {
        out.push_str(&format!("pairing: open until {}\n", fmt_ts(e)));
    }
    if st["pending"].is_object() {
        out.push_str(&format!(
            "waiting: {}\n         `rlord relay confirm` or `rlord relay reject`\n",
            pending_text(&st["pending"])
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devices() -> Value {
        json!([
            {"id": "ab12cd34ef560789", "name": "phone", "paired_at": 1_800_000_000,
             "last_seen": null, "linked": true},
            {"id": "ab99000011112222", "name": "tablet", "paired_at": 1_800_000_000,
             "last_seen": 1_800_000_100, "linked": false},
        ])
    }

    #[test]
    fn a_device_is_named_by_id_prefix_or_name() {
        let d = devices();
        assert_eq!(pick_device(&d, "phone").unwrap(), "ab12cd34ef560789");
        assert_eq!(pick_device(&d, "ab99").unwrap(), "ab99000011112222");
        assert_eq!(
            pick_device(&d, "ab12cd34ef560789").unwrap(),
            "ab12cd34ef560789"
        );
        assert!(pick_device(&d, "ab")
            .unwrap_err()
            .to_string()
            .contains("more than one"));
        assert!(pick_device(&d, "laptop")
            .unwrap_err()
            .to_string()
            .contains("no paired device"));
        assert!(pick_device(&d, "").is_err());
    }

    #[test]
    fn devices_and_status_read_plainly() {
        let t = devices_table(&devices());
        assert!(
            t.contains("phone") && t.contains("never") && t.contains("yes"),
            "{t}"
        );
        assert!(devices_table(&json!([])).contains("rlord relay pair"));
        let st = json!({
            "enabled": true, "relay": "https://relay.example.com",
            "relay_key": "q83vEjRWeJq83vEjRWeJq83vEjRWeJq83vEjRWeJq80",
            "plugin": {"installed": true, "state": "running", "problem": null},
            "reported": true, "connected": false, "error": "wrong relay",
            "devices": devices(), "pairing": null,
            "pending": {"name": "laptop", "code": "123456", "expires": 1},
        });
        let t = status_text(&st);
        assert!(t.contains("remote access: on"), "{t}");
        assert!(t.contains("key q83vEjRWeJq8…"), "{t}");
        assert!(t.contains("not connected: wrong relay"), "{t}");
        assert!(t.contains("2 paired, 1 linked now"), "{t}");
        assert!(t.contains("\"laptop\"") && t.contains("123 456"), "{t}");
        let off = status_text(&json!({
            "enabled": false, "relay": "", "relay_key": "",
            "plugin": {"installed": false, "state": "stopped",
                       "problem": "the web plugin is not installed (run `rlord web install`)"},
            "reported": false,
        }));
        assert!(
            off.contains("remote access: off") && off.contains("rlord web install"),
            "{off}"
        );
    }
}
