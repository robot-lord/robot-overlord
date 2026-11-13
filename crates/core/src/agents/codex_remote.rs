//! Codex Remote Control, as rlord sees it: continuing Codex threads on this
//! machine from the ChatGPT app.
//!
//! Unlike Claude's, it is machine-wide: a switch on Codex's shared
//! background server (`codex_server`) plus the phones paired with it. A
//! thread is reachable from the phone while it runs on that server, so a
//! session has it when the machine is connected and paired and the session
//! runs in daemon mode (`codex::on_daemon`).
//!
//! Everything goes through the server's control socket, with the
//! `experimentalApi` capability (checked against a 0.169.0 server):
//!
//! - `remoteControl/status/read` → `{status: disabled | connecting |
//!   connected | errored, serverName, installationId, environmentId}`. The
//!   same object comes as `remoteControl/status/changed` right after
//!   `initialize`.
//! - `remoteControl/enable` / `disable` switch it without restarting the
//!   server (running turns go on) and persist the switch
//!   (`app-server-daemon/settings.json`). Enabling enrolls the machine with
//!   ChatGPT first and fails at once when that fails (an account without
//!   multi-factor authentication is refused). The CLI's
//!   `codex remote-control start` and `daemon enable-remote-control` /
//!   `disable-remote-control` restart the server instead, cutting off any
//!   turn on it: rlord never runs them.
//! - `remoteControl/pairing/start` makes a short-lived code the user enters
//!   in the ChatGPT app; `remoteControl/pairing/status` (by that code) says
//!   when it was claimed; `remoteControl/client/list {environmentId}` lists
//!   the paired devices.
//!
//! The pairing code is a credential for a moment: it is kept in `Secret`,
//! which never prints, and goes only to the user's own terminal.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::codex_server::Client;
use super::remote::Availability;

/// Each step of a call waits at most this (enabling enrolls with ChatGPT,
/// so it gets longer).
const LIMIT: Duration = Duration::from_secs(5);
const ENROLL_LIMIT: Duration = Duration::from_secs(20);

/// The machine's link to ChatGPT, as the server says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// Switched off.
    Disabled,
    Connecting,
    Connected,
    /// Switched on, but not connected (enrollment refused, offline).
    Errored,
}

impl Link {
    pub fn parse(s: &str) -> Option<Link> {
        Some(match s {
            "disabled" => Link::Disabled,
            "connecting" => Link::Connecting,
            "connected" => Link::Connected,
            "errored" => Link::Errored,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Link::Disabled => "disabled",
            Link::Connecting => "connecting",
            Link::Connected => "connected",
            Link::Errored => "errored",
        }
    }

    /// The switch is on (whether or not it connected).
    pub fn enabled(self) -> bool {
        self != Link::Disabled
    }
}

/// The machine's Remote Control state.
#[derive(Clone, PartialEq, Eq)]
pub struct Machine {
    pub link: Link,
    /// The name the phone shows for this machine (its host name).
    pub server_name: Option<String>,
    /// Which ChatGPT environment it is enrolled in: needed to list paired
    /// devices; never shown (it identifies the account).
    environment: Option<String>,
}

impl std::fmt::Debug for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Machine")
            .field("link", &self.link)
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

impl Machine {
    /// From a `remoteControl/status/read` result (or the notification's
    /// params).
    pub fn parse(v: &Value) -> Option<Machine> {
        let s = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        Some(Machine {
            link: Link::parse(v["status"].as_str()?)?,
            server_name: s("serverName"),
            environment: s("environmentId"),
        })
    }

    pub fn environment(&self) -> Option<&str> {
        self.environment.as_deref()
    }
}

/// A pairing code: never printed (`Debug` hides it), only `reveal`ed to
/// the user's own terminal.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<pairing code>")
    }
}

/// A pairing in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    /// The code the user types into the ChatGPT app (the manual one when
    /// the server gives both).
    pub code: Secret,
    /// The other form (for a QR code), when given.
    pub other: Option<Secret>,
    /// When it stops working (ms since the epoch; None: not said).
    pub expires_at: Option<i64>,
}

