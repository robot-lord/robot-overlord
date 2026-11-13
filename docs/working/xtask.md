**Status:** in progress

# Release automation in `cargo xtask`

Move rlord's release automation out of bash into an `xtask` crate. The
user's rule: in a Rust project, automation with real logic (parsing JSON
or output, retries, state, many branches) is Rust, run as
`cargo xtask <task>`; shell is for simple glue; scripts end users run on a
bare machine stay POSIX `sh`.

Worktree: `.worktrees/local-release-upload/.worktrees/xtask`, branch
`xtask-release` (from `origin/main` at 6d64c4c, 0.23.0-dev.7). Nothing is
pushed yet; there is no PR.

This doc is the hand-over: the code was written in one pass and is only
checked locally by unit tests, clippy and fmt. Everything under "Left to
do" is still open. Read `CLAUDE.md` first and follow it (version bumps,
review, releases, Dependabot rule, docs layout).

## Done (commit 28ab5c2, WIP)

- `xtask` crate added to the workspace (`publish = false`, version
  `0.0.0`, deps `anyhow` and `serde_json` from the workspace), alias in
  `.cargo/config.toml` (`xtask = "run --package xtask --"`).
- Tasks written, each a port of one script:

  | task | replaces | file |
  |---|---|---|
  | `release [--local [--skip-smoke] \| --resume] [--dry-run]` | `scripts/release.sh` | `xtask/src/release.rs` |
  | `build-release macos\|linux DIST` | `scripts/build-release.sh` | `xtask/src/build.rs` |
  | `check-version-bump [BASE] [REV]` | `scripts/check-version-bump.sh` | `xtask/src/bump.rs` |
  | `update-tap VERSION SHA256SUMS [TAP_REPO]` | `scripts/update-tap.sh` | `xtask/src/tap.rs` |
  | `publish-crates [--dry-run]` | `scripts/publish-crates.sh` | `xtask/src/publish.rs` |
  | `release-preflight` | the inline bash in release.yml's `preflight` job | `xtask/src/preflight.rs` |

- The five scripts are deleted (`git rm`), no wrappers left.
- `.github/workflows/release.yml`, `.github/workflows/ci.yml`,
  `.githooks/pre-push` call xtask.
