//! `cargo xtask fresh-install [--dist DIR] [--offline] [--only WAYS] [IMAGE…]`:
//! install rlord on fresh Linux machines (throwaway Docker containers, colima
//! on the Mac) and run it end to end as a new user would: first run with no
//! agent, then with stand-ins for Claude Code, Codex and OpenCode on PATH.
//!
//! Each image is tried every way it can install rlord: `tarball` (get.sh
//! from the local release files), `deb` or `rpm` (the distro package), and
//! `stable` (get.sh from GitHub's newest stable release, then the build
//! under test over it with the old daemon running). The container runs
//! `fresh_install.sh`, plain glue that frames each step's output; the
//! judging is here. Logs go to `target/fresh-install/`.
//!
//! - `--dist DIR`: release files already built (the Linux tarballs and
//!   packages; SHA256SUMS is written if missing). Default: build them with
//!   `build-release linux` (the pinned packaging tools from `cargo xtask
//!   install-tools`).
//! - `--offline`: no GitHub: skips `stable` and the `rlord update` check.
//! - `--only tarball,deb,rpm,stable`: just those ways.
//! - IMAGE: `ubuntu:24.04`, or `linux/amd64:ubuntu:24.04` for another
//!   platform (emulated). Default: ubuntu:24.04, debian:stable,
//!   fedora:latest, native. Under emulation (qemu) the list can't set up
//!   its terminal (ENOSYS), so its first-run screen isn't checked there.
//!
//! The stand-in agents (`fresh_install_standin.sh`) answer `--version`,
//! log every call and otherwise idle: no real agent, no model call. Every
//! tmux server involved is inside the container.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::build::{self, Platform};
use crate::cmd::{self, TempDir};
use crate::version::{self, Version};

const USAGE: &str =
    "usage: cargo xtask fresh-install [--dist DIR] [--offline] [--only tarball,deb,rpm,stable] [IMAGE…]";
const DEFAULT_IMAGES: [&str; 3] = ["ubuntu:24.04", "debian:stable", "fedora:latest"];
const SCENARIO: &str = include_str!("fresh_install.sh");
const STANDIN: &str = include_str!("fresh_install_standin.sh");
/// The user the scenario creates (fresh_install.sh's `user`).
const HOME: &str = "/home/alice";

/// How rlord gets onto the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    Tarball,
    Deb,
    Rpm,
    Stable,
}

impl Way {
    const ALL: [Way; 4] = [Way::Tarball, Way::Deb, Way::Rpm, Way::Stable];

    fn name(self) -> &'static str {
        match self {
            Way::Tarball => "tarball",
            Way::Deb => "deb",
            Way::Rpm => "rpm",
            Way::Stable => "stable",
        }
    }

    fn parse(s: &str) -> Option<Way> {
        Way::ALL.into_iter().find(|w| w.name() == s)
    }

    /// Where a new login shell should find rlord.
    fn bin(self) -> String {
        match self {
            Way::Tarball | Way::Stable => format!("{HOME}/.local/bin/rlord"),
            Way::Deb | Way::Rpm => "/usr/bin/rlord".into(),
        }
    }

    /// The steps fresh_install.sh runs this way, in order.
    fn steps(self, network: bool) -> Vec<String> {
        let mut v: Vec<String> = vec!["prep".into()];
        if self == Way::Stable {
            v.extend(
                [
                    "stable-install",
                    "stable-version",
                    "stable-ls",
                    "stable-update",
                ]
                .map(String::from),
            );
        }
        v.extend(["install", "login-path", "version", "tmux-version"].map(String::from));
        if self == Way::Stable {
            v.push("upgrade-ls".into());
        } else {
            v.extend(["ping-cold", "ls-first"].map(String::from));
        }
        v.extend(
            [
                "ping",
                "web-install",
                "web-status",
                "web-url",
                "web-stop",
                "tui-first-run",
                "tui-setup-form",
                "agents-none-json",
                "agents-none",
                "overlord-status-none",
                "new-none",
                "setup",
                "setup-config",
                "standins",
                "agents-standins-json",
                "install-hooks",
                "agents-hooks-json",
            ]
            .map(String::from),
        );
        v.extend(AGENTS.map(|a| format!("new-{a}")));
        v.extend(["sessions-json", "rlord-socket"].map(String::from));
        v.extend(AGENTS.map(|a| format!("stop-{a}")));
        v.extend(["reset", "ping-after-reset"].map(String::from));
        if network {
            v.push("update".into());
        }
        v.push("standin-calls".into());
        if matches!(self, Way::Deb | Way::Rpm) {
            v.extend(["uninstall", "gone"].map(String::from));
        }
        v
    }
}