impl Pairing {
    /// From a `remoteControl/pairing/start` result.
    pub fn parse(v: &Value) -> Option<Pairing> {
        let s = |k: &str| {
            v[k].as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Secret(s.to_string()))
        };
        let manual = s("manualPairingCode");
        let full = s("pairingCode");
        let (code, other) = match (manual, full) {
            (Some(m), f) => (m, f),
            (None, Some(f)) => (f, None),
            (None, None) => return None,
        };
        Some(Pairing {
            code,
            other,
            expires_at: time_ms(&v["expiresAt"]),
        })
    }
}

/// A time given as ms or seconds since the epoch, or as RFC 3339 text.
fn time_ms(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        // Seconds until the year 2286; ms after.
        return Some(if n < 10_000_000_000 { n * 1000 } else { n });
    }
    let t = v.as_str()?;
    super::transcript::parse_iso_ms(t)
}

fn client(socket: &Path, limit: Duration) -> Result<Client> {
    Client::connect_with(socket, limit, true)
}

/// The machine's state. An error: the server couldn't be asked (not
/// running, or too old to have Remote Control).
pub fn status(socket: &Path) -> Result<Machine> {
    let r = client(socket, LIMIT)?.request("remoteControl/status/read", json!({}), LIMIT)?;
    Machine::parse(&r).context("Codex's server gave a Remote Control status rlord can't read")
}

/// Switch it on (no server restart). Fails with the reason, in words the
/// user can act on, when ChatGPT refuses this machine.
pub fn enable(socket: &Path) -> Result<Machine> {
    let mut c = client(socket, LIMIT)?;
    match c.request("remoteControl/enable", json!({}), ENROLL_LIMIT) {
        Ok(r) => match Machine::parse(&r) {
            Some(m) => Ok(m),
            None => status(socket),
        },
        Err(e) => bail!("{}", explain(&format!("{e:#}"))),
    }
}

/// Switch it off (no server restart; paired phones stay paired).
pub fn disable(socket: &Path) -> Result<Machine> {
    let r = client(socket, LIMIT)?.request("remoteControl/disable", json!({}), LIMIT)?;
    match Machine::parse(&r) {
        Some(m) => Ok(m),
        None => status(socket),
    }
}

/// How many devices are paired with this machine. None: it can't be told
/// (not connected, or the server couldn't ask ChatGPT).
pub fn paired(socket: &Path, m: &Machine) -> Option<usize> {
    let env = m.environment()?;
    let r = client(socket, LIMIT)
        .ok()?
        .request(
            "remoteControl/client/list",
            json!({"environmentId": env}),
            LIMIT,
        )
        .ok()?;
    count_clients(&r)
}

/// The number of devices in a `remoteControl/client/list` result.
pub fn count_clients(r: &Value) -> Option<usize> {
    if let Some(a) = r.as_array() {
        return Some(a.len());
    }
    ["clients", "data", "items"]
        .into_iter()
        .find_map(|k| r[k].as_array())
        .map(Vec::len)
}

/// Start pairing a phone: a code for the user to enter in the ChatGPT app.
pub fn pair(socket: &Path) -> Result<Pairing> {
    let mut c = client(socket, LIMIT)?;
    let r = match c.request("remoteControl/pairing/start", json!({}), ENROLL_LIMIT) {
        Ok(r) => r,
        Err(e) => bail!("{}", explain(&format!("{e:#}"))),
    };
    Pairing::parse(&r).context("Codex's server gave no pairing code")
}

/// Whether the pairing's code was used (a phone paired). None: it can't be
/// told.
pub fn claimed(socket: &Path, p: &Pairing) -> Option<bool> {
    let params = json!({"manualPairingCode": p.code.reveal()});
    let r = client(socket, LIMIT)
        .ok()?
        .request("remoteControl/pairing/status", params, LIMIT)
        .ok()?;
    parse_claimed(&r)
}

/// From a `remoteControl/pairing/status` result.
pub fn parse_claimed(r: &Value) -> Option<bool> {
    if let Some(b) = r["claimed"].as_bool() {
        return Some(b);
    }
    r["status"].as_str().map(|s| s == "claimed")
}

