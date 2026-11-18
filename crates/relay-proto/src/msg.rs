//! The relay link's control messages: JSON text frames of at most
//! [`MAX_CONTROL`] bytes, tagged by `"t"`. Binary data is in `frame.rs`.
//!
//! Connect: peer `hello`, relay `challenge`, peer `auth`, relay `ready` (or
//! `enroll` first, or a close code). After `ready`, numbered requests (`n`)
//! are answered `ok` or `err`.

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Error, Result};

/// The link protocol version (`hello.v`). Another is refused with
/// [`close::UNSUPPORTED`].
pub const VERSION: u32 = 1;
/// Largest control (text) frame.
pub const MAX_CONTROL: usize = 4096;

/// Bytes on the wire as base64url without padding.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct B64(pub Vec<u8>);

impl B64 {
    pub fn as_array<const N: usize>(&self) -> Option<[u8; N]> {
        self.0.as_slice().try_into().ok()
    }
}

impl std::fmt::Debug for B64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "B64({})", self)
    }
}

impl std::fmt::Display for B64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&self.0))
    }
}

impl From<&[u8]> for B64 {
    fn from(b: &[u8]) -> B64 {
        B64(b.to_vec())
    }
}

impl<const N: usize> From<[u8; N]> for B64 {
    fn from(b: [u8; N]) -> B64 {
        B64(b.to_vec())
    }
}

impl Serialize for B64 {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for B64 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<B64, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .map(B64)
            .map_err(serde::de::Error::custom)
    }
}

/// Who is connecting.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// An rlord host (`rlord-web`).
    Host,
    /// A paired device; names its host's fingerprint.
    Device,
    /// A device pairing now; names the rendezvous id.
    Pair,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Host => "host",
            Role::Device => "device",
            Role::Pair => "pair",
        }
    }
}

/// How a host enrols on its first connection. Only `token` exists in v1;
/// account login (`enroll` from the relay) is reserved.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Enroll {
    Token { token: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub v: u32,
    pub role: Role,
    /// The peer's relay identity (SEC1 uncompressed P-256).
    pub key: B64,
    /// 32 random bytes.
    pub nonce: B64,
    /// A device: its host's relay key fingerprint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<B64>,
    /// A pairing device: the rendezvous id (16 bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendezvous: Option<B64>,
    /// A host's first connection to a relay that needs enrolment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enroll: Option<Enroll>,
}

/// Peer to relay.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToRelay {
    Hello(Hello),
    Auth {
        sig: B64,
    },
    /// Host: the whole device allow-list (relay key fingerprints), on every
    /// connect and every change.
    Devices {
        n: u64,
        allow: Vec<B64>,
    },
    /// Host: open a pairing rendezvous (16-byte id, `ttl` seconds).
    Rendezvous {
        n: u64,
        id: B64,
        ttl: u64,
    },
    RendezvousClose {
        n: u64,
        id: B64,
    },
    /// Host: "something for you" for a device (sets its flag; no content).
    Signal {
        n: u64,
        device: B64,
    },
    /// Host: close a device's link.
    Drop {
        n: u64,
        link: u32,
    },
    /// Device: clears its signal flag after it has re-synced.
    AckSignal {
        n: u64,
    },
    /// Either: deletes queued mailbox messages up to `seq` (a host names
    /// the device whose queue to it it means).
    Ack {
        n: u64,
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<B64>,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    Online,
    Offline,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Device,
    Pair,
}

/// What this account's tier allows (reserved: a client says why something
/// is refused). Every field optional.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devices: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_per_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_panes: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Ready {
    /// Changes when the relay restarts (and forgets its flags and queues).
    pub boot: String,
    /// A device: whether its host is connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostState>,
    /// A device: 1 when its host signalled since its last `ack_signal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<u64>,
    /// A device: mailbox messages waiting for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
}