const AGENTS: [&str; 3] = ["claude", "codex", "opencode"];

/// The distro package an image installs, by its name.
fn package_way(image: &str) -> Option<Way> {
    // "linux/amd64:ubuntu:24.04" → "ubuntu"; "quay.io/fedora/fedora:41" → "fedora".
    let image = image.strip_prefix("linux/").map_or(image, |rest| {
        rest.split_once(':').map_or(rest, |(_, img)| img)
    });
    let repo = image.split(':').next().unwrap_or(image);
    let name = repo.rsplit('/').next().unwrap_or(repo);
    match name {
        "ubuntu" | "debian" => Some(Way::Deb),
        "fedora" | "rockylinux" | "almalinux" | "amazonlinux" => Some(Way::Rpm),
        _ => None,
    }
}

/// Every way to try on `image`, given `--only` and `--offline`.
fn ways_for(image: &str, only: &[Way], network: bool) -> Vec<Way> {
    let mut v = vec![Way::Tarball];
    v.extend(package_way(image));
    if network {
        v.push(Way::Stable);
    }
    v.retain(|w| only.is_empty() || only.contains(w));
    v
}

/// One framed step of the container's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub name: String,
    pub rc: i32,
    pub out: String,
}

/// The steps in a container's output, and whether it ran to the end.
fn parse_steps(log: &str) -> (Vec<Step>, bool) {
    let mut steps = Vec::new();
    let mut open: Option<(String, Vec<&str>)> = None;
    let mut done = false;
    for line in log.lines() {
        if let Some(name) = line.strip_prefix("@@step ") {
            open = Some((name.trim().to_string(), Vec::new()));
        } else if let Some(rc) = line.strip_prefix("@@rc ") {
            if let Some((name, lines)) = open.take() {
                steps.push(Step {
                    name,
                    rc: rc.trim().parse().unwrap_or(-1),
                    out: lines.join("\n"),
                });
            }
        } else if line == "@@done" {
            done = true;
        } else if let Some((_, lines)) = open.as_mut() {
            lines.push(line);
        }
    }
    (steps, done)
}

