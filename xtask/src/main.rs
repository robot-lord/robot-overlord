//! rlord's project automation, run as `cargo xtask <task>`.
//!
//! Anything with real logic lives here rather than in shell: it gets the
//! same compiler, tests, clippy and fmt as the rest of the code and behaves
//! the same on macOS and Linux. The scripts end users run on a bare machine
//! stay shell: get.sh (POSIX sh), install.sh and update.sh (bash). So do
//! the scripts xtask runs inside bare distro containers (`*.sh` next to
//! the task that runs them, compiled in).

mod build;
mod bump;
mod check_deps;
mod cmd;
mod demo_wrap;
mod dev_daemon;
mod fresh_install;
mod preflight;
mod publish;
mod release;
mod screenshots;
mod smoke;
mod tap;
mod tools;
mod version;
mod web;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage: cargo xtask <task>

  release [--local [--skip-smoke] | --resume] [--dry-run]
      publish the version in Cargo.toml as a GitHub release, from the merged main
  build-release macos|linux DIST
      build one platform's release files into DIST
  install-tools
      the Linux build's packaging tools, pinned, into target/release-tools
  smoke-linux DIST [IMAGE…]
      install DIST with get.sh in Linux containers and run a few commands
  check-deps
      fail if the rlord binary depends on network code (the web plugin's)
  web install|check|build|watch
      the web plugin's page (crates/web/ui): install its pinned tools, check, build
  check-version-bump [BASE] [REV]
      fail unless REV bumps the version above BASE's (origin/main, HEAD)
  update-tap VERSION SHA256SUMS [TAP_REPO]
      point the Homebrew formula at a stable release
  publish-crates [--dry-run]
      publish a stable release's crates to crates.io
  release-preflight
      the release workflow's checks (run by .github/workflows/release.yml)
  fresh-install [--dist DIR] [--offline] [--only tarball,deb,rpm,stable] [IMAGE…]
      install rlord on fresh Linux containers and run it end to end (Docker)
  screenshots --website DIR [--out DIR]
      phone-sized screenshots of the demo into the website's public/img
  demo-wrap COLS
      wrap the demo's stand-in screens (stdin) to COLS columns
  dev-daemon
      rebuild, then restart the daemon on the new binary";

fn main() -> ExitCode {
    cmd::on_interrupt();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Every task works from the repository root; a path argument is still
    // read from where the command was run.
    let invoked_from = std::env::current_dir().unwrap_or_default();
    // `cargo run` sets CARGO_MANIFEST_DIR for the checkout it ran in (used
    // only if it is xtask's: an outer cargo may have set it for another
    // crate); the compile-time one covers running the binary directly.
    let is_xtask = |dir: &PathBuf| {
        std::fs::read_to_string(dir.join("Cargo.toml"))
            .is_ok_and(|toml| toml.lines().any(|l| l == "name = \"xtask\""))
    };
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .filter(is_xtask)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let Some(root) = manifest_dir.parent() else {
        eprintln!("xtask: {} has no parent", manifest_dir.display());
        return ExitCode::FAILURE;
    };
    if let Err(e) = std::env::set_current_dir(root) {
        eprintln!("xtask: {}: {e}", root.display());
        return ExitCode::FAILURE;
    }

    let Some((task, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let result = match task.as_str() {
        "release" => release::main(rest),
        "build-release" => build::main(rest, &invoked_from),
        "install-tools" => tools::main(rest),
        "smoke-linux" => smoke::main(rest, &invoked_from),
        "dev-daemon" => dev_daemon::main(rest),
        "check-version-bump" => bump::main(rest),
        "check-deps" => check_deps::main(rest),
        "web" => web::main(rest),
        "update-tap" => tap::main(rest, &invoked_from),
        "publish-crates" => publish::main(rest),
        "release-preflight" => preflight::main(rest),
        "screenshots" => screenshots::main(rest, &invoked_from),
        "fresh-install" => fresh_install::main(rest, &invoked_from),
        "demo-wrap" => demo_wrap::main(rest),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => cmd::usage(format!("unknown task {other}\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if cmd::is_interrupted(&e) => {
            // Not eprintln!: after SIGHUP the terminal is gone, and a failed
            // write must not panic before the signal is re-raised.
            let _ = writeln!(std::io::stderr(), "{e:#}");
            // Everything is cleaned up by now (the tasks' guards dropped).
            cmd::die_by_signal();
            ExitCode::from(130)
        }
        Err(e) if e.downcast_ref::<cmd::Usage>().is_some() => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
