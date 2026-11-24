//! `cargo xtask app …`: the phone app (apps/ios; Android later). The tasks
//! make what the Xcode build consumes (the phone page, the UniFFI bindings
//! and the engine's xcframework); the Xcode build never calls them. iOS
//! needs full Xcode: if `xcode-select` points at the command line tools,
//! the tasks set `DEVELOPER_DIR` to `/Applications/Xcode.app` themselves.
//! `app dev` and `app test-e2e` (`app_dev.rs`) need neither Xcode nor a
//! simulator: the phone page runs in a desktop browser or in Node.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::cmd::{self, run};

const USAGE: &str = "\
usage: cargo xtask app <task>

  bindings              UniFFI's Swift (and Kotlin, once apps/android exists) for the engine
  core-ios              the engine for iOS devices and the simulator, as an xcframework
  ios [--sim [NAME|UDID] | --device ID]
                        page, bindings, engine, a Debug build; install and launch
                        (the simulator by default)
  test-ios [--sim NAME|UDID] [--unit]
                        the app's XCTest and XCUITest on a simulator (--unit: XCTest only)
  dev [--port N]        the phone page in a desktop browser: a loopback host (fixture
                        sessions, a fake overlord, the real relay and plugin), the
                        engine behind `rlord-device bridge`, paired, and the page served
                        on 127.0.0.1 (rebuilt on change); prints its URL
  test-e2e              the phone page's store and RemoteTransport in Node over the
                        bridge against that host: pair, list, ask, the relay and the
                        plugin restarted, unpair (no simulator, no Xcode)";

/// The FFI crate and its library name.
const FFI_PACKAGE: &str = "robot-overlord-e2e-ffi";
const FFI_LIB: &str = "rlord_e2e_ffi";
/// iOS devices and the Apple Silicon simulator (rust-toolchain.toml).
const IOS_TARGETS: [&str; 2] = ["aarch64-apple-ios", "aarch64-apple-ios-sim"];
/// The app's minimum iOS (the project's IPHONEOS_DEPLOYMENT_TARGET).
const IOS_MIN: &str = "26.0";
const XCFRAMEWORK: &str = "RlordCore.xcframework";
const BUNDLE_ID: &str = "com.robot-lord.rlord";
const XCODE_DEVELOPER: &str = "/Applications/Xcode.app/Contents/Developer";

pub fn main(args: &[String]) -> Result<()> {
    let root = workspace_root();
    match args.first().map(String::as_str) {
        Some("bindings") => bindings(&root),
        Some("core-ios") => core_ios(&root),
        Some("ios") => ios(&root, &args[1..]),
        Some("test-ios") => test_ios(&root, &args[1..]),
        Some("dev") => crate::app_dev::dev(&root, &args[1..]),
        Some("test-e2e") => crate::app_dev::test_e2e(&root, &args[1..]),
        Some("help" | "--help" | "-h") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => cmd::usage(format!("unknown app task {other}\n{USAGE}")),
    }
}

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .to_path_buf()
}

fn ios_dir(root: &Path) -> PathBuf {
    root.join("apps/ios")
}

/// Full Xcode's developer directory, when `xcode-select` points elsewhere
/// (the command line tools have no iOS SDK).
fn developer_dir() -> Option<String> {
    if let Ok(d) = std::env::var("DEVELOPER_DIR") {
        return Some(d);
    }
    let selected = cmd::output(Command::new("xcode-select").arg("-p")).unwrap_or_default();
    pick_developer_dir(selected.trim(), Path::new(XCODE_DEVELOPER).exists())
}

/// `selected` is what `xcode-select -p` printed.
fn pick_developer_dir(selected: &str, xcode_installed: bool) -> Option<String> {
    if selected.contains("CommandLineTools") || selected.is_empty() {
        xcode_installed.then(|| XCODE_DEVELOPER.to_string())
    } else {
        None
    }
}

/// A command with Xcode's developer directory set if needed.
fn xcode(program: &str) -> Command {
    let mut c = Command::new(program);
    if let Some(d) = developer_dir() {
        c.env("DEVELOPER_DIR", d);
    }
    c
}