- `CLAUDE.md` (release section, new "Automation lives in `cargo xtask`"
  section), `CONTRIBUTING.md`, `docs/working/public-launch.md` (new Road
  to 1.0 item 4, marked done; the "one helper for the version and target
  list" owed item dropped), `docs/working/linux-and-packaging.md`,
  comments in `crates/rlord/Cargo.toml` and `scripts/install.sh`.
- Version bumped to `0.23.0-dev.8`, `Cargo.lock` regenerated.
- Passing locally: `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test -p xtask` (24
  tests). `cargo xtask check-version-bump` and the usage/exit-2 path were
  run by hand.

Not yet run: `cargo build` and `cargo test` for the whole workspace after
the last edits, any `build-release`, `release --dry-run`, the release
workflow, the CI workflow.

## Design decisions

### Module layout

- `main.rs`: dispatch. Changes directory to the repository root (from
  `CARGO_MANIFEST_DIR`) before any task, as every script did with
  `cd "$(dirname "$0")/.."`. Usage errors (`cmd::Usage`) exit 2, other
  errors exit 1, as the scripts did. No `process::exit` inside tasks:
  errors return to `main` so the `Drop` guards (scratch folders, manifest
  restore) run.
- `version.rs`: the shared version/targets helper (below).
- `cmd.rs`: `run` (terminal attached, error on failure), `succeeds`
  (silent, bool), `output` (stdout, trimmed), `output_quiet`; `git()`,
  `gh()`, `cargo()`; `on_path`; `TempDir` (removed on drop);
  `file_names`. `cargo()` strips `CARGO_PKG_*` and `CARGO_MANIFEST_*`
  from the environment: `cargo run` sets them for xtask, and a nested
  cargo or plugin (cargo-deb, cargo-generate-rpm) must not mistake
  xtask's package for its own. This is a precaution, not a seen bug; the
  workflow dry run is what proves the Linux packaging.
- No clap: arguments are parsed by hand, so the CI jobs that only need
  the version check compile two small dependencies.
- Pure logic is split from the commands so it can be unit-tested:
  `release::mode` (version + options to dev / stable-local / stable-CI,
  with the same refusals as the script), `release::publishing_runs` (the
  old jq filter over `gh run list --json`), `release::tagged_commit`,
  `release::summed_files`, `tap::formula`, `tap::checksum`,
  `publish::pin_internal_deps` (the old `sed -E`), `bump::check`,
  `preflight::problems`.

### Shared version/targets helper (`xtask/src/version.rs`)

The one place for: `REPO`, `TAP`, `PACKAGE`, the crates.io publish order
(`CRATES`), the targets (`MAC`, `LINUX`, each with its deb and rpm
architecture), `Version` (parse, `is_dev`, `tag`, semver ordering,
`package_version` with `~` for a pre-release), the asset names
(`tarball`, `tarball_stem`, `deb`, `rpm`, `tarballs`, `workflow_assets`),
and reading the version from `Cargo.toml` (`workspace_version`) and
`Cargo.lock` (`lock_version`).

- Ordering is semver precedence written by hand (no `semver` dep). The
  script used `sort -t. -k…n` and `sort -V`; the results match for every
  `X.Y.Z` and `X.Y.Z-dev.N` form, which is all the repo uses.
- release.yml no longer repeats names: `release-preflight` writes
  `assets=` (the seven files the builds must produce) to `GITHUB_OUTPUT`,
  and the `release` job, which deliberately has no checkout and runs no
  repo code with its write token, loops over `$ASSETS`.
- Still repeated, by necessity: the `targets:` inputs of
  `dtolnay/rust-toolchain` in release.yml, and `on.push.tags`.

### Scripts: deleted, not wrapped

All five are deleted. Every caller is in the repo and updated in the same
change (workflows, hook, CLAUDE.md, CONTRIBUTING.md), so wrappers would
only be extra files. Left as shell:

- `scripts/get.sh`, `scripts/install.sh`, `crates/rlord/update.sh`
  (`scripts/update.sh` is a symlink to it): run by end users on a bare
  machine.
- `scripts/smoke-linux.sh` (called by `release --local` and release.yml),
  `scripts/dev-daemon.sh`, `scripts/go-public.sh`,
  `scripts/install-launchd.sh`, `scripts/install-systemd.sh`,
  `scripts/demo/`: out of scope for this change. `smoke-linux.sh` and
  `dev-daemon.sh` hold enough logic to be candidates later; recorded as
  owed in `public-launch.md`.

### Behaviour kept from `release.sh`

- Dev (`X.Y.Z-dev.N`): build macOS here, `SHA256SUMS`,
  `gh release create … --generate-notes --title vX --target HEAD
  --prerelease`, done (no tap, no crates.io).
- Stable in CI: `git tag -f`, push (local tag deleted if the push fails),
  poll `gh run list` 60 times 5 s apart for the run, `gh run watch
  --exit-status`, download, `shasum -a 256 -c`, the three tarballs must
  be in `SHA256SUMS`, `gh attestation verify` on every file listed,
  publish the draft with `--draft=false --latest` if still a draft, tap,
  crates.io. No run found: wait 10 s, cancel any non-completed run for
  the tag, fail with the undo instructions.
- `--resume`: tag must be on origin and point at HEAD; skips tagging.
- `--local`: build macOS + Linux here, sums, `scripts/smoke-linux.sh`
  unless `--skip-smoke`, create the release, tap, crates.io.
- Refusals: not macOS; `gh`/`git`/`shasum` missing; not Apple Silicon
  when building here; dirty tree; HEAD not on `origin/main`; leftover
  draft without its tag; tag or release already exists.
- crates.io only when the repo is PUBLIC and
  `~/.cargo/credentials.toml` exists (existence only; never read it).
- Scratch folder under `target/` (Docker on the Mac only sees the home
  folder).

### Differences from the scripts (intended)

- `release --dry-run` is new: refusals about the tree, HEAD and an
  existing tag become warnings; a local build (and the smoke test for
  `--local`) still runs; the `gh release create` command is printed, not
  run; the stable CI path prints what it would do and stops. Nothing is
  tagged, pushed, uploaded or published.
- `publish-crates` restores the manifests by writing back what it read,
  not `git checkout`, so it no longer discards other edits to them. A
  `Drop` guard does it, which does not run on Ctrl-C (the bash `trap`
  did); the module comment says how to put them back.
- A failed `gh run list` while polling counts as "no run yet" and is
  retried, where `set -e` would have aborted.
- The generated formula's first comment line names
  `cargo xtask update-tap`.
- `update-tap`'s `SHA256SUMS` argument is resolved against the directory
  the command was run from, as before, although xtask itself works from
  the root.
- release.yml's `preflight` job now installs the Rust toolchain
  (`dtolnay/rust-toolchain`, same pinned SHA as the other jobs) and runs
  repo code with the read-only `GH_TOKEN`. The `release` job fails if
  `ASSETS` is empty.
- `.githooks/pre-push` skips branches without `xtask/src/bump.rs` (was:
  without the script). It compiles xtask on first use.

## Left to do, in order

1. **Read the diff critically against the deleted scripts.** They are in
   git: `git show origin/main:scripts/release.sh` and the other four.
   Check each task line by line for a dropped step or changed flag.
   Things worth a second look:
   - `release.rs`: the order of the refusal checks; the
     `refuse` closure in a dry run with `--resume`; `write_sums` (file
     list sorted, `SHA256SUMS` itself excluded, trailing newline);
     `gh release create` argument order (files before flags, as before).
   - `build.rs`: `--output`/`-o` get `DIST/` with the trailing slash;
     the `-s 'version = "X~dev.N"'` override only for a dev version.
   - `preflight.rs`: `tag` and `assets` are written even when the checks
     fail; `skip=true` only on a real run whose release is published.
   - `bump.rs`: compares against `git merge-base BASE REV`, as before.
2. **Full local checks:** `cargo build`, `cargo clippy --all-targets --
   -D warnings`, `cargo test`, `cargo fmt --check`.
3. **Local dev path, publishing nothing:** from the branch run
   `cargo xtask release --dry-run`. Expect warnings (HEAD not on
   origin/main), a macOS build, the tarball
   `rlord-v0.23.0-dev.8-aarch64-apple-darwin.tar.gz` and `SHA256SUMS`
   listed, and the unrun `gh release create … --prerelease` line. Also
   `cargo xtask build-release macos <scratch dir>` and check the tarball
   holds `rlord`, `install.sh`, `LICENSE-MIT`, `LICENSE-APACHE`,
   `README.md` under `rlord-v0.23.0-dev.8-aarch64-apple-darwin/`.
   `cargo xtask publish-crates --dry-run` must refuse a dev version.
   Do not publish a release from the branch.
4. **Clean up the commits** if wanted (the WIP commit can stay; history
   is squashed on merge), push the branch. The pre-push hook, if enabled,
   now runs `cargo xtask check-version-bump`.
5. **Release workflow dry run:**
   `gh workflow run release.yml --ref xtask-release -f dry_run=true`
   (inputs: `tag`, default empty; `dry_run`, default true). Watch it to
   green: `preflight` (warnings expected for a dev version on a branch),
   `build-macos`, `build-linux`, both `smoke-linux` jobs, and `release`
   (its "Check assets" step proves the names from `version.rs` match
   what cargo-deb and cargo-generate-rpm write, including `~` for the
   dev version). Fix forward until green.
6. **Open the PR** (one PR for all of this). Body: what moved where, the
   intended differences above, that the five scripts are gone, the new
   CLAUDE.md rule, what was verified. End the body with:

   ```
   🤖 Generated with [Claude Code](https://claude.com/claude-code)

   https://claude.ai/code/session_01FbNBpvn9vQnbzPcQogYHjn
   ```

   Commit trailers:

   ```
   Co-Authored-By: Claude Fable 5.2 <noreply@anthropic.com>
   Claude-Session: https://claude.ai/code/session_01FbNBpvn9vQnbzPcQogYHjn
   ```

   (Use the implementing agent's own model name in `Co-Authored-By` if
   its session says so.) Check CI on the PR: `version-bump` now runs
   `cargo xtask check-version-bump`, `check`, `advisories`.
7. **Dependabot, after opening the PR and after each later push**
   (CLAUDE.md "Dependency updates"): launch a subagent (sonnet: routine,
   well-specified work) that resolves open Dependabot PRs in
   `robot-lord/robot-overlord` and `robot-lord/website`. It must stay
   below `0.23.0-dev.8`, or take the next version after this PR merges.
   When this was written there were no open PRs in robot-overlord.
8. **Review:** run `/code-review <PR>` with the Skill tool and wait for
   the skill's own final result (it can fork to the background and
   report late; your own read is not the review). Fix what is real,
   push, and post one PR comment per round starting
   `_Posted by a Claude agent working on this PR._` with one line per
   finding: fixed, or why not. Review again after each push.
