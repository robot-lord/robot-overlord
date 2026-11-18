//! `rlord-web`, the opt-in web UI plugin. The daemon starts it
//! (`rlord-web serve`) and supervises it; nobody runs it by hand.
//!
//! On start it reads `[web] port`, makes or reuses the token, binds
//! 127.0.0.1, says `web.hello` with its port, subscribes to the daemon's
//! changes and UI commands, sweeps leftover view sessions, tells the
//! daemon no page is open yet, and serves. It exits when that subscription ends (the daemon
//! went away), so a crashed daemon leaves no listener behind.
//!
//! Exit codes: 0 when the daemon goes away, 1 on an error, 2 on a wrong
//! command line, 3 when the daemon refuses its protocol
//! (`plugin::EXIT_INCOMPATIBLE`), 4 when its port is taken
//! (`plugin::EXIT_PORT_IN_USE`). The daemon retries neither 3 nor 4.

mod assets;
mod daemon;
mod http;
mod pane;
mod token;

use std::process::ExitCode;

use anyhow::{Context, Result};

use rlord_core::config::Config;
use rlord_core::{paths, plugin};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "usage: rlord-web serve | --version
  (started by the rlord daemon: run `rlord web`)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["--version" | "-V"] => {
            println!("{} {VERSION}", plugin::BIN_NAME);
            ExitCode::SUCCESS
        }
        ["serve"] => match serve() {
            Ok(code) => code,
            Err(e) => {
                eprintln!("rlord-web: {e:#}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn serve() -> Result<ExitCode> {
    let cfg = Config::load()?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(serve_on(cfg.web.port))
}

async fn serve_on(port: u16) -> Result<ExitCode> {
    let token = token::load_or_create(&plugin::token_path())?;
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("rlord-web: 127.0.0.1:{port} is in use");
            return Ok(ExitCode::from(plugin::EXIT_PORT_IN_USE as u8));
        }
        Err(e) => return Err(e).with_context(|| format!("listening on 127.0.0.1:{port}")),
    };
    let port = listener.local_addr()?.port();
    let d = daemon::Daemon::new(paths::socket_path());
    if let Err(e) = d.hello(port).await {
        eprintln!("rlord-web: the daemon refused hello: {e}");
        if e.code == i64::from(plugin::RPC_ERR_INCOMPATIBLE) {
            return Ok(ExitCode::from(plugin::EXIT_INCOMPATIBLE as u8));
        }
        return Ok(ExitCode::FAILURE);
    }
    let (rev, mut sub) = d.subscribe().await.context("subscribing to the daemon")?;
    // View sessions a bridge left behind (it died between making one and
    // attaching, or with the last plugin).
    let swept = tokio::task::spawn_blocking(rlord_core::tmux::sweep_views).await?;
    if !swept.is_empty() {
        eprintln!(
            "rlord-web: removed {} leftover view session(s)",
            swept.len()
        );
    }
    let app = http::App::new(token, port, d, rev);
    // No page is open yet (a plugin before this one may have left some).
    app.report_pages().await;
    // The daemon's changes, to every control socket, and its UI commands,
    // each to its page; its end is ours.
    let pump = std::sync::Arc::clone(&app);
    tokio::spawn(async move {
        while let Some(e) = sub.next().await {
            match e {
                daemon::Event::Changed(rev) => pump.changed(rev),
                daemon::Event::Ui { page, cmd } => pump.ui(&page, cmd).await,
            }
        }
        eprintln!("rlord-web: the daemon went away; exiting");
        std::process::exit(0);
    });
    eprintln!("rlord-web {VERSION}: listening on 127.0.0.1:{port}");
    axum::serve(listener, http::router(app)).await?;
    Ok(ExitCode::SUCCESS)
}