/// A server error in a line the user can act on: what ChatGPT said, without
/// the request's URL and ids.
pub fn explain(msg: &str) -> String {
    if msg.contains("Multi-factor authentication required") {
        return "ChatGPT needs multi-factor authentication on your account for Remote Control: turn it on in ChatGPT (Settings → Security), then try again".into();
    }
    if msg.contains("requires remote control to be enabled") {
        return "Remote Control is off on this machine".into();
    }
    if msg.contains("requires experimentalApi") || msg.contains("unknown method") {
        return "this Codex has no Remote Control (update Codex)".into();
    }
    // The detail ChatGPT gave, if any: `body: {"detail":"..."}`.
    if let Some(d) = msg
        .split_once("\"detail\":\"")
        .and_then(|(_, r)| r.split_once('"'))
        .map(|(d, _)| d)
    {
        return format!("ChatGPT refused this machine: {d}");
    }
    // Else up to the first URL or id.
    let cut = ["`http", " http", ", request-id", " request-id"]
        .iter()
        .filter_map(|m| msg.find(m))
        .min()
        .unwrap_or(msg.len());
    let s = msg[..cut].trim().trim_end_matches([':', ',']).trim();
    let s: String = s.chars().take(200).collect();
    if s.is_empty() {
        "Codex couldn't reach ChatGPT".into()
    } else {
        s
    }
}

/// What `codex login status` says, as a reason Remote Control can't work
/// (None: logged in with ChatGPT). It prints one line, on stderr.
pub fn login_problem(text: &str) -> Option<String> {
    let t = text.trim();
    if t.contains("using ChatGPT") {
        return None;
    }
    Some(if t.contains("API key") {
        "Codex uses an API key; Remote Control needs a ChatGPT login (run `codex login`)".into()
    } else {
        "Codex isn't logged in to ChatGPT (run `codex login`)".into()
    })
}

/// Whether this machine can use Codex Remote Control: Codex installed and
/// logged in with ChatGPT, with a background server. (The account itself,
/// for example one without multi-factor authentication, shows only when it
/// is switched on.) Runs `codex login status` and `daemon version`, a
/// second or two.
pub fn check(program: Option<&Path>) -> Availability {
    let no = |r: String, transient: bool| Availability {
        available: false,
        reason: Some(r),
        note: None,
        transient,
    };
    let Some(program) = program else {
        return no("Codex isn't installed".into(), false);
    };
    match login_status(program) {
        Ok(text) => {
            if let Some(why) = login_problem(&text) {
                return no(why, false);
            }
        }
        Err(e) => return no(e, true),
    }
    if super::codex_server::version(program).is_none() {
        return no(
            "this Codex has no shared background server (update Codex)".into(),
            false,
        );
    }
    Availability {
        available: true,
        note: Some(
            "machine-wide: a paired phone reaches every Codex thread on Codex's background server"
                .into(),
        ),
        ..Default::default()
    }
}

