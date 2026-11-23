//! The UniFFI surface of the phone app's device engine. Small on purpose:
//! everything a page feature needs travels as JSON through
//! `Engine::page_message` and `Listener::to_page`, so a new page feature
//! needs no binding change. The platform implements three traits: the
//! relay identity's keys (`Signers`), wrapping at rest (`Vault`) and the
//! way back to the page (`Listener`).

use std::sync::Arc;

uniffi::setup_scaffolding!();

/// Every error across the boundary, both ways.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("{message}")]
    Failed { message: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for FfiError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        FfiError::Failed { message: e.reason }
    }
}

/// The relay identity's P-256 keys, one per paired host, kept by the
/// platform (Secure Enclave, Android Keystore).
#[uniffi::export(with_foreign)]
pub trait Signers: Send + Sync {
    /// Make a key under `label` (replacing any); its SEC1 uncompressed
    /// public key (65 bytes).
    fn create(&self, label: String) -> Result<Vec<u8>, FfiError>;
    fn public_key(&self, label: String) -> Result<Vec<u8>, FfiError>;
    /// ECDSA P-256 with SHA-256 over `msg`: 64 bytes `r || s`, or DER.
    fn sign(&self, label: String, msg: Vec<u8>) -> Result<Vec<u8>, FfiError>;
    fn delete(&self, label: String);
    /// Keys are hardware-backed.
    fn hardware(&self) -> bool;
}

/// Wraps the engine's files at rest with a platform key.
#[uniffi::export(with_foreign)]
pub trait Vault: Send + Sync {
    fn seal(&self, plain: Vec<u8>) -> Result<Vec<u8>, FfiError>;
    /// The engine's `open`; named `unseal` here because UniFFI 0.32.2
    /// escapes Swift's `open` keyword into the C header, which then fails
    /// to compile.
    fn unseal(&self, sealed: Vec<u8>) -> Result<Vec<u8>, FfiError>;
}

/// The way back to the page and the OS.
#[uniffi::export(with_foreign)]
pub trait Listener: Send + Sync {
    /// One JSON message for the page.
    fn to_page(&self, json: String);
    /// Post a local notification (kind `waiting`: sessions newly waiting).
    fn notify(&self, kind: String, count: u32);
}

/// What one background check did.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BackgroundResult {
    /// It reached the relay.
    pub reached: bool,
    /// It ran a session with the host (something was waiting).
    pub handshake: bool,
    /// Sessions newly waiting for input (notified).
    pub waiting: u32,
}

/// The device engine.
#[derive(uniffi::Object)]
pub struct Engine {
    inner: stand_in::Engine,
}

#[uniffi::export]
impl Engine {
    /// `dir`: the app's own folder, out of every backup. `ca_pem`: an extra
    /// root certificate, for tests and debug builds only.
    #[uniffi::constructor]
    pub fn new(
        dir: String,
        ca_pem: Option<String>,
        vault: Arc<dyn Vault>,
        signers: Arc<dyn Signers>,
        listener: Arc<dyn Listener>,
    ) -> Result<Arc<Self>, FfiError> {
        let inner = stand_in::Engine::new(dir, ca_pem, vault, signers, listener)?;
        Ok(Arc::new(Engine { inner }))
    }

    /// A message from the page (JSON text, as the page sent it).
    pub fn page_message(&self, json: String) {
        self.inner.page_message(json)
    }

    /// The native scanner read a pairing code.
    pub fn scanned(&self, qr: String, default_name: String) {
        self.inner.scanned(qr, default_name)
    }

    pub fn lifecycle(&self, foreground: bool) {
        self.inner.lifecycle(foreground)
    }

    /// The network came or went, or its path changed.
    pub fn network(&self, online: bool) {
        self.inner.network(online)
    }

    /// One bounded check for the OS's background refresh; blocks at most
    /// `budget_ms`.
    pub fn background_check(&self, budget_ms: u64) -> BackgroundResult {
        self.inner.background_check(budget_ms)
    }

    /// Forget every host, key and file.
    pub fn wipe(&self) {
        self.inner.wipe()
    }

    pub fn shutdown(&self) {
        self.inner.shutdown()
    }
}

/// A stand-in until the engine crate is wired in: answers `ready` with
/// `unpaired`, echoes requests as `disconnected`.
mod stand_in {
    use super::*;

    pub struct Engine {
        listener: Arc<dyn Listener>,
    }

    impl Engine {
        pub fn new(
            _dir: String,
            _ca_pem: Option<String>,
            _vault: Arc<dyn Vault>,
            _signers: Arc<dyn Signers>,
            listener: Arc<dyn Listener>,
        ) -> Result<Engine, FfiError> {
            Ok(Engine { listener })
        }
        pub fn page_message(&self, json: String) {
            let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
            if v["app"] == "ready" {
                self.listener
                    .to_page(r#"{"event":"conn","state":"unpaired"}"#.into());
            } else if let Some(id) = v["id"].as_u64() {
                self.listener.to_page(
                    serde_json::json!({"id": id, "error": {"code": "disconnected", "message": "not paired"}})
                        .to_string(),
                );
            }
        }
        pub fn scanned(&self, _qr: String, _name: String) {}
        pub fn lifecycle(&self, _fg: bool) {}
        pub fn network(&self, _online: bool) {}
        pub fn background_check(&self, _budget: u64) -> BackgroundResult {
            BackgroundResult {
                reached: false,
                handshake: false,
                waiting: 0,
            }
        }
        pub fn wipe(&self) {}
        pub fn shutdown(&self) {}
    }
}