/// The phone page into apps/ios/Generated/page.
fn page(root: &Path) -> Result<()> {
    let me = std::env::current_exe().context("finding xtask")?;
    run(Command::new(me).args(["web", "build", "--app"]))?;
    let from = root.join("crates/web/ui/dist-app");
    let to = ios_dir(root).join("Generated/page");
    if to.exists() {
        std::fs::remove_dir_all(&to)?;
    }
    copy_dir(&from, &to)?;
    println!("copied the phone page into {}", to.display());
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let e = e?;
        let dest = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dest)?;
        } else {
            std::fs::copy(e.path(), &dest)?;
        }
    }
    Ok(())
}

/// UniFFI's bindgen, in process (pinned beside `uniffi` in Cargo.lock), from
/// a host build of the FFI crate.
fn bindings(root: &Path) -> Result<()> {
    run(cmd::cargo().args(["build", "-p", FFI_PACKAGE]))?;
    let lib = root.join(format!("target/debug/lib{FFI_LIB}.{}", host_dylib_ext()));
    let tmp = cmd::TempDir::new_in(&root.join("target"), "uniffi")?;
    let out = tmp.path().to_path_buf();
    let mut languages = vec![uniffi_bindgen::bindings::TargetLanguage::Swift];
    let android = root.join("apps/android");
    if android.exists() {
        languages.push(uniffi_bindgen::bindings::TargetLanguage::Kotlin);
    }
    uniffi_bindgen::bindings::generate(uniffi_bindgen::bindings::GenerateOptions {
        languages,
        source: utf8(&lib)?,
        out_dir: utf8(&out)?,
        config_override: None,
        format: false,
        crate_filter: Some(FFI_LIB.to_string()),
        metadata_no_deps: true,
    })
    .context("generating the UniFFI bindings")?;

    let ios = ios_dir(root).join("Generated");
    let swift = ios.join("Swift");
    let headers = ios.join("Headers");
    for d in [&swift, &headers] {
        if d.exists() {
            std::fs::remove_dir_all(d)?;
        }
        std::fs::create_dir_all(d)?;
    }
    for (from, to) in swift_outputs(&out, &swift, &headers) {
        std::fs::copy(&from, &to).with_context(|| format!("copying {}", from.display()))?;
    }
    println!("wrote the Swift bindings into {}", ios.display());
    if android.exists() {
        let kotlin = android.join("app/build/generated/uniffi");
        if kotlin.exists() {
            std::fs::remove_dir_all(&kotlin)?;
        }
        copy_dir(&out.join("uniffi"), &kotlin.join("uniffi"))?;
        println!("wrote the Kotlin bindings into {}", kotlin.display());
    }
    Ok(())
}

/// Where each of UniFFI's Swift outputs goes: the source to the app's
/// sources, the header and the module map (renamed `module.modulemap`, as
/// an xcframework wants) beside each other for the xcframework.
fn swift_outputs(out: &Path, swift: &Path, headers: &Path) -> Vec<(PathBuf, PathBuf)> {
    vec![
        (
            out.join(format!("{FFI_LIB}.swift")),
            swift.join(format!("{FFI_LIB}.swift")),
        ),
        (
            out.join(format!("{FFI_LIB}FFI.h")),
            headers.join(format!("{FFI_LIB}FFI.h")),
        ),
        (
            out.join(format!("{FFI_LIB}FFI.modulemap")),
            headers.join("module.modulemap"),
        ),
    ]
}

fn host_dylib_ext() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

fn utf8(p: &Path) -> Result<camino::Utf8PathBuf> {
    camino::Utf8PathBuf::from_path_buf(p.to_path_buf())
        .map_err(|p| anyhow::anyhow!("not UTF-8: {}", p.display()))
}

/// The engine for both iOS targets, then one xcframework.
fn core_ios(root: &Path) -> Result<()> {
    ensure_macos()?;
    let headers = ios_dir(root).join("Generated/Headers");
    if !headers.join("module.modulemap").exists() {
        bindings(root)?;
    }
    for target in IOS_TARGETS {
        let mut c = cmd::cargo();
        c.args(["build", "--release", "-p", FFI_PACKAGE, "--target", target])
            .env("IPHONEOS_DEPLOYMENT_TARGET", IOS_MIN);
        if let Some(d) = developer_dir() {
            c.env("DEVELOPER_DIR", d);
        }
        run(&mut c)?;
    }
    let out = ios_dir(root).join("Frameworks").join(XCFRAMEWORK);
    if out.exists() {
        std::fs::remove_dir_all(&out)?;
    }
    run(xcode("xcodebuild").args(xcframework_args(root, &headers, &out)))?;
    println!("built {}", out.display());
    Ok(())
}