/// `codex login status` (its stderr and stdout), bounded.
fn login_status(program: &Path) -> std::result::Result<String, String> {
    use std::io::Read;
    let mut child = std::process::Command::new(program)
        .args(["login", "status"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run codex: {e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("`codex login status` didn't answer".into());
            }
        }
    }
    let mut out = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut out);
    }
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from a 0.169.0 server on macOS (ids replaced).
    const STATUS_DISABLED: &str = r#"{"status":"disabled","serverName":"my-mac","installationId":"00000000-0000-0000-0000-000000000000","environmentId":null}"#;
    const STATUS_ERRORED: &str = r#"{"status":"errored","serverName":"my-mac","installationId":"00000000-0000-0000-0000-000000000000","environmentId":null}"#;
    const ENABLE_MFA: &str = "remoteControl/enable: remote control server enrollment failed at `https://chatgpt.com/backend-api/wham/remote/control/server/enroll`: HTTP 403 Forbidden, request-id: 00000000-0000-0000-0000-000000000000, cf-ray: 0000000000000000-LAX, body: {\"detail\":\"Multi-factor authentication required\"}";
    const PAIR_DISABLED: &str =
        "remoteControl/pairing/start: remote control pairing requires remote control to be enabled";
    const STATUS_NEEDS_CAP: &str =
        "remoteControl/status/read: remoteControl/status/read requires experimentalApi capability";

    #[test]
    fn machine_status() {
        let m = Machine::parse(&serde_json::from_str(STATUS_DISABLED).unwrap()).unwrap();
        assert_eq!(m.link, Link::Disabled);
        assert!(!m.link.enabled());
        assert_eq!(m.server_name.as_deref(), Some("my-mac"));
        assert_eq!(m.environment(), None);
        let e = Machine::parse(&serde_json::from_str(STATUS_ERRORED).unwrap()).unwrap();
        assert!(e.link.enabled());
        let c = Machine::parse(
            &json!({"status": "connected", "serverName": "m", "environmentId": "env-1"}),
        )
        .unwrap();
        assert_eq!((c.link, c.environment()), (Link::Connected, Some("env-1")));
        // The environment id is never printed.
        assert!(!format!("{c:?}").contains("env-1"));
        assert_eq!(Machine::parse(&json!({"status": "later"})), None);
    }

    #[test]
    fn errors_in_plain_words() {
        let mfa = explain(ENABLE_MFA);
        assert!(mfa.contains("multi-factor"), "{mfa}");
        assert!(!mfa.contains("request-id") && !mfa.contains("http"));
        assert_eq!(
            explain(PAIR_DISABLED),
            "Remote Control is off on this machine"
        );
        assert!(explain(STATUS_NEEDS_CAP).contains("update Codex"));
        let other = explain("x: enrollment failed at `https://a/b`: HTTP 500, request-id: 1, body: {\"detail\":\"Try later\"}");
        assert_eq!(other, "ChatGPT refused this machine: Try later");
        let bare = explain("x: failed at `https://a/b`: HTTP 500, request-id: 1");
        assert_eq!(bare, "x: failed at");
    }

    #[test]
    fn pairing_codes_never_print() {
        let p = Pairing::parse(
            &json!({"pairingCode": "LONG-CODE", "manualPairingCode": "ABCD-1234",
                                       "environmentId": "env", "expiresAt": 1_800_000_000}),
        )
        .unwrap();
        assert_eq!(p.code.reveal(), "ABCD-1234");
        assert_eq!(p.other.as_ref().unwrap().reveal(), "LONG-CODE");
        assert_eq!(p.expires_at, Some(1_800_000_000_000));
        let shown = format!("{p:?}");
        assert!(
            !shown.contains("ABCD") && !shown.contains("LONG"),
            "{shown}"
        );
        let only =
            Pairing::parse(&json!({"pairingCode": "X1", "expiresAt": "2026-11-13T19:00:00Z"}))
                .unwrap();
        assert_eq!(only.code.reveal(), "X1");
        assert!(only.expires_at.is_some());
        assert_eq!(Pairing::parse(&json!({"expiresAt": 1})), None);
        assert_eq!(parse_claimed(&json!({"claimed": true})), Some(true));
        assert_eq!(parse_claimed(&json!({"status": "pending"})), Some(false));
        assert_eq!(parse_claimed(&json!({})), None);
        assert_eq!(count_clients(&json!({"clients": [{}, {}]})), Some(2));
        assert_eq!(count_clients(&json!({"data": []})), Some(0));
        assert_eq!(count_clients(&json!([{}])), Some(1));
        assert_eq!(count_clients(&json!({})), None);
    }

    #[test]
    fn login() {
        assert_eq!(login_problem("Logged in using ChatGPT\n"), None);
        assert!(login_problem("Logged in using an API key - sk-...")
            .unwrap()
            .contains("API key"));
        assert!(login_problem("Not logged in")
            .unwrap()
            .contains("codex login"));
    }

    /// Against a stand-in control socket: the calls go with the
    /// experimental capability, and switching works.
    #[test]
    fn switch_and_pair_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("c.sock");
        let fake = super::super::codex_server::fake::Fake::start(&sock);
        assert_eq!(status(&sock).unwrap().link, Link::Disabled);
        assert!(fake.saw_experimental());
        fake.refuse_enable(ENABLE_MFA);
        let e = enable(&sock).unwrap_err().to_string();
        assert!(e.contains("multi-factor"), "{e}");
        assert_eq!(status(&sock).unwrap().link, Link::Disabled);
        fake.refuse_enable("");
        assert_eq!(enable(&sock).unwrap().link, Link::Connected);
        let m = status(&sock).unwrap();
        assert_eq!(paired(&sock, &m), Some(0));
        let p = pair(&sock).unwrap();
        assert_eq!(claimed(&sock, &p), Some(false));
        fake.claim();
        assert_eq!(claimed(&sock, &p), Some(true));
        assert_eq!(paired(&sock, &status(&sock).unwrap()), Some(1));
        assert_eq!(disable(&sock).unwrap().link, Link::Disabled);
    }
}