/// What a step must show: None when it passed, else why not.
fn judge(step: &Step, way: Way, ver: &Version) -> Option<String> {
    let ok = step.rc == 0;
    let has = |s: &str| step.out.contains(s);
    let last = step.out.lines().last().unwrap_or("").trim();
    let need = |cond: bool, why: &str| (!cond).then(|| why.to_string());
    let name = step.name.as_str();
    match name {
        "stable-version" => need(
            ok && last
                .strip_prefix("rlord ")
                .is_some_and(|v| !v.contains('-')),
            "expected a stable `rlord X.Y.Z`",
        ),
        "stable-update" => need(
            ok && has("already the newest"),
            "`rlord update` on the newest stable should say it's already the newest",
        ),
        "login-path" => need(
            ok && last == way.bin(),
            &format!("a new login shell should find {}", way.bin()),
        ),
        "version" => need(
            last == format!("rlord {ver}"),
            &format!("expected rlord {ver}"),
        ),
        "tmux-version" => need(ok && tmux_new_enough(last), "rlord needs tmux 3.2 or newer"),
        "ping-cold" => need(
            !ok && has("daemon"),
            "with no daemon, `rlord ping` should fail and say so",
        ),
        "ls-first" => need(ok && has("no sessions"), "expected `no sessions`"),
        "web-install" => need(
            ok && has("installed"),
            "`rlord web install --from` should install the release's plugin",
        ),
        "web-status" => need(
            ok && has("(verified)"),
            "the installed plugin should match this rlord's compiled-in hash",
        ),
        "web-url" => need(
            ok && has("http://127.0.0.1:"),
            "`rlord web url` should start the plugin and print its address",
        ),
        "ping" | "ping-after-reset" => need(
            ok && has(&format!("daemon v{ver}")),
            &format!("expected a daemon on v{ver}"),
        ),
        "tui-first-run" => need(
            has("Start your overlord") && has("rlord agents"),
            "the first run should open the overlord setup, saying no agent is installed",
        ),
        "tui-setup-form" => need(
            has("Where do you keep your code") && has("rlord found no projects yet"),
            "after the overlord dialog, the code-folder form should open, with no projects seen",
        ),
        "setup" => need(
            ok && has("code folder: ~/proj"),
            "`rlord setup <path>` should save the code folder",
        ),
        "setup-config" => need(
            ok && has("code_dir = \"~/proj\""),
            "the config should hold the code folder after `rlord setup`",
        ),
        "agents-none-json" => agents_check(&step.out, |a| {
            (a["installed"] == false && a["install"]["command"].as_str().is_some())
                .then_some(())
                .ok_or("should be not installed, with an install command")
        }),
        "new-none" => need(
            !ok && has("isn't installed"),
            "launching with no agent should fail and say it isn't installed",
        ),
        "agents-standins-json" => agents_check(&step.out, |a| {
            (a["installed"] == true && a["too_old"].is_null())
                .then_some(())
                .ok_or("should be installed (the stand-in) and new enough")
        }),
        "agents-hooks-json" => agents_check(&step.out, |a| {
            let needs_hooks = a["name"] != "opencode";
            (!needs_hooks || a["hooks_installed"] == true)
                .then_some(())
                .ok_or("should have rlord's hooks after `rlord install-hooks`")
        }),
        "sessions-json" => sessions_check(&step.out),
        "rlord-socket" => need(
            ok && AGENTS.iter().all(|a| has(&format!("e2e-{a}"))),
            "rlord's tmux server should hold the three sessions",
        ),
        // A package's rlord updates with its package: `rlord update` says
        // so rather than putting a second copy in ~/.local/bin.
        "update" if matches!(way, Way::Deb | Way::Rpm) => need(
            !ok && has(".deb or .rpm package"),
            "`rlord update` should refuse a package's rlord and say how to update it",
        ),
        "update" => need(
            ok && (has("already the newest") || has("keeping it")),
            "`rlord update` should keep a build newer than the newest release",
        ),
        "standin-calls" => standin_calls_check(&step.out),
        "gone" => need(!ok, "rlord should be gone after removing the package"),
        _ if name.starts_with("new-") => need(ok && has("launched"), "expected `launched`"),
        _ => need(ok, "failed"),
    }
}

/// `tmux 3.4`, `tmux 3.2a`, `tmux next-3.5` (assumed new enough).
fn tmux_new_enough(v: &str) -> bool {
    let Some(rest) = v.strip_prefix("tmux ") else {
        return false;
    };
    let digits = |s: &str| -> Option<u32> {
        s.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    };
    let Some((major, minor)) = rest.split_once('.') else {
        return false;
    };
    match (digits(major), digits(minor)) {
        (Some(a), Some(b)) => (a, b) >= (3, 2),
        _ => rest.starts_with("next-"),
    }
}

/// `rlord agents --json`: all three agents, each passing `each`.
fn agents_check(out: &str, each: impl Fn(&Value) -> Result<(), &'static str>) -> Option<String> {
    let v: Value = match serde_json::from_str(out) {
        Ok(v) => v,
        Err(e) => return Some(format!("not JSON: {e}")),
    };
    let agents = v["agents"].as_array().cloned().unwrap_or_default();
    let mut why = Vec::new();
    for name in AGENTS {
        match agents.iter().find(|a| a["name"] == name) {
            None => why.push(format!("{name}: missing")),
            Some(a) => {
                if let Err(e) = each(a) {
                    why.push(format!("{name}: {e}"));
                }
            }
        }
    }
    (!why.is_empty()).then(|| why.join("; "))
}

/// `rlord ls --json`: the three stand-in sessions are listed.
fn sessions_check(out: &str) -> Option<String> {
    let v: Value = match serde_json::from_str(out) {
        Ok(v) => v,
        Err(e) => return Some(format!("not JSON: {e}")),
    };
    let rows = v
        .as_array()
        .or_else(|| v["sessions"].as_array())
        .cloned()
        .unwrap_or_default();
    let missing: Vec<String> = AGENTS
        .iter()
        .map(|a| format!("e2e-{a}"))
        .filter(|n| !rows.iter().any(|r| r["name"] == n.as_str()))
        .collect();
    (!missing.is_empty()).then(|| format!("not listed: {}", missing.join(", ")))
}

