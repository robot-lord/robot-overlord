//! The one place that knows rlord's version rules, release targets and
//! asset names. The release, the build, the Homebrew tap, the version-bump
//! check and the release workflow's preflight all read them from here.

use std::cmp::Ordering;
use std::fmt;

use anyhow::{Context, Result};

pub const REPO: &str = "robot-lord/robot-overlord";
pub const TAP: &str = "robot-lord/homebrew-tap";
/// The package whose version Cargo.lock must agree with.
pub const PACKAGE: &str = "robot-overlord";
/// Publish order: dependencies first.
pub const CRATES: [&str; 4] = [
    "robot-overlord-core",
    "robot-overlord-daemon",
    "robot-overlord-tui",
    "robot-overlord",
];

/// A platform rlord is released for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub triple: &'static str,
    /// Architecture as Debian and RPM name it (Linux targets only).
    pub deb_arch: &'static str,
    pub rpm_arch: &'static str,
}

pub const MAC: Target = Target {
    triple: "aarch64-apple-darwin",
    deb_arch: "",
    rpm_arch: "",
};

pub const LINUX: [Target; 2] = [
    Target {
        triple: "x86_64-unknown-linux-musl",
        deb_arch: "amd64",
        rpm_arch: "x86_64",
    },
    Target {
        triple: "aarch64-unknown-linux-musl",
        deb_arch: "arm64",
        rpm_arch: "aarch64",
    },
];

/// A workspace version: `X.Y.Z` (stable) or `X.Y.Z-dev.N` (development
/// build). Ordered as semver orders them: a pre-release sorts below its
/// release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    raw: String,
    core: [u64; 3],
    pre: Option<String>,
}

impl Version {
    pub fn parse(s: &str) -> Result<Self> {
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let nums: Vec<&str> = core.split('.').collect();
        let bad = || anyhow::anyhow!("{s:?} is not a version (X.Y.Z or X.Y.Z-dev.N)");
        // Numbers are digits only (u64's parser would also take a `+`)
        // with no leading zero, as semver says, so versions that compare
        // equal are the same text.
        let number = |n: &str| {
            let digits = !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
            digits && (n == "0" || !n.starts_with('0'))
        };
        // The one pre-release form rlord uses, `<word>.<N>` (dev.8, rc.1):
        // for it cargo-deb writes the `~` that `package_version` predicts
        // (it rewrites other forms differently).
        let pre_ok = |p: &str| {
            p.split_once('.').is_some_and(|(word, n)| {
                !word.is_empty() && word.bytes().all(|b| b.is_ascii_lowercase()) && number(n)
            })
        };
        if nums.len() != 3 || !nums.iter().all(|n| number(n)) || pre.is_some_and(|p| !pre_ok(p)) {
            return Err(bad());
        }
        let mut parsed = [0u64; 3];
        for (slot, n) in parsed.iter_mut().zip(&nums) {
            // Only too large a number fails here.
            *slot = n.parse().map_err(|_| bad())?;
        }
        Ok(Self {
            raw: s.to_string(),
            core: parsed,
            pre: pre.map(str::to_string),
        })
    }

    /// A development build (any pre-release suffix): macOS only, a GitHub
    /// pre-release, never on crates.io or in the Homebrew tap.
    pub fn is_dev(&self) -> bool {
        self.pre.is_some()
    }

    /// The git tag and release name: `vX.Y.Z`.
    pub fn tag(&self) -> String {
        format!("v{}", self.raw)
    }

    /// The version as distro packages write it. RPM forbids `-` in a
    /// version; `~` sorts as a pre-release. cargo-deb writes the same on its
    /// own for the one pre-release form `parse` allows (`<word>.<N>`).
    pub fn package_version(&self) -> String {
        self.raw.replace('-', "~")
    }

    /// `rlord-v<ver>-<target>.tar.gz`: rlord, install.sh, licenses.
    pub fn tarball(&self, target: Target) -> String {
        format!("{}.tar.gz", self.tarball_stem(target))
    }

    /// The tarball's name without `.tar.gz`: also its top-level folder.
    pub fn tarball_stem(&self, target: Target) -> String {
        format!("rlord-{}-{}", self.tag(), target.triple)
    }