fn xcframework_args(root: &Path, headers: &Path, out: &Path) -> Vec<String> {
    let mut a = vec!["-create-xcframework".to_string()];
    for target in IOS_TARGETS {
        a.push("-library".into());
        a.push(
            root.join(format!("target/{target}/release/lib{FFI_LIB}.a"))
                .display()
                .to_string(),
        );
        a.push("-headers".into());
        a.push(headers.display().to_string());
    }
    a.push("-output".into());
    a.push(out.display().to_string());
    a
}

/// Where a build goes.
#[derive(Debug, PartialEq)]
enum Dest {
    Sim(Option<String>),
    Device(String),
}

fn parse_dest(args: &[String]) -> Result<Dest> {
    let mut dest = Dest::Sim(None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sim" => {
                dest = Dest::Sim(it.clone().next().filter(|n| !n.starts_with("--")).cloned())
            }
            "--device" => {
                let Some(id) = it.next() else {
                    return cmd::usage("--device needs an id (xcrun devicectl list devices)");
                };
                dest = Dest::Device(id.clone())
            }
            "--unit" => {}
            s if s.starts_with("--") => return cmd::usage(format!("unknown option {s}\n{USAGE}")),
            _ => {}
        }
    }
    Ok(dest)
}

/// The workspace version as Apple wants it: `CFBundleShortVersionString`
/// is numbers only (`1.3.0`); the build number carries the `-dev.N`.
fn apple_versions(version: &str) -> (String, String) {
    match version.split_once("-dev.") {
        Some((base, n)) => (base.to_string(), n.to_string()),
        None => (version.to_string(), "1".to_string()),
    }
}

fn workspace_version(root: &Path) -> Result<String> {
    let toml = std::fs::read_to_string(root.join("Cargo.toml"))?;
    crate::version::workspace_version(&toml).context("no version in Cargo.toml")
}

/// Everything the Xcode build consumes.
fn prepare(root: &Path) -> Result<()> {
    page(root)?;
    bindings(root)?;
    core_ios(root)
}

fn xcodebuild(root: &Path, action: &[&str], destination: &str, signing: bool) -> Result<Command> {
    let (marketing, build) = apple_versions(&workspace_version(root)?);
    let mut c = xcode("xcodebuild");
    c.current_dir(ios_dir(root)).args([
        "-project",
        "rlord.xcodeproj",
        "-scheme",
        "rlord",
        "-configuration",
        "Debug",
        "-destination",
        destination,
        "-derivedDataPath",
        "build",
    ]);
    c.args(action);
    c.arg(format!("MARKETING_VERSION={marketing}"));
    c.arg(format!("CURRENT_PROJECT_VERSION={build}"));
    if signing {
        // Lets Xcode create the team's development certificate and
        // profile, and register a new phone (the first install).
        c.args([
            "-allowProvisioningUpdates",
            "-allowProvisioningDeviceRegistration",
        ]);
    } else {
        // The simulator needs no team's certificate.
        c.args(["CODE_SIGN_IDENTITY=-", "CODE_SIGNING_REQUIRED=NO"]);
    }
    Ok(c)
}

fn ensure_macos() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("the iOS app builds on macOS only");
    }
    Ok(())
}

