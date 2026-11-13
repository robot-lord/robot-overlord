//! `cargo xtask release [--local [--skip-smoke] | --resume] [--dry-run]`:
//! publish rlord as a GitHub release from the merged main.
//!
//! The version in Cargo.toml decides the kind of release:
//!
//! - `X.Y.Z-dev.N`, a development build: built here on the maintainer's
//!   Apple Silicon Mac, macOS only, a GitHub pre-release. What ordinary PRs
//!   ship; `[update] channel = "dev"` installs them.
//! - `X.Y.Z`, a stable release: built in GitHub Actions
//!   (.github/workflows/release.yml). This pushes the tag, waits for the
//!   run, checks the release assets against the CI-made SHA256SUMS and
//!   provenance attestations, publishes the draft release CI made, then
//!   does the steps that still need local credentials: the Homebrew formula
//!   and crates.io.
//!
//! Options:
//!
//! - `--local`: stable only, the fallback when Actions is down. Build macOS
//!   and Linux here (the same `build-release` CI runs), smoke-test in
//!   Docker, and create the release from here. The tag push that creates
//!   also starts release.yml; its preflight sees the published release and
//!   skips the build.
//! - `--skip-smoke`: with `--local`, skip the Docker smoke test.
//! - `--resume`: stable only. The tag is already pushed (an earlier run
//!   stopped partway). Skip tagging; watch the tag's workflow run, verify,
//!   publish the draft (if still a draft), then do the tap and crates.io.
//! - `--dry-run`: publish nothing. The refusals below become warnings and
//!   the steps that would tag, upload or publish are printed instead. A dev
//!   build or `--local` still builds here (and `--local` smoke-tests); a
//!   stable release built in CI stops after the checks, since its build is
//!   the workflow's (rehearse that with a dry run of release.yml).
//!
//! Assets: rlord-v<ver>-<target>.tar.gz (rlord, install.sh, licenses),
//! SHA256SUMS; stable also rlord_<ver>-1_<arch>.deb and
//! rlord-<ver>-1.<arch>.rpm.
//!
//! Needs: gh; push access to robot-lord/homebrew-tap and `cargo login` for
//! a stable release. `--local` also needs what `build-release linux` needs
//! and Docker (colima) for the smoke test.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::build::{self, Platform};
use crate::cmd::{self, gh, git, TempDir};
use crate::version::{self, Version, REPO, TAP};
use crate::{publish, tap};

const USAGE: &str = "usage: cargo xtask release [--local [--skip-smoke] | --resume] [--dry-run]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Options {
    local: bool,
    resume: bool,
    smoke: bool,
    dry_run: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut o = Options {
            local: false,
            resume: false,
            smoke: true,
            dry_run: false,
        };
        for a in args {
            match a.as_str() {
                "--skip-smoke" => o.smoke = false,
                "--local" => o.local = true,
                "--resume" => o.resume = true,
                "--dry-run" => o.dry_run = true,
                other => return cmd::usage(format!("unknown option {other}\n{USAGE}")),
            }
        }
        Ok(o)
    }
}

/// How a release is made, from the version and the options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A development build: macOS, built and uploaded from here.
    Dev,
    /// A stable release built here (`--local`).
    StableLocal { smoke: bool },
    /// A stable release built by the workflow; `resume` skips the tag push.
    StableCi { resume: bool },
}

impl Mode {
    fn builds_here(self) -> bool {
        !matches!(self, Mode::StableCi { .. })
    }
}