    pub fn deb(&self, target: Target) -> String {
        format!("rlord_{}-1_{}.deb", self.package_version(), target.deb_arch)
    }

    pub fn rpm(&self, target: Target) -> String {
        format!("rlord-{}-1.{}.rpm", self.package_version(), target.rpm_arch)
    }

    /// The three tarballs every stable release has.
    pub fn tarballs(&self) -> Vec<String> {
        std::iter::once(MAC)
            .chain(LINUX)
            .map(|t| self.tarball(t))
            .collect()
    }

    /// Every file the release workflow builds (SHA256SUMS aside).
    pub fn workflow_assets(&self) -> Vec<String> {
        let mut all = self.tarballs();
        all.extend(LINUX.iter().map(|t| self.deb(*t)));
        all.extend(LINUX.iter().map(|t| self.rpm(*t)));
        all
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => cmp_pre(a, b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Semver pre-release order: dot-separated identifiers, numbers compared
/// as numbers and below words, a shorter list below a longer one.
fn cmp_pre(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.split('.'), b.split('.'));
    loop {
        match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                // Digits only, as in `parse` (u64's parser takes a `+`).
                let num = |s: &str| {
                    s.bytes()
                        .all(|b| b.is_ascii_digit())
                        .then(|| s.parse::<u64>().ok())
                        .flatten()
                };
                let ord = match (num(x), num(y)) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// The `(checksum, file name)` pairs a SHA256SUMS file lists. Lines are
/// `<sum>  <file>`; a binary-mode `*` before the name is dropped.
pub fn sha256sums(text: &str) -> Vec<(&str, &str)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let sum = fields.next()?;
            let file = fields.next()?;
            Some((sum, file.strip_prefix('*').unwrap_or(file)))
        })
        .collect()
}

/// `[workspace.package] version` from the text of the root Cargo.toml.
pub fn workspace_version(cargo_toml: &str) -> Option<String> {
    let mut lines = cargo_toml.lines();
    lines.find(|l| l.starts_with("[workspace.package]"))?;
    for line in lines {
        if line.starts_with('[') {
            break;
        }
        if let Some(v) = line
            .strip_prefix("version = \"")
            .and_then(|rest| rest.strip_suffix('"'))
        {
            return Some(v.to_string());
        }
    }
    None
}

/// The version Cargo.lock records for the `robot-overlord` package.
pub fn lock_version(cargo_lock: &str) -> Option<String> {
    let mut lines = cargo_lock.lines();
    let name = format!("name = \"{PACKAGE}\"");
    lines.find(|l| *l == name)?;
    let v = lines.next()?.strip_prefix("version = \"")?;
    Some(v.strip_suffix('"')?.to_string())
}

/// The workspace version of the checkout (the current directory is the
/// repository root).
pub fn current() -> Result<Version> {
    let text = std::fs::read_to_string("Cargo.toml").context("reading Cargo.toml")?;
    let raw = workspace_version(&text).context("no version in Cargo.toml")?;
    Version::parse(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parses_stable_and_dev() {
        assert!(!v("1.2.3").is_dev());
        assert!(v("0.23.0-dev.8").is_dev());
        assert_eq!(v("0.23.0-dev.8").to_string(), "0.23.0-dev.8");
        for bad in [
            "",
            "1.2",
            "1.2.3.4",
            "1.x.3",
            "1.2.3-",
            "v1.2.3",
            "01.2.3",
            "1.2.3-dev.01",
            "+1.2.3",
            "1.2.3-dev.",
            "1.2.3-dev..1",
            "1.2.3-dev.1+abc",
            "1.2.3-dev_1",
            "1.2.3-dev.+1",
            "1.2.3-dev",
            "1.2.3-1",
            "1.2.3-dev.1.2",
            "1.2.3-rc-1.1",
            "1.2.3-Dev.1",
        ] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn orders_like_semver() {
        let ascending = [
            "0.18.0",
            "0.18.1-dev.1",
            "0.18.1-dev.2",
            "0.18.1-dev.10",
            "0.18.1",
            "0.19.0-dev.1",
            "0.19.0",
            "0.100.0",
            "1.0.0-dev.1",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for (i, a) in ascending.iter().enumerate() {
            for (j, b) in ascending.iter().enumerate() {
                assert_eq!(v(a).cmp(&v(b)), i.cmp(&j), "{a} vs {b}");
            }
        }
    }

    #[test]
    fn pre_release_identifiers() {
        assert!(v("1.0.0-dev.9") < v("1.0.0-dev.10"));
        assert!(v("1.0.0-dev.10") < v("1.0.0-rc.1"));
    }

    #[test]
    fn tag_and_asset_names() {
        let s = v("1.0.0");
        assert_eq!(s.tag(), "v1.0.0");
        assert_eq!(s.tarball(MAC), "rlord-v1.0.0-aarch64-apple-darwin.tar.gz");
        assert_eq!(
            s.tarball_stem(LINUX[0]),
            "rlord-v1.0.0-x86_64-unknown-linux-musl"
        );
        assert_eq!(s.deb(LINUX[0]), "rlord_1.0.0-1_amd64.deb");
        assert_eq!(s.deb(LINUX[1]), "rlord_1.0.0-1_arm64.deb");
        assert_eq!(s.rpm(LINUX[0]), "rlord-1.0.0-1.x86_64.rpm");
        assert_eq!(s.rpm(LINUX[1]), "rlord-1.0.0-1.aarch64.rpm");
        assert_eq!(
            s.workflow_assets(),
            [
                "rlord-v1.0.0-aarch64-apple-darwin.tar.gz",
                "rlord-v1.0.0-x86_64-unknown-linux-musl.tar.gz",
                "rlord-v1.0.0-aarch64-unknown-linux-musl.tar.gz",
                "rlord_1.0.0-1_amd64.deb",
                "rlord_1.0.0-1_arm64.deb",
                "rlord-1.0.0-1.x86_64.rpm",
                "rlord-1.0.0-1.aarch64.rpm",
            ]
        );
    }

    #[test]
    fn packages_write_a_pre_release_with_a_tilde() {
        let d = v("0.23.0-dev.8");
        assert_eq!(d.package_version(), "0.23.0~dev.8");
        assert_eq!(d.deb(LINUX[1]), "rlord_0.23.0~dev.8-1_arm64.deb");
        assert_eq!(d.rpm(LINUX[0]), "rlord-0.23.0~dev.8-1.x86_64.rpm");
        // The tarball keeps the tag as it is.
        assert_eq!(
            d.tarball(MAC),
            "rlord-v0.23.0-dev.8-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn reads_sha256sums() {
        let sums =
            "aaa  rlord-v1.0.0-aarch64-apple-darwin.tar.gz\nbbb *rlord_1.0.0-1_amd64.deb\n\n";
        assert_eq!(
            sha256sums(sums),
            [
                ("aaa", "rlord-v1.0.0-aarch64-apple-darwin.tar.gz"),
                ("bbb", "rlord_1.0.0-1_amd64.deb")
            ]
        );
    }

    #[test]
    fn reads_the_workspace_version() {
        let toml = "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.23.0-dev.8\"\nedition = \"2021\"\n\n[workspace.dependencies]\nversion = \"9.9.9\"\n";
        assert_eq!(workspace_version(toml).as_deref(), Some("0.23.0-dev.8"));
        // A version outside [workspace.package] doesn't count.
        let other = "[package]\nversion = \"1.0.0\"\n[workspace.package]\nedition = \"2021\"\n[x]\nversion = \"2.0.0\"\n";
        assert_eq!(workspace_version(other), None);
    }

    #[test]
    fn reads_the_lock_version() {
        let lock = "[[package]]\nname = \"robot-overlord-core\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"robot-overlord\"\nversion = \"0.23.0-dev.8\"\ndependencies = []\n";
        assert_eq!(lock_version(lock).as_deref(), Some("0.23.0-dev.8"));
        assert_eq!(lock_version("[[package]]\nname = \"other\"\n"), None);
    }

    #[test]
    fn this_checkout_has_a_valid_version() {
        let toml = include_str!("../../Cargo.toml");
        let lock = include_str!("../../Cargo.lock");
        let ver = workspace_version(toml).expect("workspace version");
        Version::parse(&ver).expect("valid version");
        assert_eq!(lock_version(lock), Some(ver));
    }
}
