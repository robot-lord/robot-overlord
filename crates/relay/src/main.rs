//! `rlord-relay`: serve, print the address, make enrolment tokens, manage
//! hosts, check a config. See docs/relay.md.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rlord_relay::registry::{parse_fingerprint, show_fingerprint};
use rlord_relay::{config, Admin, Config, Status};

#[derive(Parser)]
#[command(
    name = "rlord-relay",
    version,
    about = "The relay rlord hosts and devices dial out to. It sees only ciphertext."
)]
struct Cli {
    /// The config file.
    #[arg(long, global = true, default_value = config::DEFAULT_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the relay. Prints `listening on <ip:port>` and `address <relay
    /// address>` on stdout once bound.
    Serve {
        /// Overrides `listen` (e.g. 0.0.0.0:8080 inside a container).
        #[arg(long)]
        listen: Option<String>,
    },
    /// Print the relay address (origin and key pin) to paste into `rlord
    /// relay setup`. Makes the key on first use.
    Address,
    /// Enrolment tokens.
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Registered hosts.
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },
    /// Load the config, open the store, prove the key signs and verifies,
    /// load the certificate (files mode), then exit.
    Check,
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Make a token; a host's first connection presents it.
    New {
        /// How many hosts may register with it.
        #[arg(long, default_value_t = 1)]
        uses: u32,
        /// How long it is good for: 30m, 24h, 7d (seconds without a unit).
        #[arg(long, default_value = "24h")]
        ttl: String,
        /// The account hosts registered with it belong to.
        #[arg(long, default_value = rlord_relay::registry::LOCAL)]
        account: String,
    },
}

#[derive(Subcommand)]
enum HostCmd {
    List,
    /// Register a host by its relay key fingerprint (for `registration =
    /// "closed"`).
    Add {
        fingerprint: String,
        #[arg(long, default_value = rlord_relay::registry::LOCAL)]
        account: String,
    },
    /// Forget a host and its device list.
    Remove {
        fingerprint: String,
    },
    /// Refuse the host and its devices (4402) until resumed.
    Suspend {
        fingerprint: String,
    },
    Resume {
        fingerprint: String,
    },
}

/// `30`, `30s`, `15m`, `24h`, `7d`.
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = num
        .parse()
        .with_context(|| format!("{s:?} is not a duration (30m, 24h, 7d)"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => bail!("{s:?} is not a duration (30m, 24h, 7d)"),
    };
    if secs == 0 {
        bail!("a token's ttl must be above 0");
    }
    Ok(Duration::from_secs(secs))
}

fn date(t: u64) -> String {
    // Days since the epoch to a calendar date (UTC), no time crate needed.
    let days = (t / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve { listen } => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .init();
            let mut cfg = Config::load(&cli.config)?;
            if let Some(l) = listen {
                cfg.listen = l
                    .parse()
                    .with_context(|| format!("--listen {l:?} is not an ip:port"))?;
            }
            cfg.reload_on_sighup = true;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(serve(cfg))
        }
        Cmd::Address => {
            let cfg = Config::load(&cli.config)?;
            println!("{}", rlord_relay::address(&cfg)?);
            Ok(())
        }
        Cmd::Check => {
            let cfg = Config::load(&cli.config)?;
            for line in rlord_relay::check(&cfg)? {
                println!("{line}");
            }
            println!("ok");
            Ok(())
        }
        Cmd::Token { cmd } => {
            let cfg = Config::load(&cli.config)?;
            let admin = Admin::open(&cfg.data_dir)?;
            match cmd {
                TokenCmd::New { uses, ttl, account } => {
                    println!(
                        "{}",
                        admin.token_new(uses, parse_duration(&ttl)?, &account)?
                    );
                }
            }
            Ok(())
        }
        Cmd::Host { cmd } => {
            let cfg = Config::load(&cli.config)?;
            let admin = Admin::open(&cfg.data_dir)?;
            let found = |ok: bool, fp: &str| {
                if ok {
                    Ok(())
                } else {
                    bail!("no host {fp}")
                }
            };
            match cmd {
                HostCmd::List => {
                    for h in admin.hosts()? {
                        println!(
                            "{}  {}  {}  devices {}  first seen {}  last seen {}",
                            show_fingerprint(&h.fingerprint),
                            h.account,
                            match h.status {
                                Status::Active => "active",
                                Status::Suspended => "suspended",
                            },
                            h.devices,
                            date(h.first_seen),
                            date(h.last_seen),
                        );
                    }
                    Ok(())
                }
                HostCmd::Add {
                    fingerprint,
                    account,
                } => {
                    if !admin.host_add(&parse_fingerprint(&fingerprint)?, &account)? {
                        bail!("{fingerprint} is already registered");
                    }
                    Ok(())
                }
                HostCmd::Remove { fingerprint } => found(
                    admin.host_remove(&parse_fingerprint(&fingerprint)?)?,
                    &fingerprint,
                ),
                HostCmd::Suspend { fingerprint } => found(
                    admin.host_suspend(&parse_fingerprint(&fingerprint)?)?,
                    &fingerprint,
                ),
                HostCmd::Resume { fingerprint } => found(
                    admin.host_resume(&parse_fingerprint(&fingerprint)?)?,
                    &fingerprint,
                ),
            }
        }
    }
}

async fn serve(cfg: Config) -> Result<()> {
    let running = rlord_relay::start(cfg).await?;
    {
        let mut out = std::io::stdout().lock();
        writeln!(out, "listening on {}", running.addr())?;
        writeln!(out, "address {}", running.address())?;
        out.flush()?;
    }
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    running.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_dates() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        for bad in ["", "0", "1w", "h", "-1h"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_763_337_600), "2025-11-17");
        assert_eq!(date(951_782_400), "2000-02-29");
    }
}
