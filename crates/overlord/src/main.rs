use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use overlord_core::paths;
use overlord_daemon::{client_call, Daemon};

#[derive(Parser)]
#[command(
    name = "overlord",
    version,
    about = "AI Overlord — session management for AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground (use the systemd unit for real deployments)
    Daemon,
    /// Check daemon liveness
    Ping,
    /// List sessions
    Ls {
        /// Include archived sessions
        #[arg(long)]
        archived: bool,
        /// Raw JSON output
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::Daemon => run_daemon(),
        Command::Ping => {
            let r = client_call(&paths::socket_path(), "ping", Value::Null)?;
            println!(
                "pong (daemon v{})",
                r.get("version").and_then(Value::as_str).unwrap_or("?")
            );
            Ok(())
        }
        Command::Ls { archived, json } => {
            let r = client_call(
                &paths::socket_path(),
                "session.list",
                json!({"archived": archived}),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print_session_table(&r);
            }
            Ok(())
        }
    }
}

fn run_daemon() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let db = overlord_core::db::Db::open(&paths::db_path())?;
        let daemon = Daemon::new(db);
        let listener = Daemon::bind(&paths::socket_path())?;
        tracing::info!(socket = %paths::socket_path().display(), db = %paths::db_path().display(), "overlordd starting");
        tokio::select! {
            r = daemon.serve(listener) => r,
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down");
                Ok(())
            }
        }
    })
}

fn print_session_table(list: &Value) {
    let Some(rows) = list.as_array() else {
        println!("{list}");
        return;
    };
    if rows.is_empty() {
        println!("no sessions");
        return;
    }
    println!(
        "{:<14} {:<24} {:<8} {:<13} PROJECT",
        "ID", "NAME", "AGENT", "STATUS"
    );
    for s in rows {
        let get = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("-");
        let id = get("id");
        let short_id = &id[..id.len().min(12)];
        println!(
            "{:<14} {:<24} {:<8} {:<13} {}",
            short_id,
            get("name"),
            get("agent"),
            get("status"),
            get("project_path")
        );
    }
}