/// rlord may ask an agent its version, list its models, check its login,
/// launch it and so on, but never install, update or log it in or out.
fn standin_calls_check(out: &str) -> Option<String> {
    const NEVER: [&str; 6] = [
        "install",
        "update",
        "upgrade",
        "uninstall",
        "login",
        "logout",
    ];
    let bad: Vec<&str> = out
        .lines()
        .filter(|l| {
            let mut words = l.split_whitespace().skip(1);
            match words.next() {
                Some("auth") => words.next() != Some("status"),
                Some(sub) => NEVER.contains(&sub),
                None => false,
            }
        })
        .collect();
    (!bad.is_empty()).then(|| format!("rlord ran: {}", bad.join(" | ")))
}

/// One container's verdict.
struct Run {
    image: String,
    way: Way,
    log: PathBuf,
    problems: Vec<String>,
}

/// What went wrong in a run (nothing: it passed). `emulated`: another
/// platform than Docker's own, under qemu, where the list can't set up its
/// terminal (ENOSYS): its first-run screen isn't checked there.
fn verdict(log: &str, way: Way, ver: &Version, network: bool, emulated: bool) -> Vec<String> {
    let (steps, done) = parse_steps(log);
    let mut problems = Vec::new();
    let mut wanted = way.steps(network);
    if emulated {
        wanted.retain(|s| s != "tui-first-run");
    }
    for (i, want) in wanted.iter().enumerate() {
        match steps.iter().find(|s| &s.name == want) {
            // The scenario stopped here: one line, not one per later step.
            None if !steps.iter().any(|s| wanted[i..].contains(&s.name)) => {
                problems.push(format!(
                    "{want} and the {} steps after it didn't run",
                    wanted.len() - i - 1
                ));
                break;
            }
            None => problems.push(format!("{want}: didn't run")),
            Some(s) => {
                if let Some(why) = judge(s, way, ver) {
                    let tail: Vec<&str> = s.out.lines().rev().take(3).collect();
                    let tail: Vec<&str> = tail.into_iter().rev().collect();
                    problems.push(format!(
                        "{want}: {why} (exit {}; ends: {})",
                        s.rc,
                        tail.join(" ⏎ ")
                    ));
                }
            }
        }
    }
    if !done && problems.is_empty() {
        problems.push("the scenario stopped before the end".into());
    }
    problems
}