/// Relay to peer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FromRelay {
    Challenge {
        origin: String,
        key: B64,
        nonce: B64,
        sig: B64,
    },
    Ready(Ready),
    /// Reserved (account login): show the address and code, then wait for
    /// `ready`.
    Enroll {
        url: String,
        code: String,
        expires: u64,
    },
    /// The relay is about to close this connection: don't reconnect before
    /// `retry_after` seconds.
    Bye {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after: Option<u64>,
    },
    Ok {
        n: u64,
    },
    Err {
        n: u64,
        code: String,
    },
    /// Host: a device (or pairing device) is connected; frames for it carry
    /// `id`.
    Link {
        id: u32,
        /// The device's relay key fingerprint.
        device: B64,
        kind: LinkKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rendezvous: Option<B64>,
    },
    Unlink {
        id: u32,
    },
    /// Device: its host came or went.
    Host {
        state: HostState,
    },
    /// Device: its host signalled while it was connected.
    Signal,
    /// A lane-2 frame from this connection was queued with this `seq` (for
    /// a host, in the queue to `device`). Every lane-2 frame is answered
    /// `queued` or `refused`, in the order the frames were sent.
    Queued {
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<B64>,
    },
    /// A lane-2 frame from this connection was not queued (`code`: `full`,
    /// `no_queue`, `unknown`, …); nothing of it is kept. Resend it later
    /// (before any newer one) or give up.
    Refused {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<B64>,
    },
    /// A live frame had nowhere to go.
    Undeliverable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link: Option<u32>,
    },
}

impl ToRelay {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("serializable")
    }

    pub fn parse(s: &str) -> Result<ToRelay> {
        if s.len() > MAX_CONTROL {
            return Err(Error::Message("control message too large".into()));
        }
        serde_json::from_str(s).map_err(|e| Error::Message(e.to_string()))
    }
}

impl FromRelay {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("serializable")
    }

    pub fn parse(s: &str) -> Result<FromRelay> {
        if s.len() > MAX_CONTROL {
            return Err(Error::Message("control message too large".into()));
        }
        serde_json::from_str(s).map_err(|e| Error::Message(e.to_string()))
    }
}

/// `err` codes.
pub mod code {
    /// The queue is full (32 messages or 128 KiB); nothing older is dropped.
    pub const FULL: &str = "full";
    /// Queues are off on this relay (`mailbox_ttl = 0`).
    pub const NO_QUEUE: &str = "no_queue";
    /// Over a limit (devices per host, rendezvous per host, …).
    pub const LIMIT: &str = "limit";
    /// A malformed or out-of-place request.
    pub const BAD: &str = "bad";
    /// Not a device of this host, or no such link or rendezvous.
    pub const UNKNOWN: &str = "unknown";
    /// Sent too fast.
    pub const RATE: &str = "rate";
    /// The relay is overloaded (with `bye`).
    pub const OVERLOADED: &str = "overloaded";
    /// The account is suspended (with `bye`).
    pub const SUSPENDED: &str = "suspended";
    /// The connection was idle or a link stalled (with `bye`).
    pub const IDLE: &str = "idle";
}

/// Websocket close codes.
pub mod close {
    /// A signature didn't verify, or a bad hello.
    pub const BAD_SIGNATURE: u16 = 4401;
    /// Suspended account.
    pub const SUSPENDED: u16 = 4402;
    /// Not registered, not on the host's list, or no such rendezvous.
    pub const NOT_ALLOWED: u16 = 4403;
    /// Too slow to authenticate (5 s), or idle (60 s).
    pub const TOO_SLOW: u16 = 4408;
    /// Replaced by a newer connection of the same identity.
    pub const REPLACED: u16 = 4409;
    /// Another link protocol version.
    pub const UNSUPPORTED: u16 = 4426;
    /// Over a limit.
    pub const LIMIT: u16 = 4429;
    /// The relay is overloaded.
    pub const OVERLOADED: u16 = 4503;
    /// This connection's link ended under it (its host dropped it, or the
    /// link stalled in either direction): the end-to-end session on it is
    /// broken. Reconnect at once, with a fresh handshake; not final, no
    /// backoff the first time (a client that keeps getting it backs off).
    pub const RESET: u16 = 4205;

