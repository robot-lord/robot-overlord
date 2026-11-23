//! The facade the platform calls. Plain sync calls that return at once
//! (except [`Engine::background_check`], which blocks for at most its
//! budget); the engine owns a small tokio runtime of its own that runs
//! the [`Driver`]. `crates/e2e-ffi` wraps this 1:1 with UniFFI.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::driver::{self, Cmd, Driver};
use crate::machine::{Input, Machine};
use crate::{BackgroundOutcome, Config, Listener, PlatformError, Signers, Vault};

pub struct Engine {
    tx: mpsc::UnboundedSender<Cmd>,
    rt: Mutex<Option<tokio::runtime::Runtime>>,
}

impl Engine {
    /// Starts the engine over `cfg.dir` (the paired host, if any, is read
    /// from there). It starts in the background with the network assumed
    /// up: the shell says `lifecycle(true)` when the app is in front.
    pub fn new(
        cfg: Config,
        vault: Arc<dyn Vault>,
        signers: Arc<dyn Signers>,
        listener: Arc<dyn Listener>,
    ) -> Result<Engine, PlatformError> {
        std::fs::create_dir_all(&cfg.dir)
            .map_err(|e| PlatformError::new(format!("{}: {e}", cfg.dir.display())))?;
        let tls = Arc::new(link_tls(cfg.ca_pem.as_deref())?);
        let saved = driver::load(&cfg.dir, vault.as_ref());
        let mut seed = [0u8; 8];
        getrandom::fill(&mut seed).map_err(|e| PlatformError::new(e.to_string()))?;
        let machine = Machine::new(signers.clone(), saved, u64::from_le_bytes(seed));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rlord-engine")
            .enable_all()
            .build()
            .map_err(|e| PlatformError::new(format!("runtime: {e}")))?;
        let (tx, rx) = mpsc::unbounded_channel();
        let d = Driver::new(machine, cfg.dir, tls, vault, signers, listener);
        rt.spawn(d.run(rx));
        Ok(Engine {
            tx,
            rt: Mutex::new(Some(rt)),
        })
    }

    fn input(&self, i: Input) {
        let _ = self.tx.send(Cmd::Input(i));
    }

    /// A message from the page (JSON): a request or an `app` message.
    pub fn page_message(&self, json: String) {
        self.input(Input::Page(json));
    }

    /// The native scanner read `qr`; `default_name` is the phone's model
    /// name, offered as the device name.
    pub fn scanned(&self, qr: String, default_name: String) {
        self.input(Input::Scanned {
            qr,
            name: default_name,
        });
    }

    pub fn lifecycle(&self, foreground: bool) {
        self.input(Input::Lifecycle { foreground });
    }

    pub fn network(&self, online: bool) {
        self.input(Input::Network { online });
    }

    /// One bounded attempt (blocking, at most `budget_ms`): dial, the
    /// cheap check, and only if something waits a session, a snapshot
    /// and a notification of sessions newly waiting for input.
    pub fn background_check(&self, budget_ms: u64) -> BackgroundOutcome {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        if self.tx.send(Cmd::Background { budget_ms, reply }).is_err() {
            return BackgroundOutcome::Failed {
                reason: "the engine has stopped".into(),
            };
        }
        rx.recv_timeout(Duration::from_millis(budget_ms + 1_000))
            .unwrap_or(BackgroundOutcome::Failed {
                reason: "out of time".into(),
            })
    }

    /// Forgets the host, its keys and every file, without the network.
    pub fn wipe(&self) {
        self.input(Input::Wipe);
    }

    /// Stops the engine: its link closes, its runtime ends.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(rt) = self.rt.lock().unwrap().take() {
            // Give the driver a moment to close its link cleanly.
            std::thread::sleep(Duration::from_millis(50));
            rt.shutdown_background();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn link_tls(ca: Option<&str>) -> Result<rustls::ClientConfig, PlatformError> {
    crate::link::tls_config(ca).map_err(PlatformError::new)
}
