//! The relay address: one string carrying the origin and the pin,
//! `https://relay.example.com#k=<base64url fingerprint>`. Printed by
//! `rlord-relay address`, pasted into `rlord relay setup`.

use base64::Engine as _;

use crate::{Error, Result};

/// A relay origin is at most this many bytes.
pub const MAX_ORIGIN: usize = 255;

/// The websocket path every peer opens.
pub const PATH: &str = "/v1";

/// Where a relay is and which key it must prove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayAddress {
    /// The normalised origin (`normalize_origin`).
    pub origin: String,
    /// SHA-256 of the relay's SEC1 uncompressed P-256 public key.
    pub fingerprint: [u8; 32],
}

impl RelayAddress {
    pub fn parse(s: &str) -> Result<RelayAddress> {
        let s = s.trim();
        let (origin, frag) = s
            .split_once('#')
            .ok_or(Error::Address("no #k=<key fingerprint> after the origin"))?;
        let k = frag
            .strip_prefix("k=")
            .ok_or(Error::Address("the fragment must be k=<key fingerprint>"))?;
        Ok(RelayAddress {
            origin: normalize_origin(origin)?,
            fingerprint: fingerprint_from_b64(k)?,
        })
    }

    /// The websocket URL for this origin: `wss://<host>/v1` (or `ws://`
    /// for a loopback `http` origin). The only URL a client ever dials.
    pub fn ws_url(&self) -> String {
        ws_url(&self.origin)
    }
}

impl std::fmt::Display for RelayAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}#k={}",
            self.origin,
            b64_fingerprint(&self.fingerprint)
        )
    }
}

pub fn b64_fingerprint(fp: &[u8; 32]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(fp)
}

pub fn fingerprint_from_b64(s: &str) -> Result<[u8; 32]> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim())
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or(Error::Address(
            "the key fingerprint is not 32 bytes of base64url",
        ))
}

/// `wss://host[:port]/v1` for `https://host[:port]`, `ws://…` for `http://`.
pub fn ws_url(origin: &str) -> String {
    match origin.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}{PATH}"),
        Some(("http", rest)) => format!("ws://{rest}{PATH}"),
        _ => format!("{origin}{PATH}"),
    }
}

/// Is `host` (as written in a URL) a loopback address?
fn is_loopback(host: &str) -> bool {
    host == "127.0.0.1" || host == "[::1]"
}

/// The one spelling of an origin peers compare byte for byte: scheme
/// `https` (or `http` for a loopback address only, for tests and a relay
/// on the same machine), lower-case host, no userinfo, path, query or
/// fragment, no default port, at most 255 bytes.
pub fn normalize_origin(s: &str) -> Result<String> {
    let s = s.trim();
    let (scheme, rest) = s
        .split_once("://")
        .ok_or(Error::Address("the origin needs https://"))?;
    let scheme = scheme.to_ascii_lowercase();
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.contains(['/', '?', '#', '@', ' ']) || rest.is_empty() {
        return Err(Error::Address(
            "the origin is a scheme and a host (and port), nothing else",
        ));
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.contains(']') => {
            let port: u16 = p
                .parse()
                .map_err(|_| Error::Address("the port is not a number"))?;
            (h.to_ascii_lowercase(), Some(port))
        }
        _ => (rest.to_ascii_lowercase(), None),
    };
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '[' | ']' | ':'))
    {
        return Err(Error::Address("the host has characters a host can't have"));
    }
    let default = match scheme.as_str() {
        "https" => 443,
        "http" if is_loopback(&host) => 80,
        "http" => {
            return Err(Error::Address(
                "plain http is only allowed for a loopback relay (127.0.0.1)",
            ))
        }
        _ => return Err(Error::Address("the origin needs https://")),
    };
    let origin = match port {
        Some(p) if p != default => format!("{scheme}://{host}:{p}"),
        _ => format!("{scheme}://{host}"),
    };
    if origin.len() > MAX_ORIGIN {
        return Err(Error::Address("the origin is longer than 255 bytes"));
    }
    Ok(origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_normalise() {
        let n = |s| normalize_origin(s).unwrap();
        assert_eq!(n("https://Relay.Example.com"), "https://relay.example.com");
        assert_eq!(n("HTTPS://relay.example.com/"), "https://relay.example.com");
        assert_eq!(
            n("https://relay.example.com:443"),
            "https://relay.example.com"
        );
        assert_eq!(
            n("https://relay.example.com:8443"),
            "https://relay.example.com:8443"
        );
        assert_eq!(n("http://127.0.0.1:4000"), "http://127.0.0.1:4000");
        assert_eq!(n("http://127.0.0.1:80"), "http://127.0.0.1");
        assert_eq!(n("https://[::1]:9"), "https://[::1]:9");
        for bad in [
            "relay.example.com",
            "http://relay.example.com",
            "ftp://relay.example.com",
            "https://relay.example.com/path",
            "https://relay.example.com?q",
            "https://user@relay.example.com",
            "https://",
            "https://relay.example.com:99999",
            "https://ex ample.com",
        ] {
            assert!(normalize_origin(bad).is_err(), "{bad}");
        }
        let long = format!("https://{}.com", "a".repeat(250));
        assert!(normalize_origin(&long).is_err());
    }

    #[test]
    fn addresses_round_trip() {
        let a = RelayAddress {
            origin: "https://relay.example.com".into(),
            fingerprint: [7; 32],
        };
        let s = a.to_string();
        assert!(s.starts_with("https://relay.example.com#k="));
        assert_eq!(RelayAddress::parse(&s).unwrap(), a);
        assert_eq!(
            RelayAddress::parse(&s.replace("relay.", "RELAY.")).unwrap(),
            a
        );
        assert_eq!(a.ws_url(), "wss://relay.example.com/v1");
        assert_eq!(ws_url("http://127.0.0.1:5"), "ws://127.0.0.1:5/v1");
        assert!(RelayAddress::parse("https://relay.example.com").is_err());
        assert!(RelayAddress::parse("https://relay.example.com#k=abc").is_err());
        assert!(RelayAddress::parse("https://relay.example.com#x=abc").is_err());
    }
}