/// The mode, or why the options don't fit the version. `Ok` also carries a
/// note to print when an option is ignored.
fn mode(dev: bool, o: Options) -> Result<(Mode, Option<&'static str>), &'static str> {
    if dev && o.local {
        return Err("--local is for stable releases; dev builds are always local");
    }
    if dev && o.resume {
        return Err("--resume is for stable releases");
    }
    if o.local && o.resume {
        return Err("--local and --resume don't mix");
    }
    if dev {
        let note = (!o.smoke).then_some("--skip-smoke: dev builds have no smoke test; ignored");
        return Ok((Mode::Dev, note));
    }
    if o.local {
        return Ok((Mode::StableLocal { smoke: o.smoke }, None));
    }
    if !o.smoke {
        return Err("--skip-smoke only applies with --local (CI always smoke-tests)");
    }
    Ok((Mode::StableCi { resume: o.resume }, None))
}

pub fn main(args: &[String]) -> Result<()> {
    let opts = Options::parse(args)?;
    let ver = version::current()?;
    let tag = ver.tag();
    let (mode, note) = match mode(ver.is_dev(), opts) {
        Ok(m) => m,
        Err(why) => return cmd::usage(why),
    };
    if let Some(note) = note {
        eprintln!("{note}");
    }

    // Every mode runs macOS tools later (shasum, codesign): check now,
    // before anything is tagged or pushed.
    if std::env::consts::OS != "macos" {
        bail!("release runs on macOS only (it uses shasum and codesign); on Linux, trigger the release workflow from Actions instead");
    }
    for c in ["gh", "git", "shasum"] {
        if !cmd::on_path(c) {
            bail!("release needs {c}");
        }
    }
    if mode.builds_here() && !build::is_apple_silicon_mac() {
        bail!("build from an Apple Silicon Mac");
    }

    // Refuse anything that would make the release not match a pushed
    // commit. A dry run publishes nothing, so it only warns.
    let refuse = |why: String| -> Result<()> {
        // A check cut short by Ctrl-C isn't a finding.
        cmd::check_interrupted()?;
        if opts.dry_run {
            eprintln!("warning: {why} (dry run: continuing)");
            return Ok(());
        }
        bail!(why)
    };
    if !cmd::output(git().args(["status", "--porcelain"]))?.is_empty() {
        refuse("working tree is dirty".into())?;
    }
    cmd::run(git().args(["fetch", "-q", "origin"]))?;
    let head = cmd::output(git().args(["rev-parse", "HEAD"]))?;
    // Only merged code ships: HEAD must be on origin/main, not just on some
    // pushed branch (a PR branch would burn the version before it merges).
    if !cmd::succeeds(git().args(["merge-base", "--is-ancestor", &head, "origin/main"])) {
        refuse(format!(
            "HEAD {head} is not on origin/main — release only merged code"
        ))?;
    }
    if mode == (Mode::StableCi { resume: true }) {
        // The tag must be pushed, and point at HEAD.
        let refs = cmd::output(git().args([
            "ls-remote",
            "--tags",
            "origin",
            &format!("refs/tags/{tag}^{{}}"),
            &format!("refs/tags/{tag}"),
        ]))?;
        match tagged_commit(&refs) {
            None => refuse(format!("--resume: {tag} isn't on origin"))?,
            Some(remote) if remote != head => {
                refuse(format!("--resume: {tag} is {remote}, HEAD is {head}"))?
            }
            Some(_) => {}
        }
    } else {
        let release = release_state(&tag)?;
        let pushed = tag_on_origin(&tag)?;
        if release == Some(true) && !pushed {
            // A failed earlier attempt deleted the tag but left CI's draft
            // behind.
            refuse(format!(
                "a leftover draft release {tag} exists without its tag — clean up with: gh release delete {tag} -R {REPO} --yes; then run this again"
            ))?;
        } else if release.is_some() || pushed {
            let mut why = format!("{tag} already exists — bump the version in Cargo.toml");
            if !ver.is_dev() {
                why.push_str(
                    "\n(if an earlier run pushed it and stopped: cargo xtask release --resume)",
                );
            }
            refuse(why)?;
        }
    }

    // Under the checkout, not the system temp dir: Docker on the Mac
    // (colima) only sees the home folder, and the smoke test mounts these
    // files.
    let stage = TempDir::new_in(Path::new("target"), "release")?;
    let dist = stage.path().canonicalize()?.join("dist");
    std::fs::create_dir(&dist)?;

    match mode {
        Mode::Dev | Mode::StableLocal { .. } => {
            build::build(Platform::Macos, &dist)?;
            if mode != Mode::Dev {
                build::build(Platform::Linux, &dist)?;
                // What CI's "Check assets" step checks, for a release made here.
                for asset in ver.workflow_assets() {
                    if !dist.join(&asset).is_file() {
                        bail!("the build didn't write {asset}");
                    }
                }
            }
            // Checksums before the smoke test: the installer it runs
            // verifies them.
            write_sums(&dist)?;
            if mode == (Mode::StableLocal { smoke: true }) {
                cmd::run(Command::new("scripts/smoke-linux.sh").arg(&dist))?;
            }
            let files = cmd::file_names(&dist)?;
            let mut create = gh();
            create.args(["release", "create", &tag, "-R", REPO]);
            create.args(files.iter().map(|f| dist.join(f)));
            create.args(["--generate-notes", "--title", &tag, "--target", &head]);
            if mode == Mode::Dev {
                create.arg("--prerelease");
            }
            if opts.dry_run {
                println!("dry run: built {}", files.join(", "));
                println!(
                    "{}",
                    std::fs::read_to_string(dist.join("SHA256SUMS"))?.trim_end()
                );
                println!("dry run: not run: {}", shown(&create, &dist));
                if mode != Mode::Dev {
                    println!("dry run: not run: update-tap, publish-crates");
                }
                return Ok(());
            }
            cmd::run(&mut create)?;
            if mode == Mode::Dev {
                println!("released: {}", release_url(&tag));
                return Ok(());
            }
        }
        Mode::StableCi { resume } => {
            if opts.dry_run {
                if !resume {
                    println!("dry run: not run: git tag -f {tag} {head}; git push origin refs/tags/{tag}");
                }
                println!("dry run: not run: wait for the release workflow on {tag}, check every asset against SHA256SUMS and its attestation, publish the draft, update-tap, publish-crates");
                return Ok(());
            }
            release_from_ci(&ver, &head, resume, &dist)?;
        }
    }

    tap::update(&ver, &dist.join("SHA256SUMS"), TAP)?;
    // crates.io publishes the source: only once the repo itself is public.
    let visibility = cmd::output(gh().args([
        "repo",
        "view",
        REPO,
        "--json",
        "visibility",
        "-q",
        ".visibility",
    ]))?;
    let logged_in = crates_io_token_available();
    if visibility != "PUBLIC" {
        eprintln!("crates.io: {REPO} is private — skipped (publish later with cargo xtask publish-crates)");
    } else if logged_in {
        publish::publish(false)?;
    } else {
        eprintln!("crates.io: not logged in (cargo login) — skipped");
    }
    println!("released: {}", release_url(&tag));
    Ok(())
}

/// Stable in CI: the tag push starts .github/workflows/release.yml. Wait
/// for its run, check what it built, publish its draft.
fn release_from_ci(ver: &Version, head: &str, resume: bool, dist: &Path) -> Result<()> {
    let tag = ver.tag();
    let undo = format!(
        "delete the tag and any draft release (git push origin :refs/tags/{tag}; git tag -d {tag}; gh release delete {tag} -R {REPO} --yes if it is a draft), then cargo xtask release --local"
    );
    let find_runs = || -> Result<Vec<Run>> {
        let json = cmd::output(gh().args([
            "run",
            "list",
            "-R",
            REPO,
            "-w",
            "release.yml",
            "-L",
            "50",
            "--json",
            "databaseId,displayTitle,headBranch,headSha,status,event,createdAt",
        ]))?;
        publishing_runs(&json, head, &tag)
    };
    // Runs from an earlier attempt on this commit aren't this push's run.
    let earlier: Vec<u64> = if resume {
        Vec::new()
    } else {
        find_runs()?.iter().map(|r| r.id).collect()
    };
    if !resume {
        cmd::run(git().args(["tag", "-f", &tag, head]))?;
        match cmd::run(git().args(["push", "origin", &format!("refs/tags/{tag}")])) {
            Ok(()) => {}
            // The push may or may not have reached origin.
            Err(e) if cmd::is_interrupted(&e) => {
                return Err(e.context(format!("interrupted while pushing {tag}: if `git ls-remote --tags origin {tag}` lists it, cargo xtask release --resume")));
            }
            Err(_) => {
                cmd::succeeds(git().args(["tag", "-d", &tag]));
                bail!("pushing {tag} failed (local tag deleted)");
            }
        }
    }
    println!("waiting for the release workflow on {tag}…");
    // A failed listing (a network blip) is tried again, like no run; but
    // if they keep failing, the run's state is unknown and nothing here may
    // say there is none.
    let unknown = |e: anyhow::Error| {
        if cmd::is_interrupted(&e) {
            return e;
        }
        anyhow::anyhow!("couldn't list the release workflow's runs for {tag} ({e:#}); the run may still be going — check https://github.com/{REPO}/actions before anything else, then cargo xtask release --resume")
    };
    let mut run = None;
    let mut failures = 0;
    for attempt in 0..60 {
        match find_runs() {
            Ok(runs) => {
                failures = 0;
                run = runs.into_iter().find(|r| !earlier.contains(&r.id));
            }
            Err(e) if cmd::is_interrupted(&e) => return Err(e),
            Err(e) => {
                failures += 1;
                if failures >= 5 {
                    return Err(unknown(e));
                }
            }
        }
        if run.is_some() {
            break;
        }
        if attempt < 59 {
            cmd::sleep(Duration::from_secs(5))?;
        }
    }
    let Some(run) = run else {
        // Race: the run may register just after we give up. Cancel anything
        // queued for the tag so a late run can't publish over a --local
        // release.
        cmd::sleep(Duration::from_secs(10))?;
        let mut not_cancelled = Vec::new();
        for late in find_runs().map_err(unknown)? {
            if late.status != "completed" {
                let id = late.id.to_string();
                if cmd::run(gh().args(["run", "cancel", &id, "-R", REPO])).is_err() {
                    not_cancelled.push(id);
                }
            }
        }
        cmd::check_interrupted()?;
        if !not_cancelled.is_empty() {
            bail!("no release workflow run for {tag} was found in time, and late runs {} couldn't be cancelled — cancel them in https://github.com/{REPO}/actions before anything else; then to build here: {undo}", not_cancelled.join(", "));
        }
        bail!("no release workflow run for {tag} (any late run was cancelled) — check Actions; to build here: {undo}");
    };
    let id = run.id.to_string();
    match cmd::run(gh().args(["run", "watch", &id, "-R", REPO, "--exit-status"])) {
        Ok(()) => {}
        Err(e) if cmd::is_interrupted(&e) => {
            return Err(e.context(format!("stopped watching; the workflow run goes on (https://github.com/{REPO}/actions/runs/{id}): when it finishes, cargo xtask release --resume")));
        }
        Err(_) => bail!("release workflow failed: https://github.com/{REPO}/actions/runs/{id} — fix and re-run it (gh workflow run release.yml --ref {tag} -f tag={tag} -f dry_run=false), then cargo xtask release --resume; or {undo}"),
    }
    // CI leaves a draft. Before publishing it, check every asset against
    // the CI-made SHA256SUMS, and the provenance attestation of every file
    // in it (tarballs, .deb, .rpm).
    cmd::run(
        gh().args(["release", "download", &tag, "-R", REPO, "-D"])
            .arg(dist),
    )?;
    cmd::run(
        Command::new("shasum")
            .args(["-a", "256", "-c", "SHA256SUMS"])
            .current_dir(dist),
    )?;
    let sums = std::fs::read_to_string(dist.join("SHA256SUMS"))?;
    let listed = summed_files(&sums);
    for asset in ver.workflow_assets() {
        if !listed.contains(&asset.as_str()) {
            bail!("SHA256SUMS lacks {asset}");
        }
    }
    for file in listed {
        // stdout hidden, gh's reason for a failure shown.
        cmd::output(
            gh().args(["attestation", "verify"])
                .arg(dist.join(file))
                .args(["-R", REPO]),
        )
        .with_context(|| format!("provenance check failed for {file}"))?;
    }
    // A failed lookup stops here: it must not read as "already published"
    // and go on to the tap and crates.io.
    let draft = match release_state(&tag)? {
        Some(draft) => draft,
        None => bail!("{tag} has no GitHub release after its workflow run"),
    };
    if draft {
        cmd::run(gh().args([
            "release",
            "edit",
            &tag,
            "-R",
            REPO,
            "--draft=false",
            "--latest",
        ]))?;
    }
    Ok(())
}

/// Whether cargo has a crates.io token: CARGO_REGISTRY_TOKEN, or a
/// credentials file in CARGO_HOME (default ~/.cargo). Existence only: the
/// token itself is never read.
fn crates_io_token_available() -> bool {
    if std::env::var_os("CARGO_REGISTRY_TOKEN").is_some_and(|t| !t.is_empty()) {
        return true;
    }
    let home = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cargo")));
    home.is_some_and(|h| {
        ["credentials.toml", "credentials"]
            .iter()
            .any(|f| h.join(f).is_file())
    })
}

fn release_url(tag: &str) -> String {
    format!("https://github.com/{REPO}/releases/tag/{tag}")
}

/// The tag's GitHub release: `None` if there is none, else whether it is a
/// draft. Any other failure of gh (auth, network) is an error, never "no
/// release".
fn release_state(tag: &str) -> Result<Option<bool>> {
    cmd::check_interrupted()?;
    let out = gh()
        .args([
            "release", "view", tag, "-R", REPO, "--json", "isDraft", "-q", ".isDraft",
        ])
        .output()
        .context("couldn't start gh release view")?;
    cmd::check_interrupted()?;
    if out.status.success() {
        return match String::from_utf8_lossy(&out.stdout).trim() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            other => bail!("gh release view {tag}: unexpected {other:?}"),
        };
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("release not found") {
        return Ok(None);
    }
    bail!("gh release view {tag} failed: {}", err.trim())
}

/// Whether the tag is on origin. `ls-remote --exit-code` exits 2 for no
/// match; anything else but 0 is an error, never "not pushed".
fn tag_on_origin(tag: &str) -> Result<bool> {
    cmd::check_interrupted()?;
    let status = git()
        .args(["ls-remote", "--exit-code", "--tags", "origin", tag])
        .stdout(std::process::Stdio::null())
        .status()
        .context("couldn't start git ls-remote")?;
    cmd::check_interrupted()?;
    match status.code() {
        Some(0) => Ok(true),
        Some(2) => Ok(false),
        _ => bail!("git ls-remote origin {tag} failed ({status})"),
    }
}

/// SHA256SUMS for every file in `dist`.
fn write_sums(dist: &Path) -> Result<()> {
    let files: Vec<String> = cmd::file_names(dist)?
        .into_iter()
        .filter(|f| f != "SHA256SUMS")
        .collect();
    if files.is_empty() {
        bail!("nothing was built into {}", dist.display());
    }
    let sums = cmd::output(
        Command::new("shasum")
            .args(["-a", "256", "--"])
            .args(&files)
            .current_dir(dist)
            .stdin(std::process::Stdio::null()),
    )?;
    std::fs::write(dist.join("SHA256SUMS"), sums + "\n").context("writing SHA256SUMS")
}

/// A command as text, with the scratch folder shortened.
fn shown(cmd: &Command, dist: &Path) -> String {
    let prefix = format!("{}/", dist.display());
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args() {
        s.push(' ');
        s.push_str(&a.to_string_lossy().replace(&prefix, "dist/"));
    }
    s
}

/// The commit a tag points at, from `git ls-remote --tags origin
/// refs/tags/T^{} refs/tags/T`: the peeled line when the tag is annotated
/// (it sorts last), else the tag's own.
fn tagged_commit(ls_remote: &str) -> Option<&str> {
    ls_remote.lines().last()?.split_whitespace().next()
}

/// The file names SHA256SUMS lists.
fn summed_files(sums: &str) -> Vec<&str> {
    version::sha256sums(sums)
        .into_iter()
        .map(|(_, file)| file)
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct Run {
    id: u64,
    status: String,
}

/// Runs that can publish this tag, newest first, from `gh run list
/// --json`. Matches the run-name in release.yml ("release vX.Y.Z", never a
/// "(dry run)") for a push or a re-run dispatched with `--ref vX.Y.Z -f
/// tag=vX.Y.Z`, plus push runs on the tag. Never picks a dry run.
fn publishing_runs(json: &str, head: &str, tag: &str) -> Result<Vec<Run>> {
    let runs: Vec<serde_json::Value> = serde_json::from_str(json).context("reading gh run list")?;
    let title = format!("release {tag}");
    let text = |run: &serde_json::Value, key: &str| run[key].as_str().unwrap_or("").to_string();
    let mut found: Vec<(String, Run)> = runs
        .iter()
        .filter(|r| {
            let shown = text(r, "displayTitle");
            text(r, "headSha") == head
                && (shown == title || (text(r, "event") == "push" && text(r, "headBranch") == tag))
                && !shown.ends_with("(dry run)")
        })
        .filter_map(|r| {
            let run = Run {
                id: r["databaseId"].as_u64()?,
                status: text(r, "status"),
            };
            Some((text(r, "createdAt"), run))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(found.into_iter().map(|(_, run)| run).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(flags: &[&str]) -> Options {
        let args: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        Options::parse(&args).unwrap()
    }

    #[test]
    fn unknown_option_is_a_usage_error() {
        let err = Options::parse(&["--fast".to_string()]).unwrap_err();
        assert!(err.downcast_ref::<cmd::Usage>().is_some());
    }

    #[test]
    fn the_version_and_options_pick_the_mode() {
        assert_eq!(mode(true, opts(&[])), Ok((Mode::Dev, None)));
        assert_eq!(mode(true, opts(&["--dry-run"])), Ok((Mode::Dev, None)));
        assert_eq!(
            mode(false, opts(&[])),
            Ok((Mode::StableCi { resume: false }, None))
        );
        assert_eq!(
            mode(false, opts(&["--resume"])),
            Ok((Mode::StableCi { resume: true }, None))
        );
        assert_eq!(
            mode(false, opts(&["--local"])),
            Ok((Mode::StableLocal { smoke: true }, None))
        );
        assert_eq!(
            mode(false, opts(&["--local", "--skip-smoke"])),
            Ok((Mode::StableLocal { smoke: false }, None))
        );
        assert!(Mode::Dev.builds_here());
        assert!(Mode::StableLocal { smoke: true }.builds_here());
        assert!(!Mode::StableCi { resume: false }.builds_here());
    }

    #[test]
    fn options_that_do_not_fit_are_refused() {
        assert!(mode(true, opts(&["--local"])).is_err());
        assert!(mode(true, opts(&["--resume"])).is_err());
        assert!(mode(false, opts(&["--local", "--resume"])).is_err());
        assert!(mode(false, opts(&["--skip-smoke"])).is_err());
        assert!(mode(false, opts(&["--resume", "--skip-smoke"])).is_err());
        // A dev build has no smoke test to skip: noted, not refused.
        let (m, note) = mode(true, opts(&["--skip-smoke"])).unwrap();
        assert_eq!(m, Mode::Dev);
        assert!(note.is_some());
    }

    #[test]
    fn picks_the_runs_that_can_publish_the_tag() {
        let json = r#"[
          {"databaseId": 1, "displayTitle": "release v1.0.0", "headBranch": "v1.0.0", "headSha": "abc", "status": "completed", "event": "push", "createdAt": "2026-11-12T10:00:00Z"},
          {"databaseId": 2, "displayTitle": "release v1.0.0", "headBranch": "v1.0.0", "headSha": "abc", "status": "in_progress", "event": "workflow_dispatch", "createdAt": "2026-11-12T11:00:00Z"},
          {"databaseId": 3, "displayTitle": "release v1.0.0 (dry run)", "headBranch": "main", "headSha": "abc", "status": "queued", "event": "workflow_dispatch", "createdAt": "2026-11-12T12:00:00Z"},
          {"databaseId": 4, "displayTitle": "release v1.0.0", "headBranch": "v1.0.0", "headSha": "other", "status": "queued", "event": "push", "createdAt": "2026-11-12T13:00:00Z"},
          {"databaseId": 5, "displayTitle": "release v0.9.0", "headBranch": "v0.9.0", "headSha": "abc", "status": "completed", "event": "push", "createdAt": "2026-11-12T14:00:00Z"},
          {"databaseId": 6, "displayTitle": "renamed", "headBranch": "v1.0.0", "headSha": "abc", "status": "queued", "event": "push", "createdAt": "2026-11-12T09:00:00Z"}
        ]"#;
        let runs = publishing_runs(json, "abc", "v1.0.0").unwrap();
        let ids: Vec<u64> = runs.iter().map(|r| r.id).collect();
        assert_eq!(ids, [2, 1, 6]);
        assert_eq!(runs[0].status, "in_progress");
        assert!(publishing_runs("[]", "abc", "v1.0.0").unwrap().is_empty());
        assert!(publishing_runs("not json", "abc", "v1.0.0").is_err());
    }

    #[test]
    fn a_tag_resolves_to_its_commit() {
        assert_eq!(tagged_commit("abc\trefs/tags/v1.0.0\n"), Some("abc"));
        // Annotated: the peeled commit comes last.
        assert_eq!(
            tagged_commit("tagobj\trefs/tags/v1.0.0\ncommit\trefs/tags/v1.0.0^{}"),
            Some("commit")
        );
        assert_eq!(tagged_commit(""), None);
    }

    #[test]
    fn lists_the_files_in_sha256sums() {
        let sums = "aaa  rlord-v1.0.0-aarch64-apple-darwin.tar.gz\nbbb  rlord_1.0.0-1_amd64.deb\n";
        assert_eq!(
            summed_files(sums),
            [
                "rlord-v1.0.0-aarch64-apple-darwin.tar.gz",
                "rlord_1.0.0-1_amd64.deb"
            ]
        );
    }
}