pub fn main(args: &[String], invoked_from: &Path) -> Result<()> {
    let mut dist: Option<PathBuf> = None;
    let mut network = true;
    let mut only = Vec::new();
    let mut images = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dist" => match it.next() {
                Some(d) => dist = Some(invoked_from.join(d)),
                None => return cmd::usage(USAGE),
            },
            "--offline" => network = false,
            "--only" => {
                let Some(list) = it.next() else {
                    return cmd::usage(USAGE);
                };
                for w in list.split(',') {
                    match Way::parse(w.trim()) {
                        Some(w) => only.push(w),
                        None => return cmd::usage(format!("unknown way {w}\n{USAGE}")),
                    }
                }
            }
            s if s.starts_with('-') => return cmd::usage(format!("unknown option {s}\n{USAGE}")),
            image => images.push(image.to_string()),
        }
    }
    if images.is_empty() {
        images = DEFAULT_IMAGES.map(String::from).to_vec();
    }
    if !cmd::succeeds(Command::new("docker").arg("info"))? {
        bail!("Docker isn't running (colima start)");
    }
    let ver = version::current()?;

    // Under the checkout: Docker on the Mac (colima) only sees the home
    // folder.
    let out_dir = PathBuf::from("target/fresh-install");
    std::fs::create_dir_all(&out_dir)?;
    let out_dir = out_dir.canonicalize()?;
    let _built;
    let dist = match dist {
        Some(d) => d
            .canonicalize()
            .with_context(|| format!("{}", d.display()))?,
        None => {
            let t = TempDir::new_in(&out_dir, "dist")?;
            build::build(Platform::Linux, t.path())?;
            let d = t.path().to_path_buf();
            _built = t;
            d
        }
    };
    if !dist.join("SHA256SUMS").is_file() {
        crate::release::write_sums(&dist)?;
    }
    let files = TempDir::new_in(&out_dir, "scripts")?;
    std::fs::write(files.path().join("fresh_install.sh"), SCENARIO)?;
    std::fs::write(files.path().join("standin.sh"), STANDIN)?;
    std::fs::copy("scripts/get.sh", files.path().join("get.sh"))?;

    let mut jobs = Vec::new();
    for image in &images {
        for way in ways_for(image, &only, network) {
            jobs.push((image.clone(), way));
        }
    }
    if jobs.is_empty() {
        bail!("nothing to run (see --only)");
    }
    println!(
        "fresh install of rlord {ver}: {} runs ({}), logs in {}",
        jobs.len(),
        if network { "online" } else { "offline" },
        out_dir.display()
    );
    // All at once: each is mostly waiting on package mirrors.
    let handles: Vec<_> = jobs
        .into_iter()
        .map(|(image, way)| {
            let (dist, files, out_dir) =
                (dist.clone(), files.path().to_path_buf(), out_dir.clone());
            std::thread::spawn(move || -> Result<(String, Way, PathBuf, String)> {
                let log = out_dir.join(format!("{}-{}.log", slug(&image), way.name()));
                let text = run_container(&image, way, network, &dist, &files)?;
                std::fs::write(&log, &text)?;
                Ok((image, way, log, text))
            })
        })
        .collect();
    let mut runs = Vec::new();
    for h in handles {
        let (image, way, log, text) = h.join().expect("container thread")?;
        let emulated = platform_and_image(&image)
            .0
            .is_some_and(|p| p != native_platform());
        let problems = verdict(&text, way, &ver, network, emulated);
        runs.push(Run {
            image,
            way,
            log,
            problems,
        });
    }
    cmd::check_interrupted()?;

    let mut failed = 0;
    for r in &runs {
        let mark = if r.problems.is_empty() {
            "ok  "
        } else {
            "FAIL"
        };
        println!("{mark} {:<28} {}", r.image, r.way.name());
        for p in &r.problems {
            println!("       {p}");
        }
        if !r.problems.is_empty() {
            println!("       log: {}", r.log.display());
            failed += 1;
        }
    }
    if failed > 0 {
        bail!("{failed} of {} fresh installs failed", runs.len());
    }
    Ok(())
}

/// `linux/amd64:ubuntu:24.04` → ("--platform linux/amd64", "ubuntu:24.04").
fn platform_and_image(spec: &str) -> (Option<&str>, &str) {
    if spec.starts_with("linux/") {
        if let Some((p, img)) = spec.split_once(':') {
            return (Some(p), img);
        }
    }
    (None, spec)
}

/// Docker's own platform (colima's VM runs the Mac's architecture).
fn native_platform() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "linux/arm64",
        _ => "linux/amd64",
    }
}