fn ios(root: &Path, args: &[String]) -> Result<()> {
    ensure_macos()?;
    let dest = parse_dest(args)?;
    prepare(root)?;
    match dest {
        Dest::Sim(name) => {
            let udid = simulator(name.as_deref())?;
            boot(&udid)?;
            run(&mut xcodebuild(
                root,
                &["build"],
                &format!("id={udid}"),
                false,
            )?)?;
            let app = ios_dir(root).join("build/Build/Products/Debug-iphonesimulator/rlord.app");
            run(xcode("xcrun").args(["simctl", "install", &udid]).arg(&app))?;
            // A simulator has no passcode, so the app lock would only
            // cover the page: off for this launch (UserDefaults' argument
            // domain).
            run(xcode("xcrun").args(["simctl", "launch", &udid, BUNDLE_ID, "-appLock", "NO"]))?;
            // Show it (Simulator.app lives inside Xcode); not fatal.
            let dev = developer_dir().unwrap_or_else(|| XCODE_DEVELOPER.to_string());
            let _ = cmd::status_quiet(
                Command::new("open").arg(format!("{dev}/Applications/Simulator.app")),
            );
        }
        Dest::Device(id) => {
            run(&mut xcodebuild(
                root,
                &["build"],
                &format!("id={id}"),
                true,
            )?)?;
            let app = ios_dir(root).join("build/Build/Products/Debug-iphoneos/rlord.app");
            run(xcode("xcrun")
                .args(["devicectl", "device", "install", "app", "--device", &id])
                .arg(&app))?;
            run(xcode("xcrun").args([
                "devicectl",
                "device",
                "process",
                "launch",
                "--device",
                &id,
                BUNDLE_ID,
            ]))?;
        }
    }
    Ok(())
}

fn test_ios(root: &Path, args: &[String]) -> Result<()> {
    ensure_macos()?;
    let unit_only = args.iter().any(|a| a == "--unit");
    let Dest::Sim(name) = parse_dest(args)? else {
        return cmd::usage("test-ios runs on a simulator only");
    };
    prepare(root)?;
    let udid = simulator(name.as_deref())?;
    boot(&udid)?;
    let mut c = xcodebuild(root, &["test"], &format!("id={udid}"), false)?;
    if unit_only {
        c.arg("-only-testing:rlordTests");
        return run(&mut c);
    }
    // The UI test pairs with a real host: the loopback stack, which the
    // simulator reaches on the Mac's 127.0.0.1. Its pairing code and
    // control folder reach the test runner as TEST_RUNNER_ variables.
    let mut stack = Stack::start(root, true)?;
    let qr = stack.pairing_code()?;
    c.env("TEST_RUNNER_RLORD_QR", &qr)
        .env("TEST_RUNNER_RLORD_CONTROL", &stack.control);
    let result = run(&mut c);
    stack.quit();
    result
}

/// The loopback host (`crates/web/examples/remote_stack.rs`): a daemon
/// with fixture sessions, the real plugin and the real relay on 127.0.0.1.
pub struct Stack {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    events: std::sync::mpsc::Receiver<serde_json::Value>,
    /// Where `<name>.cmd` files drive it.
    pub control: String,
    pub relay: String,
    /// Whether its fake overlord runs (it needs tmux).
    pub overlord: bool,
    _dir: cmd::TempDir,
}

const STACK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

