//! The UniFFI surface of the phone app's device engine. Small on purpose:
//! everything a page feature needs travels as JSON through
//! `Engine::page_message` and `Listener::to_page`, so a new page feature
//! needs no binding change. The platform implements three traits: the
//! relay identity's keys (`Signers`), wrapping at rest (`Vault`) and the
//! way back to the page (`Listener`).

use std::sync::Arc;

use rlord_e2e_client as client;

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
    /// `not_paired`, `nothing_new`, `host_offline`, `refreshed` or
    /// `failed`.
    pub outcome: String,
    /// It reached the relay.
    pub reached: bool,
    /// It ran a session with the host (something was waiting).
    pub handshake: bool,
    /// Sessions newly waiting for input (notified).
    pub waiting: u32,
    /// Why it failed (`failed` only).
    pub reason: Option<String>,
}

impl From<client::BackgroundOutcome> for BackgroundResult {
    fn from(o: client::BackgroundOutcome) -> Self {
        use client::BackgroundOutcome as O;
        let r = |outcome: &str, reached, handshake, waiting, reason| BackgroundResult {
            outcome: outcome.into(),
            reached,
            handshake,
            waiting,
            reason,
        };
        match o {
            O::NotPaired => r("not_paired", false, false, 0, None),
            O::NothingNew => r("nothing_new", true, false, 0, None),
            O::HostOffline => r("host_offline", true, false, 0, None),
            O::Refreshed { notified, .. } => r("refreshed", true, true, notified, None),
            O::Failed { reason } => r("failed", false, false, 0, Some(reason)),
        }
    }
}

/// The device engine.
#[derive(uniffi::Object)]
pub struct Engine {
    inner: client::Engine,
}

#[uniffi::export]
impl Engine {
    /// `dir`: the app's own folder, out of every backup. `ca_pem`: an extra
    /// root certificate, for tests and debug builds only. The engine starts
    /// as if in the background: the shell calls `lifecycle(true)` once the
    /// app is in front.
    #[uniffi::constructor]
    pub fn new(
        dir: String,
        ca_pem: Option<String>,
        vault: Arc<dyn Vault>,
        signers: Arc<dyn Signers>,
        listener: Arc<dyn Listener>,
    ) -> Result<Arc<Self>, FfiError> {
        let inner = client::Engine::new(
            client::Config {
                dir: dir.into(),
                ca_pem,
            },
            Arc::new(VaultAdapter(vault)),
            Arc::new(SignersAdapter(signers)),
            Arc::new(ListenerAdapter(listener)),
        )
        .map_err(|e| FfiError::Failed { message: e.message })?;
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
        self.inner.background_check(budget_ms).into()
    }

    /// Forget every host, key and file.
    pub fn wipe(&self) {
        self.inner.wipe()
    }

    pub fn shutdown(&self) {
        self.inner.shutdown()
    }
}

// The platform's objects, seen by the engine through its own traits.

fn platform(e: FfiError) -> client::PlatformError {
    match e {
        FfiError::Failed { message } => client::PlatformError::new(message),
    }
}

struct SignersAdapter(Arc<dyn Signers>);

impl client::Signers for SignersAdapter {
    fn create(&self, label: String) -> Result<Vec<u8>, client::PlatformError> {
        self.0.create(label).map_err(platform)
    }
    fn public_key(&self, label: String) -> Result<Vec<u8>, client::PlatformError> {
        self.0.public_key(label).map_err(platform)
    }
    fn sign(&self, label: String, msg: Vec<u8>) -> Result<Vec<u8>, client::PlatformError> {
        self.0.sign(label, msg).map_err(platform)
    }
    fn delete(&self, label: String) {
        self.0.delete(label)
    }
    fn hardware(&self) -> bool {
        self.0.hardware()
    }
}

struct VaultAdapter(Arc<dyn Vault>);

impl client::Vault for VaultAdapter {
    fn seal(&self, plain: Vec<u8>) -> Result<Vec<u8>, client::PlatformError> {
        self.0.seal(plain).map_err(platform)
    }
    fn open(&self, sealed: Vec<u8>) -> Result<Vec<u8>, client::PlatformError> {
        self.0.unseal(sealed).map_err(platform)
    }
}

struct ListenerAdapter(Arc<dyn Listener>);

impl client::Listener for ListenerAdapter {
    fn to_page(&self, json: String) {
        self.0.to_page(json)
    }
    fn notify(&self, kind: String, count: u32) {
        self.0.notify(kind, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// A vault that keeps bytes as they are, signers from the engine's own
    /// software keys, and a listener that records what the page would get:
    /// the adapters carry each call both ways.
    struct Plain;
    impl Vault for Plain {
        fn seal(&self, plain: Vec<u8>) -> Result<Vec<u8>, FfiError> {
            Ok(plain)
        }
        fn unseal(&self, sealed: Vec<u8>) -> Result<Vec<u8>, FfiError> {
            Ok(sealed)
        }
    }

    struct Soft(client::soft::SoftSigners);
    impl Signers for Soft {
        fn create(&self, label: String) -> Result<Vec<u8>, FfiError> {
            client::Signers::create(&self.0, label).map_err(fail)
        }
        fn public_key(&self, label: String) -> Result<Vec<u8>, FfiError> {
            client::Signers::public_key(&self.0, label).map_err(fail)
        }
        fn sign(&self, label: String, msg: Vec<u8>) -> Result<Vec<u8>, FfiError> {
            client::Signers::sign(&self.0, label, msg).map_err(fail)
        }
        fn delete(&self, label: String) {
            client::Signers::delete(&self.0, label)
        }
        fn hardware(&self) -> bool {
            false
        }
    }

    fn fail(e: client::PlatformError) -> FfiError {
        FfiError::Failed { message: e.message }
    }

    #[derive(Default)]
    struct Record(Mutex<Vec<String>>);
    impl Listener for Record {
        fn to_page(&self, json: String) {
            self.0.lock().unwrap().push(json);
        }
        fn notify(&self, _kind: String, _count: u32) {}
    }

    #[test]
    fn the_engine_answers_through_the_adapters() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Arc::new(Record::default());
        let e = Engine::new(
            dir.path().to_string_lossy().into(),
            None,
            Arc::new(Plain),
            Arc::new(Soft(client::soft::SoftSigners::default())),
            rec.clone(),
        )
        .unwrap();
        e.page_message(r#"{"app":"ready"}"#.into());
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if rec.0.lock().unwrap().iter().any(|m| m.contains("unpaired")) {
                break;
            }
            assert!(Instant::now() < until, "{:?}", rec.0.lock().unwrap());
            std::thread::sleep(Duration::from_millis(20));
        }
        let r = e.background_check(1_000);
        assert_eq!(r.outcome, "not_paired");
        assert!(!r.reached);
        e.shutdown();
    }

    #[test]
    fn a_platform_failure_crosses_as_its_message() {
        let e = platform(FfiError::Failed {
            message: "keystore refused".into(),
        });
        assert_eq!(e.message, "keystore refused");
    }
}