/// A file-name-safe form of an image spec.
fn slug(spec: &str) -> String {
    spec.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Run the scenario in a throwaway container; everything it printed.
fn run_container(
    image: &str,
    way: Way,
    network: bool,
    dist: &Path,
    files: &Path,
) -> Result<String> {
    let (platform, image) = platform_and_image(image);
    let mut c = Command::new("docker");
    c.args(["run", "--rm"]);
    if let Some(p) = platform {
        c.args(["--platform", p]);
    }
    // Offline still reaches the package mirrors (sudo, tmux): no
    // `--network none`; the scenario just skips its GitHub steps.
    c.arg("-v")
        .arg(format!("{}:/dist:ro", dist.display()))
        .arg("-v")
        .arg(format!("{}:/e2e:ro", files.display()))
        .args(["-e", &format!("E2E_INSTALL={}", way.name())])
        .args(["-e", &format!("E2E_NETWORK={}", u8::from(network))])
        .args([image, "sh", "/e2e/fresh_install.sh"])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    let out = c
        .output()
        .with_context(|| format!("running {image} ({})", way.name()))?;
    // The verdict comes from the steps, not docker's exit status.
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ver() -> Version {
        Version::parse("0.23.0-dev.23").unwrap()
    }

    fn step(name: &str, rc: i32, out: &str) -> Step {
        Step {
            name: name.into(),
            rc,
            out: out.into(),
        }
    }

    #[test]
    fn parses_framed_steps() {
        let log = "noise\n@@step a\nline 1\nline 2\n@@rc 0\n@@step b\n@@rc 3\n@@step c\nhalf";
        let (steps, done) = parse_steps(log);
        assert_eq!(steps, [step("a", 0, "line 1\nline 2"), step("b", 3, "")]);
        assert!(!done);
        assert!(parse_steps("@@step a\n@@rc 0\n@@done\n").1);
    }

    #[test]
    fn ways_by_image() {
        assert_eq!(package_way("ubuntu:24.04"), Some(Way::Deb));
        assert_eq!(package_way("linux/amd64:debian:stable"), Some(Way::Deb));
        assert_eq!(package_way("fedora:latest"), Some(Way::Rpm));
        assert_eq!(package_way("quay.io/fedora/fedora:41"), Some(Way::Rpm));
        assert_eq!(package_way("alpine:3"), None);
        assert_eq!(
            ways_for("fedora:latest", &[], true),
            [Way::Tarball, Way::Rpm, Way::Stable]
        );
        assert_eq!(ways_for("debian:12", &[], false), [Way::Tarball, Way::Deb]);
        assert_eq!(ways_for("alpine:3", &[Way::Deb], true), []);
        assert_eq!(
            platform_and_image("linux/amd64:ubuntu:24.04"),
            (Some("linux/amd64"), "ubuntu:24.04")
        );
        assert_eq!(platform_and_image("ubuntu:24.04"), (None, "ubuntu:24.04"));
        assert_eq!(slug("linux/amd64:ubuntu:24.04"), "linux-amd64-ubuntu-24-04");
    }

    #[test]
    fn steps_follow_the_way() {
        let deb = Way::Deb.steps(true);
        assert!(deb.contains(&"uninstall".to_string()) && deb.contains(&"update".to_string()));
        assert!(deb.contains(&"ping-cold".to_string()));
        let stable = Way::Stable.steps(true);
        assert!(stable.contains(&"upgrade-ls".to_string()));
        assert!(!stable.contains(&"ping-cold".to_string()));
        assert!(!Way::Tarball.steps(false).contains(&"update".to_string()));
    }

    #[test]
    fn tmux_versions() {
        assert!(tmux_new_enough("tmux 3.4"));
        assert!(tmux_new_enough("tmux 3.2a"));
        assert!(tmux_new_enough("tmux 10.0"));
        assert!(tmux_new_enough("tmux next-3.5"));
        assert!(!tmux_new_enough("tmux 3.1c"));
        assert!(!tmux_new_enough("tmux 2.9"));
        assert!(!tmux_new_enough("bash: tmux: command not found"));
    }

    #[test]
    fn judges_versions_and_paths() {
        let v = ver();
        assert_eq!(
            judge(&step("version", 0, "rlord 0.23.0-dev.23"), Way::Deb, &v),
            None
        );
        assert!(judge(&step("version", 0, "rlord 0.18.1"), Way::Deb, &v).is_some());
        assert_eq!(
            judge(&step("login-path", 0, "/usr/bin/rlord"), Way::Rpm, &v),
            None
        );
        assert!(judge(&step("login-path", 1, ""), Way::Tarball, &v).is_some());
        assert_eq!(
            judge(
                &step("login-path", 0, "/home/alice/.local/bin/rlord"),
                Way::Tarball,
                &v
            ),
            None
        );
        assert_eq!(
            judge(&step("stable-version", 0, "rlord 0.18.1"), Way::Stable, &v),
            None
        );
        assert!(judge(&step("ping-cold", 0, "pong"), Way::Deb, &v).is_some());
        assert_eq!(
            judge(
                &step("ping", 0, "pong (daemon v0.23.0-dev.23)"),
                Way::Deb,
                &v
            ),
            None
        );
        assert_eq!(
            judge(
                &step(
                    "setup",
                    0,
                    "code folder: ~/proj · the repo finder scans ~/proj"
                ),
                Way::Deb,
                &v
            ),
            None
        );
        assert!(judge(&step("setup-config", 0, "code_dir = \"\""), Way::Deb, &v).is_some());
        // Unlisted steps just need to succeed.
        assert_eq!(judge(&step("stop-codex", 0, ""), Way::Deb, &v), None);
        assert!(judge(&step("reset", 1, "old daemon did not exit"), Way::Deb, &v).is_some());
    }

    #[test]
    fn judges_agent_listings() {
        let none = r#"{"agents": [
            {"name": "claude", "installed": false, "install": {"command": "curl …"}},
            {"name": "codex", "installed": false, "install": {"command": "curl …"}},
            {"name": "opencode", "installed": false, "install": {"command": "curl …"}}]}"#;
        let v = ver();
        assert_eq!(
            judge(&step("agents-none-json", 0, none), Way::Deb, &v),
            None
        );
        let why = judge(&step("agents-standins-json", 0, none), Way::Deb, &v).unwrap();
        assert!(why.contains("claude:") && why.contains("opencode:"));
        let hooks = r#"{"agents": [
            {"name": "claude", "installed": true, "hooks_installed": true},
            {"name": "codex", "installed": true, "hooks_installed": false},
            {"name": "opencode", "installed": true, "hooks_installed": null}]}"#;
        let why = judge(&step("agents-hooks-json", 0, hooks), Way::Deb, &v).unwrap();
        assert!(why.starts_with("codex:"), "{why}");
        assert!(
            judge(&step("agents-none-json", 0, "Error: no"), Way::Deb, &v)
                .unwrap()
                .starts_with("not JSON")
        );
    }

    #[test]
    fn judges_sessions_and_stand_in_calls() {
        let v = ver();
        let rows = r#"[{"name": "e2e-claude"}, {"name": "e2e-codex"}, {"name": "e2e-opencode"}]"#;
        assert_eq!(judge(&step("sessions-json", 0, rows), Way::Deb, &v), None);
        let why = judge(
            &step("sessions-json", 0, r#"[{"name": "e2e-claude"}]"#),
            Way::Deb,
            &v,
        );
        assert_eq!(why.as_deref(), Some("not listed: e2e-codex, e2e-opencode"));
        let calls = "claude --version\nopencode models\ncodex app-server daemon version\n\
                     claude --session-id x --model opus\nclaude auth status --json";
        assert_eq!(judge(&step("standin-calls", 0, calls), Way::Deb, &v), None);
        let why = judge(
            &step("standin-calls", 0, "claude --version\nclaude update"),
            Way::Deb,
            &v,
        );
        assert_eq!(why.as_deref(), Some("rlord ran: claude update"));
        let why = judge(&step("standin-calls", 0, "codex auth login"), Way::Deb, &v);
        assert_eq!(why.as_deref(), Some("rlord ran: codex auth login"));
        // A package's rlord refuses `rlord update`; get.sh's keeps a newer build.
        let refused = "Error: this rlord (/usr/bin/rlord) came from a .deb or .rpm package: …";
        assert_eq!(judge(&step("update", 1, refused), Way::Rpm, &v), None);
        assert!(judge(&step("update", 0, "rlord 0.18.1 installed"), Way::Deb, &v).is_some());
        let kept =
            "rlord 0.23.0-dev.23 is newer than the newest stable release (0.18.1); keeping it";
        assert_eq!(judge(&step("update", 0, kept), Way::Tarball, &v), None);
    }

    #[test]
    fn a_missing_step_fails_the_run() {
        let v = ver();
        let problems = verdict("@@step prep\n@@rc 0\n", Way::Tarball, &v, false, false);
        let later = Way::Tarball.steps(false).len() - 2;
        assert_eq!(
            problems,
            [format!("install and the {later} steps after it didn't run")]
        );
        // A step skipped in the middle is named on its own.
        let log = "@@step prep\n@@rc 0\n@@step login-path\n@@rc 1\n";
        let problems = verdict(log, Way::Tarball, &v, false, false);
        assert_eq!(problems[0], "install: didn't run");
        // Emulated, the first-run screen isn't wanted.
        let tui = "@@step tui-first-run\n[exited]\nrlord exited: 0\n@@rc 0\n";
        let problems = verdict(tui, Way::Tarball, &v, false, false);
        assert!(problems.iter().any(|p| p.starts_with("tui-first-run:")));
        let problems = verdict(tui, Way::Tarball, &v, false, true);
        assert!(!problems.iter().any(|p| p.starts_with("tui-first-run")));
    }
}