9. **Merge** (the user has authorised it once review is done):
   squash-merge.
10. **Release with the new path**, from the merged main:

    ```bash
    git fetch origin && git checkout --detach origin/main
    cargo xtask release
    ```

    This is the real end-to-end test of the dev path. Confirm the
    release URL
    (`https://github.com/robot-lord/robot-overlord/releases/tag/v0.23.0-dev.8`)
    shows a pre-release with the tarball and `SHA256SUMS`. If it fails,
    fix forward in a follow-up commit or PR (which needs the next
    `-dev.N`); do not publish by hand.
11. **Reconcile this doc** in the PR that finishes the work (this one, if
    nothing is left over): the lasting parts are already in `CLAUDE.md`
    and `CONTRIBUTING.md`; `git rm docs/working/xtask.md`; keep owed
    items in `docs/working/public-launch.md` (already there). Say in the
    PR description what was promoted and archived.
12. **Report back:** PR URL, each review finding and what was done with
    it, the release URL, anything deferred.

## Rules for whoever continues

- Work only in this worktree; never `cd` to the main repo root; no bare
  `git stash`.
- Never print tokens or credentials (`~/.cargo/credentials.toml`, gh
  tokens). Don't touch the user's default tmux server. No
  `--dangerously-*` flags.
- Plain copy, no time estimates.

## Owed after this change

- The stable paths of `cargo xtask release` (CI, `--resume`, `--local`,
  tap, crates.io) can't be run for real before 1.0.0. They have unit
  tests for their decisions and the dry run; their first real run is the
  1.0.0 release. Recorded in `public-launch.md`, Road to 1.0 item 4.
- `scripts/smoke-linux.sh` and `scripts/dev-daemon.sh` still hold logic
  in bash.
- `publish-crates` does not restore the manifests on Ctrl-C.
