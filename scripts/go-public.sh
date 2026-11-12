#!/usr/bin/env bash
# GitHub settings that only exist for public repositories. Run once, right
# after making the repository public (docs/working/public-launch.md).
set -euo pipefail
repo=${1:-robot-lord/robot-overlord}
[[ $(gh repo view "$repo" --json visibility -q .visibility) == PUBLIC ]] || {
  echo "go-public: $repo isn't public yet" >&2; exit 1; }
gh api -X PATCH "repos/$repo" --input - >/dev/null <<'JSON'
{"security_and_analysis": {"secret_scanning": {"status": "enabled"},
  "secret_scanning_push_protection": {"status": "enabled"}}}
JSON
echo "secret scanning and push protection: on"
gh api -X PUT "repos/$repo/private-vulnerability-reporting" >/dev/null
echo "private vulnerability reporting: on"
gh api -X PUT "repos/$repo/actions/permissions/fork-pr-contributor-approval" \
  -f approval_policy=all_external_contributors >/dev/null
echo "workflows on outside contributors' PRs: need approval"
