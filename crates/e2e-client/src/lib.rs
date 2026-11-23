//! rlord's device engine: the device's whole side of remote access
//! (`docs/working/e2e-protocol.md`), natively, for the phone app and the
//! test device. The WebView page and `rlord-device` only exchange JSON
//! with it; it holds the keys (through the platform's [`Signers`] and
//! [`Vault`]), the one relay link, the Noise sessions, the mailbox, the
//! outbox and the reconnect loop.
//!
//! - [`machine`]: the sans-IO state machine. Every rule of the reconnect
//!   loop and the outbox lives there and is unit-tested with a scripted
//!   link and a fake clock.
//! - [`driver`]: tokio, the websocket and TLS, timers and storage, feeding
//!   the machine.
//! - [`Engine`]: the facade the platform calls (plain sync calls; the
//!   engine owns its own runtime). `crates/e2e-ffi` wraps it with UniFFI.
//!
//! The bridge messages (page to engine and back) are fixed in the slice 4
//! contract; `machine` documents each where it handles it.

pub mod driver;
mod engine;
pub mod link;
pub mod machine;
pub mod outbox;
pub mod soft;
pub mod store;

use std::path::PathBuf;

pub use engine::Engine;

/// What a platform call reports when it fails (a keystore refused, a
/// vault couldn't open a file). One string: the engine only logs it and
/// says it to the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformError {
    pub message: String,
}

impl PlatformError {
    pub fn new(m: impl Into<String>) -> PlatformError {
        PlatformError { message: m.into() }
    }
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PlatformError {}

/// The relay identity keys (P-256), one per paired host, kept by the
/// platform (Secure Enclave, Android Keystore; software in tests and
/// `rlord-device`). The engine only ever names a key by its label.
pub trait Signers: Send + Sync {
    /// Make a new P-256 key under `label` (replacing any); its SEC1
    /// uncompressed public key (65 bytes).
    fn create(&self, label: String) -> Result<Vec<u8>, PlatformError>;
    /// The SEC1 uncompressed public key of the key under `label`.
    fn public_key(&self, label: String) -> Result<Vec<u8>, PlatformError>;
    /// ECDSA P-256 with SHA-256 over `msg`: 64 bytes `r || s`, or ASN.1
    /// DER (the engine converts; high S is fine, no normalising).
    fn sign(&self, label: String, msg: Vec<u8>) -> Result<Vec<u8>, PlatformError>;
    /// Forget the key under `label` (nothing when there is none).
    fn delete(&self, label: String);
    /// True when keys are hardware-backed (Secure Enclave, StrongBox, TEE).
    fn hardware(&self) -> bool;
}

/// Wraps the engine's files at rest with a key the platform holds.
pub trait Vault: Send + Sync {
    fn seal(&self, plain: Vec<u8>) -> Result<Vec<u8>, PlatformError>;
    fn open(&self, sealed: Vec<u8>) -> Result<Vec<u8>, PlatformError>;
}

/// Where the engine's output goes. Called from any engine thread.
pub trait Listener: Send + Sync {
    /// One JSON message for the page (a reply or an event).
    fn to_page(&self, json: String);
    /// Post a local notification: kind `"waiting"`, `count` = sessions
    /// newly waiting for input since the last one notified.
    fn notify(&self, kind: String, count: u32);
}

/// How the engine is set up.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// The app's support folder (excluded from backup): one sealed file
    /// per paired host.
    pub dir: PathBuf,
    /// An extra root CA (PEM) the relay's certificate may chain to. Tests
    /// and debug builds only; a release app passes None.
    pub ca_pem: Option<String>,
}

/// What [`Engine::background_check`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundOutcome {
    /// No host is paired.
    NotPaired,
    /// The cheap check found nothing waiting: no handshake was made.
    NothingNew,
    /// The host is offline; mailbox answers waiting at the relay (if any)
    /// were taken.
    HostOffline,
    /// A session was made and the state refreshed. `waiting`: sessions
    /// waiting for input; `notified`: how many of them were new (a
    /// notification was asked for when not 0).
    Refreshed { waiting: u32, notified: u32 },
    /// It could not finish within its budget or the link failed.
    Failed { reason: String },
}

/// The remote API levels this engine speaks (`hello.api`).
pub const API_MIN: u32 = 1;
pub const API_MAX: u32 = 1;

/// This engine's version (the workspace's).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
