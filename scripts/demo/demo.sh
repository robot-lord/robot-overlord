#!/usr/bin/env bash
# A throwaway rlord full of made-up sessions, for screenshots and trying the
# UI. Fully separate from a real install: its own data, config and tmux
# server (rlord-demo); "claude", "codex" and "opencode" are stand-ins
# (scripts/demo/bin) that draw a screen and report state as the real ones
# would (rlord's hooks; OpenCode: its session database), with no model.
#
#   scripts/demo/demo.sh [DIR]     # set up (DIR default: a new temp dir)
#   source DIR/env.sh && rlord     # look around
#   scripts/demo/demo.sh --stop DIR
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

if [[ ${1:-} == --stop ]]; then
  d=${2:?DIR}
  # shellcheck disable=SC1091
  source "$d/env.sh"
  rlord tmux unbind >/dev/null 2>&1 || true
  tmux -L rlord-demo kill-server 2>/dev/null || true
  pkill -f "$d/bin/rlord daemon" 2>/dev/null || true
  exit 0
fi

d=${1:-$(mktemp -d "${TMPDIR:-/tmp}/rlord-demo.XXXXXX")}
mkdir -p "$d"/{data,config/robot-overlord,run,state,claude,codex,bin,src,screens,states,summaries,usage}
d=$(cd "$d" && pwd)
# The daemon's socket lives under it, and a unix socket's path is short
# (about 100 bytes): say so here, not as a daemon that "isn't answering".
if (( $(printf '%s' "$d/run/robot-overlord/daemon.sock" | wc -c) > 100 )); then
  echo "demo dir path too long for a unix socket; use a shorter DIR: $d" >&2
  exit 1
fi

# The rlord to demo: this checkout's release build.
(cd "$root" && cargo build --release -q -p robot-overlord)
cp "$root/target/release/rlord" "$d/bin/rlord"
# tmux by itself (not its whole folder, which may hold a real agent): the
# stand-in login shell below runs rlord's own panes, the list included.
ln -sf "$(command -v tmux)" "$d/bin/tmux"
cp "$here/bin/claude" "$here/bin/codex" "$here/bin/opencode" "$d/bin/"
# A stand-in login shell that sees only the demo's programs: rlord asks the
# login shell where an agent is when its PATH doesn't have it, and the
# demo must never find (and launch) a real agent that way.
cat > "$d/bin/demo-shell" <<EOF
#!/bin/sh
PATH=$d/bin:/usr/bin:/bin; export PATH
case "\$1" in -l*) flags=\$1; shift; exec /bin/sh "-\${flags#-l}" "\$@" ;; esac
exec /bin/sh "\$@"
EOF
chmod +x "$d/bin/demo-shell"

cat > "$d/env.sh" <<EOF
export XDG_DATA_HOME=$d/data XDG_CONFIG_HOME=$d/config XDG_RUNTIME_DIR=$d/run
export XDG_STATE_HOME=$d/state CLAUDE_CONFIG_DIR=$d/claude CODEX_HOME=$d/codex
export RLORD_TMUX_SOCKET=rlord-demo RLORD_DEMO_DIR=$d
# Only the stand-in agents: never ask a login shell where a real one is.
export RLORD_NO_LOGIN_SHELL=1 SHELL=$d/bin/demo-shell
export PATH=$d/bin:\$PATH
EOF
# shellcheck disable=SC1091
source "$d/env.sh"

cat > "$d/config/robot-overlord/config.toml" <<'EOF'
[overlord]
model = "opus"
effort = "low"
EOF

B=$'\e[1m' D=$'\e[2m' R=$'\e[0m' Y=$'\e[33m' M=$'\e[35m'
box() { printf '%s\n' "${D}────────────────────────────────────────────────────────────────────────${R}" "> $1" "${D}────────────────────────────────────────────────────────────────────────${R}" "  ${D}? for shortcuts${R}"; }

# add NAME PROJECT STATE "TAGS" DESCRIPTION PROMPT  (screen body on stdin)
# AGENT=codex add …: a Codex session; AGENT=opencode: an OpenCode one.
# USAGE="TURNS CONTEXT": how far its conversation has got (the stand-in
# writes that many turns; the list's Usage column shows them).
add() {
  local name=$1 proj=$2 state=$3 tags=$4 desc=$5 prompt=$6 dir=$d/src/$1
  mkdir -p "$dir"
  printf '%s' "$state" > "$d/states/$name"
  printf '%s\n' "${USAGE:-3 20000}" > "$d/usage/$name"
  { printf '%s\n\n' "${D}${proj}${R}"; printf '%s\n' "> $prompt" ""; cat; printf '\n'; box ""; } > "$d/screens/$name.txt"
  rlord new --agent "${AGENT:-claude}" --project "$dir" --name "$name" "$prompt" >/dev/null
  # shellcheck disable=SC2086
  rlord tag "$name" $tags >/dev/null
  rlord describe "$name" "$desc" >/dev/null
}