    /// Codes a client must not retry without a change (the user's action,
    /// a new config or a newer client).
    pub fn is_final(code: u16) -> bool {
        matches!(code, BAD_SIGNATURE | SUSPENDED | NOT_ALLOWED | UNSUPPORTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_have_their_wire_names() {
        let h = ToRelay::Hello(Hello {
            v: VERSION,
            role: Role::Device,
            key: B64(vec![4; 65]),
            nonce: B64(vec![1; 32]),
            host: Some(B64(vec![2; 32])),
            rendezvous: None,
            enroll: None,
        });
        let s = h.to_json();
        assert!(
            s.starts_with(r#"{"t":"hello","v":1,"role":"device""#),
            "{s}"
        );
        assert!(!s.contains("rendezvous"));
        assert_eq!(ToRelay::parse(&s).unwrap(), h);
        let e = ToRelay::Hello(Hello {
            v: VERSION,
            role: Role::Host,
            key: B64(vec![4; 65]),
            nonce: B64(vec![1; 32]),
            host: None,
            rendezvous: None,
            enroll: Some(Enroll::Token {
                token: "abc".into(),
            }),
        });
        assert!(e
            .to_json()
            .contains(r#""enroll":{"kind":"token","token":"abc"}"#));
        assert_eq!(
            ToRelay::RendezvousClose {
                n: 3,
                id: B64(vec![0; 16])
            }
            .to_json(),
            r#"{"t":"rendezvous_close","n":3,"id":"AAAAAAAAAAAAAAAAAAAAAA"}"#
        );
        assert_eq!(
            ToRelay::AckSignal { n: 1 }.to_json(),
            r#"{"t":"ack_signal","n":1}"#
        );
        let r = FromRelay::Ready(Ready {
            boot: "b1".into(),
            host: Some(HostState::Online),
            signal: Some(0),
            queued: Some(2),
            limits: None,
        });
        assert_eq!(
            r.to_json(),
            r#"{"t":"ready","boot":"b1","host":"online","signal":0,"queued":2}"#
        );
        assert_eq!(FromRelay::parse(&r.to_json()).unwrap(), r);
        assert_eq!(FromRelay::Signal.to_json(), r#"{"t":"signal"}"#);
        assert_eq!(
            FromRelay::Bye {
                code: code::OVERLOADED.into(),
                retry_after: Some(30)
            }
            .to_json(),
            r#"{"t":"bye","code":"overloaded","retry_after":30}"#
        );
        assert_eq!(
            FromRelay::Queued {
                seq: 4,
                device: None
            }
            .to_json(),
            r#"{"t":"queued","seq":4}"#
        );
        let q = FromRelay::Refused {
            code: code::FULL.into(),
            device: Some(B64(vec![1; 32])),
        };
        assert!(q.to_json().starts_with(r#"{"t":"refused","code":"full","device":""#));
        assert_eq!(FromRelay::parse(&q.to_json()).unwrap(), q);
        let l = FromRelay::Link {
            id: 7,
            device: B64(vec![1; 32]),
            kind: LinkKind::Pair,
            rendezvous: Some(B64(vec![2; 16])),
        };
        assert_eq!(FromRelay::parse(&l.to_json()).unwrap(), l);
    }

    #[test]
    fn rejects() {
        assert!(ToRelay::parse(r#"{"t":"nope"}"#).is_err());
        assert!(
            ToRelay::parse(&format!(r#"{{"t":"auth","sig":"{}"}}"#, "A".repeat(5000))).is_err()
        );
        assert!(ToRelay::parse(r#"{"t":"auth","sig":"***"}"#).is_err());
        // Unknown enrolment kinds are refused, not ignored.
        assert!(ToRelay::parse(
            r#"{"t":"hello","v":1,"role":"host","key":"","nonce":"","enroll":{"kind":"account"}}"#
        )
        .is_err());
        assert!(close::is_final(close::NOT_ALLOWED));
        assert!(!close::is_final(close::OVERLOADED));
        assert!(!close::is_final(close::REPLACED));
        assert!(!close::is_final(close::RESET));
    }
}