impl Stack {
    pub fn start(root: &Path, auto_confirm: bool) -> Result<Stack> {
        run(cmd::cargo().args([
            "build",
            "-p",
            "robot-overlord-web",
            "--bin",
            "rlord-web",
            "--example",
            "remote_stack",
        ]))?;
        let dir = cmd::TempDir::new_in(&root.join("target"), "app-stack")?;
        let mut c = Command::new(root.join("target/debug/examples/remote_stack"));
        c.arg("--dir")
            .arg(dir.path())
            .env("RLORD_WEB_BIN", root.join("target/debug/rlord-web"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        if auto_confirm {
            c.arg("--auto-confirm");
        }
        let mut child = c.spawn().context("starting remote_stack")?;
        let stdin = child.stdin.take().context("stack stdin")?;
        let stdout = child.stdout.take().context("stack stdout")?;
        let (tx, events) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                match parse_event(&line) {
                    Some(v) => {
                        if v["event"] == "confirmed" || v["event"] == "error" {
                            eprintln!("stack: {line}");
                        }
                        if tx.send(v).is_err() {
                            break;
                        }
                    }
                    None => eprintln!("stack: {line}"),
                }
            }
        });
        let mut stack = Stack {
            child,
            stdin,
            events,
            control: String::new(),
            relay: String::new(),
            overlord: false,
            _dir: dir,
        };
        let ready = stack.wait("ready")?;
        stack.control = ready["control"].as_str().unwrap_or_default().to_string();
        stack.relay = ready["relay"].as_str().unwrap_or_default().to_string();
        stack.overlord = ready["overlord"] == true;
        println!("host stack ready: relay {}", stack.relay);
        Ok(stack)
    }

    pub fn send(&mut self, command: &str) -> Result<()> {
        use std::io::Write;
        writeln!(self.stdin, "{command}")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// The next event named `name` (others are skipped).
    pub fn wait(&mut self, name: &str) -> Result<serde_json::Value> {
        let until = std::time::Instant::now() + STACK_WAIT;
        loop {
            let left = until.saturating_duration_since(std::time::Instant::now());
            let v = self
                .events
                .recv_timeout(left)
                .with_context(|| format!("the host stack never said {name}"))?;
            if v["event"] == name {
                return Ok(v);
            }
        }
    }

    pub fn pairing_code(&mut self) -> Result<String> {
        self.send("pair")?;
        let v = self.wait("qr")?;
        v["qr"]
            .as_str()
            .map(str::to_string)
            .context("no qr in the stack's answer")
    }

    pub fn quit(&mut self) {
        let _ = self.send("quit");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < until {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// One event line of the stack; anything else is its log.
fn parse_event(line: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v["event"].is_string().then_some(v)
}

/// The simulator to use: the one named (or with that UDID), else the first
/// available iPhone on an iOS runtime at least the app's minimum.
fn simulator(want: Option<&str>) -> Result<String> {
    let json = cmd::output(xcode("xcrun").args(["simctl", "list", "devices", "available", "-j"]))?;
    pick_simulator(&json, want)
}

fn pick_simulator(json: &str, want: Option<&str>) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(json).context("simctl's device list")?;
    let mut found = Vec::new();
    for (runtime, devices) in v["devices"].as_object().into_iter().flatten() {
        let Some(version) = ios_runtime_version(runtime) else {
            continue;
        };
        for d in devices.as_array().into_iter().flatten() {
            let (Some(name), Some(udid)) = (d["name"].as_str(), d["udid"].as_str()) else {
                continue;
            };
            found.push((version, name.to_string(), udid.to_string()));
        }
    }
    if let Some(w) = want {
        return found
            .into_iter()
            .find(|(_, n, u)| n == w || u == w)
            .map(|(_, _, u)| u)
            .with_context(|| format!("no available simulator named {w}"));
    }
    let min: u32 = IOS_MIN
        .split('.')
        .next()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    found.sort_by_key(|a| std::cmp::Reverse(a.0));
    found
        .into_iter()
        .find(|(v, n, _)| v.0 >= min && n.starts_with("iPhone"))
        .map(|(_, _, u)| u)
        .with_context(|| {
            format!("no iPhone simulator on iOS {IOS_MIN} or later (Xcode, Settings, Components)")
        })
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-27-2` gives (27, 2).
fn ios_runtime_version(runtime: &str) -> Option<(u32, u32)> {
    let rest = runtime.strip_prefix("com.apple.CoreSimulator.SimRuntime.iOS-")?;
    let mut parts = rest.split('-');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

fn boot(udid: &str) -> Result<()> {
    // Booting a booted simulator fails; that's fine.
    let _ = cmd::status_quiet(xcode("xcrun").args(["simctl", "boot", udid]));
    run(xcode("xcrun").args(["simctl", "bootstatus", udid, "-b"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn developer_dir_only_when_the_tools_are_selected() {
        assert_eq!(
            pick_developer_dir("/Library/Developer/CommandLineTools", true).as_deref(),
            Some(XCODE_DEVELOPER)
        );
        assert_eq!(
            pick_developer_dir("/Library/Developer/CommandLineTools", false),
            None
        );
        assert_eq!(pick_developer_dir(XCODE_DEVELOPER, true), None);
        assert_eq!(
            pick_developer_dir("", true).as_deref(),
            Some(XCODE_DEVELOPER)
        );
    }

    #[test]
    fn apple_versions_split_the_dev_number() {
        assert_eq!(apple_versions("1.3.0-dev.5"), ("1.3.0".into(), "5".into()));
        assert_eq!(apple_versions("1.3.0"), ("1.3.0".into(), "1".into()));
    }

    #[test]
    fn destinations() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_dest(&s(&[])).unwrap(), Dest::Sim(None));
        assert_eq!(parse_dest(&s(&["--sim"])).unwrap(), Dest::Sim(None));
        assert_eq!(
            parse_dest(&s(&["--sim", "iPhone 17"])).unwrap(),
            Dest::Sim(Some("iPhone 17".into()))
        );
        assert_eq!(
            parse_dest(&s(&["--sim", "--unit"])).unwrap(),
            Dest::Sim(None)
        );
        assert_eq!(
            parse_dest(&s(&["--device", "ABC"])).unwrap(),
            Dest::Device("ABC".into())
        );
        assert!(parse_dest(&s(&["--device"])).is_err());
        assert!(parse_dest(&s(&["--bogus"])).is_err());
    }

    #[test]
    fn picks_a_new_enough_iphone() {
        let json = r#"{"devices":{
            "com.apple.CoreSimulator.SimRuntime.iOS-25-0":[{"name":"iPhone 16","udid":"OLD"}],
            "com.apple.CoreSimulator.SimRuntime.watchOS-12-0":[{"name":"Watch","udid":"W"}],
            "com.apple.CoreSimulator.SimRuntime.iOS-27-2":[{"name":"iPad Air","udid":"PAD"},{"name":"iPhone 18 Pro","udid":"NEW"}]
        }}"#;
        assert_eq!(pick_simulator(json, None).unwrap(), "NEW");
        assert_eq!(pick_simulator(json, Some("iPhone 16")).unwrap(), "OLD");
        assert_eq!(pick_simulator(json, Some("PAD")).unwrap(), "PAD");
        assert!(pick_simulator(json, Some("nope")).is_err());
        let old = r#"{"devices":{"com.apple.CoreSimulator.SimRuntime.iOS-25-0":[{"name":"iPhone 16","udid":"OLD"}]}}"#;
        assert!(pick_simulator(old, None).is_err());
    }

    #[test]
    fn stack_events() {
        assert_eq!(
            parse_event(r#"{"event":"qr","qr":"RLORD1:x"}"#).unwrap()["qr"],
            "RLORD1:x"
        );
        assert!(parse_event("2026-11-23 INFO something").is_none());
        assert!(parse_event(r#"{"no":"event"}"#).is_none());
    }

    #[test]
    fn runtime_versions() {
        assert_eq!(
            ios_runtime_version("com.apple.CoreSimulator.SimRuntime.iOS-27-2"),
            Some((27, 2))
        );
        assert_eq!(
            ios_runtime_version("com.apple.CoreSimulator.SimRuntime.iOS-26"),
            Some((26, 0))
        );
        assert_eq!(
            ios_runtime_version("com.apple.CoreSimulator.SimRuntime.tvOS-27-2"),
            None
        );
    }

    #[test]
    fn xcframework_takes_both_targets() {
        let a = xcframework_args(Path::new("/r"), Path::new("/h"), Path::new("/o"));
        assert_eq!(a[0], "-create-xcframework");
        assert!(a.contains(&"/r/target/aarch64-apple-ios/release/librlord_e2e_ffi.a".to_string()));
        assert!(
            a.contains(&"/r/target/aarch64-apple-ios-sim/release/librlord_e2e_ffi.a".to_string())
        );
        assert_eq!(a.iter().filter(|s| *s == "-headers").count(), 2);
        assert_eq!(a.last().unwrap(), "/o");
    }

    #[test]
    fn swift_outputs_rename_the_module_map() {
        let o = swift_outputs(Path::new("/o"), Path::new("/s"), Path::new("/h"));
        assert!(o.contains(&(
            PathBuf::from("/o/rlord_e2e_ffiFFI.modulemap"),
            PathBuf::from("/h/module.modulemap")
        )));
        assert!(o.contains(&(
            PathBuf::from("/o/rlord_e2e_ffi.swift"),
            PathBuf::from("/s/rlord_e2e_ffi.swift")
        )));
    }
}