USAGE="38 151000" add checkout-redesign acme-web working "web wip" "Rebuild checkout on the new design system" "Rebuild the checkout page with the new design system components" <<EOF
● I'll move checkout to the new components, one step at a time.
● Read(src/checkout/CheckoutPage.tsx)
  ${D}⎿  Read 214 lines${R}
● Update(src/checkout/CheckoutPage.tsx)
  ${D}⎿  Updated with 38 additions and 52 removals${R}
● Update(src/checkout/PaymentForm.tsx)
  ${D}⎿  Updated with 21 additions and 17 removals${R}
● Bash(pnpm test checkout)
  ${D}⎿  Running…${R}

${Y}✻ Swapping the address form to <Field>… (2m 14s · esc to interrupt)${R}
EOF

USAGE="14 64000" add billing-webhooks acme-api waiting "billing" "Retry failed Stripe webhooks idempotently" "Make the Stripe webhook handler retry failed events without double-charging" <<EOF
● The handler now stores each event id and skips duplicates.
● Bash(cargo test webhooks)
  ${D}⎿  test result: ok. 18 passed; 0 failed${R}

● Before I add the retry job: failed events can be retried for 3 days.
  Should the job give up after 3 days too, or alert someone?

  ${B}1.${R} Give up after 3 days and log it
  ${B}2.${R} Alert #billing-oncall, keep retrying
  ${B}3.${R} Something else
EOF

USAGE="6 41000" add flaky-login-test acme-web idle "auth tests" "Fix the flaky login integration test" "The login integration test fails about 1 in 10 runs, find out why" <<EOF
● Found it: the test reads the session cookie before the refresh
  request finishes. It passes only when the refresh wins the race.
● Update(tests/auth/login.test.ts)
  ${D}⎿  Updated with 4 additions and 1 removal${R}
● Bash(for i in \$(seq 20); do pnpm test login || break; done)
  ${D}⎿  20 runs, 0 failures${R}

● Fixed: the test now awaits the refresh. 20 runs, no failures.
EOF

USAGE="9 52000" add api-rate-limits acme-api working "api" "Per-key rate limits on the public API" "Add per-API-key rate limiting to the public API" <<EOF
● Plan: a token bucket per key in Redis, limits from the plan tier.
● Write(src/ratelimit/bucket.rs)
  ${D}⎿  Wrote 96 lines${R}
● Bash(cargo test ratelimit)
  ${D}⎿  Running…${R}

${Y}✻ Testing the burst limit… (48s · esc to interrupt)${R}
EOF

USAGE="22 118000" add docs-navigation acme-docs waiting "docs" "New sidebar navigation for the docs" "Rework the docs sidebar so guides and API reference are separate" <<EOF
● The new sidebar is ready: Guides and Reference are separate trees,
  and every page keeps its URL.

● Want me to push the branch and open a PR?
EOF

USAGE="11 73000" add terraform-upgrade acme-infra idle "infra" "Upgrade Terraform providers to v5" "Upgrade the AWS Terraform provider to v5 and fix what breaks" <<EOF
● Upgraded the AWS provider to 5.31 and fixed the three breaking changes.
● Bash(terraform plan)
  ${D}⎿  No changes. Your infrastructure matches the configuration.${R}

● Done. The plan is clean.
EOF

USAGE="4 23000" add push-notifications acme-mobile idle "mobile" "Push notifications for order updates" "Send push notifications when an order ships" <<EOF
● Shipping events now send a push through the notifications service.
● Bash(pnpm test notifications)
  ${D}⎿  12 passed${R}
EOF

AGENT=codex USAGE="7 88000" add image-pipeline acme-api idle "api perf" "Resize product images off the request path" "Move product image resizing to a background job" <<EOF
• Moved resizing into a queue worker; uploads now return in ~80ms.
• Ran cargo test images
  ${D}└ test result: ok. 9 passed; 0 failed${R}

• Done. The old inline resize is removed; thumbnails backfill on first view.
EOF

# An OpenCode session. (No stand-in server: rlord reads working / your
# turn from OpenCode's session row; "needs you" would need its events.)
AGENT=opencode USAGE="3 15000" add onboarding-emails acme-api idle "api growth" "Welcome email sequence for new sellers" "Draft the three-email welcome sequence for new sellers" <<EOF
• Drafted the sequence in emails/onboarding/: welcome, first listing, payouts.
• Each email has a plain-text twin and one call to action.

• Ready for review. Subject lines are in emails/onboarding/README.md.
EOF

# An archived session, with its summary (written by the stand-in claude
# through the real archive path).
name=search-indexer-migration
mkdir -p "$d/src/$name"
printf idle > "$d/states/$name"
printf '%s\n' "27 132000" > "$d/usage/$name"
printf '%s\n' "● Migrated the indexer to the new queue." > "$d/screens/$name.txt"
cat > "$d/summaries/$name.txt" <<'EOF'
Moved the search indexer from cron batches to the event queue, so new
listings are searchable within seconds instead of every 15 minutes.

Done: queue consumer (src/indexer/consumer.rs), backfill command
(`indexer backfill --since`), dashboards for lag. The old cron job is
disabled but not deleted.

