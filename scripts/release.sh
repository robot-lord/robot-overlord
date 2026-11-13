#!/usr/bin/env bash
# Publish rlord as a GitHub release from the merged main.
#
#   scripts/release.sh [--local [--skip-smoke] | --resume]
#
# The version in Cargo.toml decides the kind of release:
#   X.Y.Z-dev.N  development build: built here on the maintainer's Apple
#                Silicon Mac, macOS only, a GitHub pre-release. What ordinary
#                PRs ship; `[update] channel = "dev"` installs them.
#   X.Y.Z        stable release: built in GitHub Actions
#                (.github/workflows/release.yml). This script pushes the tag,
#                waits for the run, checks the release assets against the
#                CI-made SHA256SUMS and provenance attestations, publishes the
#                draft release CI made, then does the steps that still need
#                local credentials: the Homebrew formula and crates.io.
#
# --local      stable only, the fallback when Actions is down: build macOS +
#              Linux here (scripts/build-release.sh, the same commands CI
#              runs), smoke-test in Docker, and create the release from here.
#              The tag push that creates also starts release.yml; its
#              preflight sees the published release and skips the build.
# --skip-smoke with --local: skip the Docker smoke test.
# --resume     stable only: the tag is already pushed (an earlier run of this
#              script stopped partway). Skip tagging; watch the tag's
#              workflow run, verify, publish the draft (if still a draft),
#              then do the tap and crates.io.
#
# Assets: rlord-v<ver>-<target>.tar.gz (rlord, install.sh, licenses),
# SHA256SUMS; stable also rlord_<ver>_<arch>.deb, rlord-<ver>.<arch>.rpm.
#
# Needs: gh; push access to robot-lord/homebrew-tap and `cargo login` for a
# stable release. --local also needs rustup targets
# x86_64/aarch64-unknown-linux-musl, zig, cargo-zigbuild, cargo-deb,
# cargo-generate-rpm and Docker (colima) for the smoke test.
set -euo pipefail
cd "$(dirname "$0")/.."
repo=robot-lord/robot-overlord
tap=robot-lord/homebrew-tap

smoke=1
local_build=0
resume=0
for a in "$@"; do
  case $a in
    --skip-smoke) smoke=0 ;;
    --local) local_build=1 ;;
    --resume) resume=1 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done

ver=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
tag="v$ver"
[[ -n "$ver" ]] || { echo "no version in Cargo.toml" >&2; exit 1; }
dev=0
[[ $ver == *-* ]] && dev=1
[[ $dev == 1 && $local_build == 1 ]] && { echo "--local is for stable releases; dev builds are always local" >&2; exit 2; }
[[ $dev == 1 && $resume == 1 ]] && { echo "--resume is for stable releases" >&2; exit 2; }
[[ $local_build == 1 && $resume == 1 ]] && { echo "--local and --resume don't mix" >&2; exit 2; }
if [[ $smoke == 0 && $dev == 1 ]]; then
  echo "--skip-smoke: dev builds have no smoke test; ignored" >&2
elif [[ $smoke == 0 && $local_build == 0 ]]; then
  echo "--skip-smoke only applies with --local (CI always smoke-tests)" >&2; exit 2
fi
build_here=$(( dev == 1 || local_build == 1 ))
# Every mode runs macOS tools later (BSD sed, shasum, codesign): check now,
# before anything is tagged or pushed.
[[ "$(uname -s)" == Darwin ]] || { echo "release.sh runs on macOS only (it uses BSD sed, shasum, codesign); on Linux, trigger the release workflow from Actions instead" >&2; exit 1; }
for c in gh git shasum; do
  command -v "$c" >/dev/null || { echo "release.sh needs $c" >&2; exit 1; }
done
if [[ $build_here == 1 ]]; then
  [[ "$(uname -m)" == arm64 ]] || { echo "build from an Apple Silicon Mac" >&2; exit 1; }
fi

# Refuse anything that would make the release not match a pushed commit.
[[ -z "$(git status --porcelain)" ]] || { echo "working tree is dirty" >&2; exit 1; }
git fetch -q origin
head=$(git rev-parse HEAD)
# Only merged code ships: HEAD must be on origin/main, not just on some
# pushed branch (a PR branch would burn the version before it merges).
git merge-base --is-ancestor "$head" origin/main ||
  { echo "HEAD $head is not on origin/main — release only merged code" >&2; exit 1; }
if [[ $resume == 1 ]]; then
  # The tag must be pushed, and point at HEAD.
  remote=$(git ls-remote --tags origin "refs/tags/$tag^{}" "refs/tags/$tag" | awk '{print $1}' | tail -1)
  [[ -n $remote ]] || { echo "--resume: $tag isn't on origin" >&2; exit 1; }
  [[ $remote == "$head" ]] || { echo "--resume: $tag is $remote, HEAD is $head" >&2; exit 1; }
