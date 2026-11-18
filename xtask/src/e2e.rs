//! `cargo xtask e2e vectors|wasm|test-wasm`: the end-to-end crypto core's
//! test vectors and its WASM build (`crates/e2e`, `crates/e2e-wasm`).
//!
//! - `vectors [--check]` writes `crates/e2e/vectors/v1.json` from the native
//!   core (or fails when the committed file differs).
//! - `wasm` builds `robot-overlord-e2e-wasm` for `wasm32-unknown-unknown`
//!   (O3, `+simd128`, getrandom's `wasm_js` backend) and runs bindgen in
//!   process (`wasm-bindgen-cli-support`, pinned beside `wasm-bindgen`;
//!   `--target web`) into `crates/e2e-wasm/pkg/` (not committed). No
//!   wasm-pack, no wasm-opt: no tool is downloaded during a build.
//! - `test-wasm` builds it, then runs the page's separate Vitest project
//!   (`crates/web/ui/test/e2e/`): the vectors in Node, byte for byte, and
//!   an interop run (pairing, reconnect, frames both ways) between the
//!   WASM core as the device and the native core as the host
//!   (`rlord-device interop-host`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::cmd;

const VECTORS: &str = "crates/e2e/vectors/v1.json";
const PKG: &str = "crates/e2e-wasm/pkg";
const UI: &str = "crates/web/ui";

const USAGE: &str = "usage: cargo xtask e2e vectors [--check] | wasm | test-wasm

  vectors     write crates/e2e/vectors/v1.json (--check: fail if it is stale)
  wasm        build the WASM core into crates/e2e-wasm/pkg/
  test-wasm   build it, then run the vectors and interop tests in Node (Vitest)";

pub fn main(args: &[String]) -> Result<()> {
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["vectors"] => vectors(false),
        ["vectors", "--check"] => vectors(true),
        ["wasm"] => wasm().map(|_| ()),
        ["test-wasm"] => {
            wasm()?;
            test_wasm()
        }
        _ => cmd::usage(USAGE),
    }
}

fn vectors(check: bool) -> Result<()> {
    let text = rlord_e2e::vectors::document().map_err(anyhow::Error::msg)?;
    if check {
        let old = std::fs::read_to_string(VECTORS).with_context(|| format!("reading {VECTORS}"))?;
        if old != text {
            bail!("{VECTORS} is stale: run `cargo xtask e2e vectors`");
        }
        println!("{VECTORS} is up to date");
    } else {
        std::fs::write(VECTORS, text).with_context(|| format!("writing {VECTORS}"))?;
        println!("wrote {VECTORS}");
    }
    Ok(())
}

/// The rustc flags of the WASM build: getrandom's browser backend and SIMD
/// (10 to 20% faster frames, measured in the spike).
fn wasm_rustflags() -> String {
    [
        "--cfg",
        "getrandom_backend=\"wasm_js\"",
        "-C",
        "target-feature=+simd128",
    ]
    .join("\x1f")
}

fn wasm() -> Result<PathBuf> {
    let target_dir = PathBuf::from("target/wasm");
    cmd::run(
        cmd::cargo()
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-unknown-unknown",
                "-p",
                "robot-overlord-e2e-wasm",
            ])
            .arg("--target-dir")
            .arg(&target_dir)
            .env("CARGO_ENCODED_RUSTFLAGS", wasm_rustflags())
            .env("CARGO_PROFILE_RELEASE_OPT_LEVEL", "3"),
    )?;
    let input = target_dir.join("wasm32-unknown-unknown/release/rlord_e2e_wasm.wasm");
    let out = Path::new(PKG);
    std::fs::create_dir_all(out)?;
    wasm_bindgen_cli_support::Bindgen::new()
        .input_path(&input)
        .web(true)?
        .typescript(true)
        .generate(out)
        .context("wasm-bindgen")?;
    let wasm = out.join("rlord_e2e_wasm_bg.wasm");
    println!(
        "built {} ({} bytes)",
        wasm.display(),
        std::fs::metadata(&wasm)?.len()
    );
    Ok(wasm)
}

fn test_wasm() -> Result<()> {
    // The native half of the interop test.
    cmd::run(cmd::cargo().args(["build", "-p", "robot-overlord-e2e-device"]))?;
    let device = std::fs::canonicalize("target/debug/rlord-device")?;
    let ui = Path::new(UI);
    if !ui.join("node_modules").exists() {
        crate::web::main(&["install".to_string()])?;
    }
    // (Relative to the child's folder, `ui`.)
    cmd::run(
        std::process::Command::new("node_modules/.bin/vitest")
            .args(["run", "--config", "vitest.e2e.config.ts"])
            .env("RLORD_DEVICE_BIN", device)
            .current_dir(ui),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn flags_are_separated_for_cargo() {
        let f = super::wasm_rustflags();
        assert_eq!(f.split('\x1f').count(), 4);
        assert!(f.contains("+simd128"));
    }
}