Left: delete the cron job and its IAM role once a week of lag stays under
5s; the backfill still loads everything into memory for large tenants.
EOF
rlord new --project "$d/src/$name" --name "$name" "Move the search indexer onto the event queue" >/dev/null
rlord tag "$name" search >/dev/null
rlord describe "$name" "Search indexer: cron batches to the event queue" >/dev/null
sleep 2
rlord archive "$name" >/dev/null
for _ in $(seq 20); do
  sqlite3 "$d/data/robot-overlord/state.db" "select summary from sessions where name='$name'" | grep -q . && break
  sleep 0.5
done

# The OpenCode session's status comes from rlord's own look at OpenCode's
# database (every few seconds): wait for it, so the times below stand.
for _ in $(seq 20); do
  [[ $(sqlite3 "$d/data/robot-overlord/state.db" "select status from sessions where name='onboarding-emails'") == idle ]] && break
  sleep 0.5
done

# Spread the times out, as if the fleet had been running for a while.
now=$(($(date +%s) * 1000))
ago() { echo $((now - $1 * 60000)); }
sql() { sqlite3 "$d/data/robot-overlord/state.db" "$1"; }
set_times() { # name user_min agent_min created_min
  sql "update sessions set last_user_msg_at=$(ago "$2"), last_agent_msg_at=$(ago "$3"), last_state_change_at=$(ago "$3"), created_at=$(ago "$4") where name='$1'"
}
set_times checkout-redesign 3 0 95
set_times billing-webhooks 41 12 180
set_times flaky-login-test 130 118 300
set_times api-rate-limits 7 1 60
set_times docs-navigation 75 64 1500
set_times terraform-upgrade 1440 1430 2900
set_times push-notifications 2880 2870 4400
set_times image-pipeline 200 190 420
set_times onboarding-emails 55 50 240
set_times search-indexer-migration 10080 10070 14400
sql "update sessions set pinned=1 where name='checkout-redesign'"

# The overlord (the stand-in draws its screen).
printf '%s\n' "5 36000" > "$d/usage/overlord"
cat > "$d/screens/overlord.txt" <<EOF
${M}${B}◆ overlord${R}

> what's waiting on me?

● Two sessions are waiting on you:
  • ${B}billing-webhooks${R}: asks how long failed Stripe events should be
    retried (3 days, or alert #billing-oncall)
  • ${B}docs-navigation${R}: the new sidebar is ready; it wants to open a PR

> tell docs-navigation to go ahead and open the PR

● Sent. docs-navigation is pushing the branch.

> start a session on the slow product search page

● Launched ${B}slow-product-search${R} in acme-web, on its own worktree,
  tagged web and perf.

$(box "")
EOF
python3 - "$d/run/robot-overlord/daemon.sock" <<'PY'
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
s.sendall((json.dumps({"id": 1, "method": "overlord.ensure", "params": {}}) + "\n").encode())
s.recv(65536)
PY

# Earlier overlord conversations (it restarts fresh after idle): transcripts
# next to the current one, and the restart events that chain them.
for _ in $(seq 20); do
  src=$(sql "select t.source_path from transcripts t join sessions s on s.id=t.session_id where s.role='overlord'" 2>/dev/null)
  [[ -n $src ]] && break
  sleep 0.5
done
oid=$(sql "select id from sessions where role='overlord'")
cur=$(sql "select agent_session from sessions where role='overlord'")
if [[ -n ${src:-} && -n $oid && -n $cur ]]; then
  tdir=$(dirname "$src")
  past() { # minutes_ago turns first_prompt reply
    local id
    id=$(python3 -c 'import uuid; print(uuid.uuid4())')
    python3 - "$tdir/$id.jsonl" "$2" "$3" "$4" <<'PY2'
import json, sys
path, turns, first, reply = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
with open(path, "w") as f:
    for i in range(turns):
        f.write(json.dumps({"type": "user", "message": {"content": first if i == 0 else "and then?"}, "timestamp": "2026-01-01T00:00:00Z"}) + "\n")
        f.write(json.dumps({"type": "assistant", "message": {"content": [{"type": "text", "text": reply}]}, "timestamp": "2026-01-01T00:00:05Z"}) + "\n")
PY2
    echo "$id"
  }
  c1=$(past 1500 4 "what did everything do overnight?" "Three sessions finished; terraform-upgrade's plan is clean.")
  c2=$(past 400 6 "tag the api sessions and tidy their descriptions" "Tagged 4 sessions api; rewrote two descriptions.")
  c3=$(past 130 2 "is anything stuck?" "No: billing-webhooks is waiting on you, the rest are working.")
  ev() { sql "insert into events (session_id, ts, kind, payload) values ('$oid', $(ago "$1"), 'fresh_conversation', '{\"from\":\"$2\",\"to\":\"$3\",\"reason\":\"idle\",\"idle_mins\":15}')"; }
  ev 1490 "$c1" "$c2"
  ev 380 "$c2" "$c3"
  ev 115 "$c3" "$cur"
fi

echo "demo ready in $d"
echo "  source $d/env.sh && rlord"
echo "  scripts/demo/screenshots.sh $d    # images into docs/images"
echo "  scripts/demo/demo.sh --stop $d"