elif gh release view "$tag" -R "$repo" >/dev/null 2>&1 || git ls-remote --exit-code --tags origin "$tag" >/dev/null 2>&1; then
  echo "$tag already exists — bump the version in Cargo.toml" >&2
  [[ $dev == 0 ]] && echo "(if an earlier run of this script pushed it and stopped: scripts/release.sh --resume)" >&2
  exit 1
fi

# Under the checkout, not the system temp dir: Docker on the Mac (colima)
# only sees the home folder, and the smoke test mounts these files. (macOS
# mktemp ignores TMPDIR.)
mkdir -p target
stage=$(mktemp -d "$PWD/target/release.XXXXXX")
trap 'rm -rf "$stage"' EXIT
dist=$stage/dist
mkdir -p "$dist"

if [[ $build_here == 1 ]]; then
  scripts/build-release.sh macos "$dist"
  [[ $dev == 0 ]] && scripts/build-release.sh linux "$dist"
  # Checksums before the smoke test: the installer it runs verifies them.
  (cd "$dist" && shasum -a 256 -- * > SHA256SUMS)
  if [[ $dev == 0 && $smoke == 1 ]]; then
    scripts/smoke-linux.sh "$dist"
  fi
  flags=(--generate-notes --title "$tag" --target "$head")
  [[ $dev == 1 ]] && flags+=(--prerelease)
  gh release create "$tag" -R "$repo" "$dist"/* "${flags[@]}"
  [[ $dev == 1 ]] && exit 0
else
  # Stable in CI: the tag push starts .github/workflows/release.yml.
  undo="delete the tag (git push origin :refs/tags/$tag; git tag -d $tag), then scripts/release.sh --local"
  if [[ $resume == 0 ]]; then
    git tag -f "$tag" "$head"
    git push origin "refs/tags/$tag" || { git tag -d "$tag" >/dev/null; echo "pushing $tag failed (local tag deleted)" >&2; exit 1; }
  fi
  find_runs() { # release.yml runs for this tag (push or dispatch), newest first: "id status"
    gh run list -R "$repo" -w release.yml -L 50 \
      --json databaseId,headBranch,headSha,status,event,createdAt \
      -q "[.[] | select(.headSha == \"$head\" and (.event == \"push\" or .event == \"workflow_dispatch\") and (.headBranch == \"$tag\" or .event == \"workflow_dispatch\"))] | sort_by(.createdAt) | reverse | .[] | \"\(.databaseId) \(.status)\""
  }
  echo "waiting for the release workflow on $tag…"
  run=
  for _ in $(seq 60); do
    run=$(find_runs | awk 'NR==1 {print $1}')
    [[ -n $run ]] && break
    sleep 5
  done
  if [[ -z $run ]]; then
    # Race: the run may register just after we give up. Cancel anything
    # queued for the tag so a late run can't publish over a --local release.
    sleep 10
    find_runs | while read -r id st; do
      [[ $st == completed ]] || gh run cancel "$id" -R "$repo" || true
    done
    echo "no release workflow run for $tag (any late run was cancelled) — check Actions; to build here: $undo" >&2
    exit 1
  fi
  gh run watch "$run" -R "$repo" --exit-status ||
    { echo "release workflow failed: https://github.com/$repo/actions/runs/$run — fix and re-run it, then scripts/release.sh --resume; or $undo" >&2; exit 1; }
  # CI leaves a draft. Check every asset against the CI-made SHA256SUMS,
  # and the tarballs' provenance, before publishing it.
  gh release download "$tag" -R "$repo" -D "$dist"
  (cd "$dist" && shasum -a 256 -c SHA256SUMS)
  for t in aarch64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
    f=rlord-$tag-$t.tar.gz
    grep -q " $f\$" "$dist/SHA256SUMS" || { echo "SHA256SUMS lacks $f" >&2; exit 1; }
    gh attestation verify "$dist/$f" -R "$repo" >/dev/null || { echo "provenance check failed for $f" >&2; exit 1; }
  done
  if [[ $(gh release view "$tag" -R "$repo" --json isDraft -q .isDraft) == true ]]; then
    gh release edit "$tag" -R "$repo" --draft=false --latest
  fi
fi

scripts/update-tap.sh "$ver" "$dist/SHA256SUMS" "$tap"
# crates.io publishes the source: only once the repo itself is public.
if [[ "$(gh repo view "$repo" --json visibility -q .visibility)" != PUBLIC ]]; then
  echo "crates.io: $repo is private — skipped (publish later with scripts/publish-crates.sh)" >&2
elif [[ -f ~/.cargo/credentials.toml ]]; then
  scripts/publish-crates.sh
else
  echo "crates.io: not logged in (cargo login) — skipped" >&2
fi
echo "released: https://github.com/$repo/releases/tag/$tag"
