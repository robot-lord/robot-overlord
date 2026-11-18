**Status:** in progress

# Attention queue: which session needs you next

Plan only. Nothing here is built. The user's request: while working in one
session, see that another session's status changed, most of all when it is
asking for the user; a clear indicator of what needs you next instead of
going back to the list to check. The overlord reads the same thing on
request. The user's condition: a half-working version is worse than none.

Short version of what the evidence says:

- The two statuses the queue needs already exist and each means one thing
  (`needs you` = `waiting_input`, `your turn` = `idle`). Since the fix on
  2026-11-11 the Claude needs-you signal has been clean in this install's
  log: 16 of 16 episodes started and ended on the right hook.
- Before that fix, 73 of 89 needs-you episodes were false (Claude's idle
  reminder). That is the failure the user is afraid of, and it happened
  here. So the first PR ships no alerts: it computes the queue in shadow
  and logs what it would have done.
- Needs-you is rare for this user (about 2 a day). The bulk of "a session
  wants me" is a finished turn. Alerting on needs-you only would be
  reliable but would rarely fire; see question 1.
- Three things rlord cannot tell today, and the plan says what to do about
  each: a turn the user interrupted, a turn that restarted by itself after
  background work, and Codex without trusted hooks.

## 1. Evidence audit

### How status is derived today

One stored status per session (`sessions.status`, `last_state_change_at`),
set by `Db::set_status` only when it changes. There is no `state_changed`
event: a transition is implied by the hook event row that caused it.
Overlays (`limited`, `background`) are laid over `idle` when a row is
shown (`daemon/src/lib.rs` `shown_status`).

| Agent | Source | needs you enters | needs you clears | your turn enters |
| --- | --- | --- | --- | --- |
| Claude | hooks (`claude::map_hook_event`, `methods::hook_emit`) | `PermissionRequest`; `PreToolUse` for `AskUserQuestion`; `Notification` of type `permission_prompt`, `elicitation_*`, `agent_needs_input` | `PostToolUse` / `PostToolUseFailure` (`Effect::Resume`), `Stop`, `UserPromptSubmit`, `SessionEnd`; elicitation answers | `Stop`, `StopFailure`, `SessionStart` |
| Codex, hooks trusted | hooks (`codex::map_hook_event`) | `PermissionRequest` | `PostToolUse`, `Stop`, `UserPromptSubmit`, `SessionEnd` | `Stop`, `SessionStart` |
| Codex, hooks not trusted | `daemon/src/discover.rs`: thread index and transcript tail, 3 s tick | never (`next_status` only moves between starting, working, idle) | n/a | transcript says the turn completed |
| OpenCode | `daemon/src/opencode.rs`: server event stream, resync from session rows and pending permissions/forms | a pending permission (`perm:`) or form (`form:`) | its answer, turn end | turn ended (events, or rows when no server) |

Who is blocked is tracked per party (`needs_you_by:<id>` in kv: the
session's own agent or a sub-agent), so one sub-agent moving on doesn't
clear another's prompt. A hook from outside the session's pane is ignored
(`foreign_hook`). Hook events spooled while the daemon was down are
replayed.

### What the log shows

Read-only from this install's `state.db` (`events`, `sessions`):
2026-11-10 to 2026-11-18, 11 sessions (8 Claude, 2 Codex, 1 OpenCode),
5029 events. Episodes are rebuilt from hook events. One user, eight days,
almost all Claude: enough to find failures, not enough to prove their
absence.

Needs you:

- 89 episodes in all. 73 began with a `Notification` on 11-10 and 11-11;
  65 of the 67 that followed a `Stop` came 55 to 65 s after it. That is
  Claude's idle reminder, mapped to needs-you by the code of the time.
  60 of them ended only at the user's next prompt. Fixed in `637e1a0`
  (11-11): none since.
- One session still reads `waiting_input` from such a notification 158
  hours later. Nothing re-checks a stored status.
- Since the fix: 16 episodes in 7 days. 14 `AskUserQuestion`, 2 plain
  permission prompts (one Claude, one Codex overlord). All 16 ended on
  `PostToolUse`. None repeated within a minute. Shortest 3 s, median
  about 80 s. Three waited long: 17 min, 40 min, 7 h. Those three are
  the cases an alert is for.
- For `AskUserQuestion` both `PreToolUse` and `PermissionRequest` fire in
  the same second (14 of 14). A stored reason must prefer "question".
- Never seen in the log, so unverified: a denied permission, Escape at a
  prompt, plan-mode approval, a Codex denial, any OpenCode prompt.

Your turn:

- 1024 `agent_stopped` events: 959 `Stop`, 60 `StopFailure` (6%), 5 from
  transcripts or OpenCode events.
- Time from a finished turn to the next prompt (873 episodes): median
  85 s; 6% under 1 s, 10% under 5 s, 14% under 10 s, 26% under 30 s; 5%
  over 30 min. The sub-5 s ones are turns that carried straight on
  (queued prompts, held messages) or the user watching.
- 56 `Stop` events arrived while the session was already idle with no
  prompt between: the agent worked again without a `UserPromptSubmit`
  (a background task's finish waking it, most likely). During that work
  the session read "your turn". That is a wrong status about 5% of the
  time a turn ends.
- 331 of 1305 prompts arrived with no `Stop` since the previous prompt.
  Some are prompts typed mid-turn. The rest are interrupted turns:
  Claude runs no `Stop` hook on Escape, so the session reads "working"
  until the next prompt. The log can't split the two.
- Only 49 of 925 finished turns (non-overlord) ended while the user's
  latest prompt was in another session: about 6 a day. For those, the
  median wait was 150 s, the 90th percentile 22 min. When the latest
  prompt was in the same session: median 95 s.
- At a `Stop`, the number of sessions sitting at "your turn" (under 6 h
  old) was 2 at the median, 3 at the 90th percentile, 5 at most.

Codex and OpenCode: 21 and 2 idle episodes, 1 needs-you (Codex, clean).
Effectively unmeasured.

### Reliability verdict

| Signal | Verdict |
| --- | --- |
| Claude permission prompt, `AskUserQuestion` | Reliable on enter and on answer (16 of 16). Denial and Escape paths unverified. |
| Claude turn finished | Reliable when `Stop` fires. Wrong when the turn restarts by itself (about 5%) and when the user interrupts (unmeasured). |
| Claude `Notification` | Proven dangerous once. Only typed notifications count now; keep it that way. |
| Codex with trusted hooks | Same shape as Claude for permission prompts; one sample. No question signal. |
| Codex without trusted hooks | Blind to needs-you. A blocked session reads "working". |
| OpenCode | Designed with resync, so it should self-heal; no data. Blind without a server. |
| Any stored status | Never re-verified. A missed clearing event leaves it wrong until the next event. |

## 2. Definitions

### One source of truth

The queue is a pure function, not a second state machine:

`attention::queue(rows, presence, now) -> Vec<Entry>`

- `rows`: the session rows as `session.list` already builds them (stored
  status, `last_state_change_at`, `last_agent_msg_at`, the `limit` and
  `async_tasks` overlays, role, archived).
- `presence`: which sessions a person's terminal is showing, and each
  session's `last_viewed_at` (below).
- New stored data is small: `sessions.last_viewed_at`, the needs-you
  reason, and which entries were already announced (in memory, keyed by
  session, kind and since; lost on a daemon restart, so a restart marks
  every current entry as announced without announcing it).

Everything that shows the queue (bar, message, jump key, list, overlord
tool, CLI) calls this one function. A unit test table covers it.

### Entry kinds

| Kind | Meaning | Since |
| --- | --- | --- |
| `needs_you` | Stored status `waiting_input`: blocked mid-turn on a permission prompt or a question dialog. Reason `permission` or `question` (kept from the event that set it; `question` wins when both fire). | `last_state_change_at` |
| `your_turn` | Shown status is your turn: stored `idle`, no `limited` overlay, no `background` overlay. Reason `finished`, or `failed` when the turn ended on `StopFailure`. | The later of `last_agent_msg_at` (moved by every `Stop`, also one that arrives while already idle) and the end of a background overlay |

Not entries: `working`, `starting`, `stopped`, `error`, `limited`,
`background`, archived sessions, a session with no tmux session. The
overlord's own sessions: `needs_you` yes, `your_turn` no (its replies
show in its overlay; 183 of 1171 idle episodes were the overlord's).

### Enter, announce, clear

Being in the queue and being announced are separate. The queue is a
fact. An announcement happens at most once per entry.

- **Enter**: the row matches the kind. No delay: the list and the
  overlord tool show it at once, as they show the status now.
- **Settle before announcing**: `needs_you` 3 s, `your_turn` 10 s,
  measured from since. If the entry is gone by then, nothing is
  announced and nothing is counted on the bar. Reasons: needs-you is set
  by a definite event and the shortest real one was 3 s; 14% of finished
  turns were followed by a prompt within 10 s.
- **Never announce** an entry for a session a person's terminal is
  showing when the settle time ends. It is marked seen instead.
- **Clear `needs_you`**: only the status leaving `waiting_input`.
  Looking at the session doesn't clear it; it is still blocked. It stops
  counting as new once seen.
- **Clear `your_turn`**: the status leaving `idle` (a prompt, a stop, an
  archive), or an overlay appearing.
- **Seen**: `last_viewed_at` later than since. A seen `your_turn` entry
  leaves the bar count and the jump order; the list still shows the
  status. A later `Stop` moves since forward, so the entry is new again.
- **No expiry**. A real question waited 7 h. An entry whose tmux session
  is gone is dropped (the session is stopped, or about to be reconciled).

### Order

`needs_you` first, oldest first. Then unseen `your_turn`, oldest first.
Oldest first because the longest wait is the most overdue and because
the order must not reshuffle under the jump key. Ties by session id.

### Per-agent gaps and what to do instead

| Gap | Effect | Plan |
| --- | --- | --- |
| Claude: turn restarts with no prompt (56 cases) | "your turn" shown while it works; a false announcement, then a second one | The 10 s settle hides most. Proper fix in PR A after a live check: a `PostToolUse` from the session's own agent while `idle` sets `working`. Sub-agent tool calls carry an agent id and are excluded. |
| Claude: user interrupts a turn | stays "working", never queued | The user was in the session when it happened, so the queue loses little. PR A logs enough to count it. A fix (the transcript's interrupted marker, read like `limits.rs` reads its lines) only if the count says it matters. |
| Claude: permission denied or Escape at the prompt | may stay `needs_you` until `Stop` or the next prompt | Live check in PR A. If no hook fires, `needs_you` clears on the next `Stop` or prompt, which both follow shortly in practice; note the lag. |
| Codex: hooks not trusted | needs-you unknowable | The row carries `needs_you_known: false`. The overlord tool and the list say "can't tell while Codex hooks aren't trusted". No announcement is ever made from a guess. `your_turn` still works (transcript). |
| Codex: no question tool hook | a Codex question dialog is not `needs_you` | Leave out. It surfaces as a permission prompt or a finished turn. |
| OpenCode: no server reachable | needs-you unknowable | Same `needs_you_known: false` while the server is unreachable. |
| Free-text question at the end of a turn | is `your_turn`, not `needs_you` | Leave out for good: no model calls in rlord, and punctuation guesses are the half-baked class. |
| Stale stored status | an entry that isn't true | See failure modes: a `needs_you` entry is checked against the pane's liveness, and shadow metrics count entries that ended with no user action. |

## 3. Surfaces

### How "in a session" is known

rlord's tmux server knows which session each terminal shows.
`tmux::list_clients` already returns, per non-control client, its
session and last activity. Presence for the queue:

- A session is **being viewed** when a client's current session is its
  tmux session.
- `last_viewed_at` is set to now for viewed sessions, each time the
  daemon samples.
- Sampling: one `list-clients` call a second, only while the queue has an
  unseen entry or one inside its settle time; otherwise none. Plus once
  whenever `rlord nav …` or `goto_session` moves a client.
- To check on a scratch socket in PR A (never the user's server, never
  `-L rlord` from tests): whether `#{client_flags}` reports `focused`
  with `focus-events on`. If it does, a client whose terminal window
  lost focus doesn't count as viewing. If it doesn't, attached counts as
  viewing and the doc says so.
- The TUI's home session counts as viewing nothing: the list itself
  shows the queue.
- Remote Control and other ways to answer a session need no presence:
  the answer changes the status.

### tmux status bar

Agent sessions already get a hint bar from `tmux::set_hint_bar`
(`@rlord-hints`, led into `status-right`, or a slim bar of rlord's own).
Add one more session option, `@rlord-attn`, placed before the hints.

- Text: `2 need you · 1 ready`, nothing when both are zero. Needs-you in
  the theme's alert colour, ready dim.
- Per session, not global, so each session's count leaves itself out.
  The daemon writes the option only for sessions whose text changed, then
  refreshes the clients showing them. Bounded by the number of running
  sessions.
- Counts only entries past their settle time, unseen for `your_turn`.
- Clickable through the existing status-click ranges: a click runs the
  jump.
- A session using the user's tmux profile with the status bar off keeps
  it off (as hints do today). There the message is the only passive
  sign.

### Notification

`tmux display-message -c <client> -d 4000` to each client that is
showing some other rlord session, once per entry, when its settle time
ends. Example: `api-fix needs you: permission prompt`. Two or more in
the same second become one line (`2 sessions need you`).

- Passive: it uses the status line for a few seconds and takes no
  focus and no keys.
- Not sent to a client showing the list (the TUI shows it there) or a
  client outside rlord's server.
- `[attention] notify = "needs_you"` by default; `"all"` adds
  `your_turn`; `"off"` none. The bar count is separate
  (`[attention] bar = true`).
- No popup, no sound, no OS notification in this plan.

### Jump key

A new nav verb, `rlord nav attention`, bound like `back` and `overlord`
(`[tmux] attention`, root table and the profile table, skipped on a
conflict as today). It switches the pressing client to the first queue
entry after the current session in queue order, wrapping. With an empty
queue it shows `nothing waiting` and stays. From the list, the same key
moves the selection to that row and enters it.

It differs from the `next`/`prev` cycling that is off by default: the
queue has a meaningful order and ends.

### TUI list

- A count in the header, same words as the bar.
- A mark on rows with an unannounced-or-unseen entry (the status words
  stay as they are).
- A sort `attention` (queue order first, then the current sort) and a
  filter token `status:attn`, through the existing query object, so
  `set_list_view` gets it too.
- The details card and hover text say the reason and how long
  (`needs you: question, 12 min`), reusing `statustext`.

### Overlord

One read-only MCP tool, `attention` (`mcp.rs`, `session_tools.rs`):
the queue in order, at most `limit` (default 20) entries, each with
session id, name, kind, reason, since, waited, seen, plus the counts and
the sessions whose needs-you can't be known. No screen reads, no
transcript reads: it is the same pure function.

`overlord.md` (agent-neutral, a few lines):

- Routing table: "what needs me / what's next / anything waiting" →
  itself, with `attention`.
- "Use `attention` for what is waiting on the user. Don't read screens
  or call `list_sessions` to work it out. Answer with the names in its
  order and the reason; offer to go to the first (`goto_session`). Say
  when a session's state can't be known."
- `triage`'s `waiting_on_user` flag stays for upkeep; one line says
  `attention` is the one to use for the user's question.
- The overlord is not told to watch the queue or to speak unasked.
  Nothing is pushed into its conversation.

CLI: `rlord attention` prints the same list (also the way to inspect the
shadow run).

## 4. What to leave out, failure modes, measuring

### Left out

- Detecting questions in free text (no model calls; guesses mislead).
- Any overlay or popup that takes focus, and switching the user's
  session for them.
- Sound, terminal bell, OS notifications (question 6).
- Pushing queue changes to the overlord.
- Snooze, dismiss, priorities, per-session rules. Viewing is the
  dismiss.
- "Working but stuck" detection, `error` and `limited` as entry kinds
  (a limit reset is already handled by `resume_after_limit`).
- A separate attention table or history. The `events` rows are the
  history.
- Remote sessions' presence, and anything on the user's own tmux server.

### Failure modes

| Failure | Prevention |
| --- | --- |
| False needs-you from a notification (happened: 73) | Only typed notifications set it (already so). A test pins that an untyped or unknown notification never creates an entry. Shadow metric: needs-you entries that ended without a tool result or user prompt. |
| Entry never clears (happened: one row, 158 h) | Entries are derived, so they clear when the status does. For the status itself: `needs_you` requires the tmux session to exist; shadow metric counts entries older than 12 h and how they ended. No fix-up for the existing stale row (the user's rule: no install migrations); a prompt in that session clears it. |
| Flapping (turn ends, carries on) | Settle times. Announce once per entry. Shadow metric: announcements whose entry cleared within 30 s with no user prompt. |
| Announcing the session you are in | Presence checked at settle time; a viewed session is marked seen, not announced. Per-session bar text leaves itself out. Test with a scratch tmux socket. |
| Announcing while the user watches the list | No tmux message to a client on the home session. |
| Repeat announcements | Keyed by session, kind and since. A daemon restart announces nothing that already existed. |
| A wall of entries after a restart or a long absence | Bar shows counts only. One coalesced message. |
| Wrong "your turn" after a self-restart | Settle time now; the `PostToolUse`-while-idle fix after its live check. |
| Missing needs-you (Codex untrusted hooks, OpenCode without server) | Said plainly as "can't tell" in the tool and list. Never inferred. |
| Bar and list disagree | One function. The bar shows a subset by a stated rule (settled, unseen). |
| tmux cost | One `list-clients` a second only while something is pending; option writes only on change. |
| The user's profile hides the status bar | Respected; the message still shows. |
| Overlord does heavy work for a simple question | One cheap tool; `overlord.md` routes to it. |

### Measuring before widening

PR A logs, and shows nothing:

- `attention_entered` `{kind, reason}`.
- `attention_settled` `{kind, viewed}`: the moment an announcement would
  be made; `viewed` true means it would have been withheld.
- `attention_cleared` `{kind, by, waited_ms, announced, seen}` with `by`
  one of `prompt`, `answer` (tool result after a prompt), `stop`, `ended`,
  `self` (the agent carried on with no user action), `overlay`.

`cargo xtask attention-report` (pure logic over `events`, unit-tested)
prints from them. Go/no-go numbers for turning announcements on, over at
least a week of real use:

- needs-you announcements cleared by `self`: none.
- your-turn announcements cleared by `self` within 30 s: under 2%.
- announcements for a viewed session: none (by construction; the number
  checks the presence code).
- entries open over 12 h whose session was dead: none.
- announcements per day, by kind: looked at with the user. The audit
  predicts about 2 needs-you and about 6 your-turn a day.

After announcements are on, the same events give time-to-answer for
needs-you, to compare with the audit's median of about 80 s and the
long tail (17 min, 40 min, 7 h).

## 5. Rollout

Three PRs, each a minor release. (The user prefers fewer, larger PRs;
the split is at the points where evidence decides whether to go on.)

**PR A: queue in shadow.** Size: medium; mostly new pure code and tests.

- `daemon/src/attention.rs`: the queue function, presence sampling,
  shadow events. `sessions.last_viewed_at`; the needs-you reason stored
  (also settles the "Needs-you reason" item owed in `bg-status.md`).
- `rlord attention`, the `attention` MCP tool, `overlord.md` lines,
  `needs_you_known`.
- `cargo xtask attention-report`.
- Live checks, results written here: denial and Escape at a Claude
  prompt, plan-mode approval, a turn woken by a background task
  (does a `PostToolUse` arrive while idle), a Codex denial,
  `client_flags` focus on a scratch socket. Then the
  `PostToolUse`-while-idle fix if the check supports it.
- No bar, no message, no key.

**PR B: passive display.** Size: medium; tmux options, one nav verb, TUI.

- `@rlord-attn` on the hint bar, the jump verb and its key, the list's
  count, mark, sort and filter. `docs/guide.md` and the keys screen.
- Gate: the shadow numbers for entries (not announcements) look right.

**PR C: announcements.** Size: small.

- The `display-message` announcement, `needs_you` only by default.
- Gate: the go/no-go numbers above.
- Reconcile: promote "Attention queue" into `docs/architecture.md` next
  to "Status engine", `git rm` this file.

Later, only with numbers: `your_turn` announcements by default, the
interrupted-turn fix, more for Codex and OpenCode.

Risks: tmux behaviour that needs live checks (focus flag, status refresh
after an option write, `display-message` with the status bar off); the
Claude hook paths never seen in the log; the small sample for Codex and
OpenCode.

## Questions for the user

1. **Does a finished turn announce itself?** Recommendation: the bar
   counts both kinds from PR B; the message is needs-you only at first.
   Needs-you is about 2 a day here and reliable; finished turns in
   another session about 6 a day and wrong about 5% of the time until
   the self-restart fix is proven. Turn on `notify = "all"` yourself
   once the shadow numbers pass.
2. **Does looking at a session clear its "your turn" from the count?**
   Recommendation: yes. Otherwise the count never returns to zero for
   sessions you chose to leave. Needs-you stays until answered.
3. **The jump key.** Recommendation: `M-a` (Option-A), beside `M-d` and
   `M-o`. It shadows readline's rarely used backward-sentence in agent
   panes. Say if you want another, or none by default.
4. **Sessions the overlord prompted.** Their finished turn is for the
   overlord, but you may want to know. Recommendation: in the queue,
   with the reason saying who prompted.
5. **Overlord sessions.** Recommendation: needs-you yes, your-turn no.
6. **A sign outside rlord** (terminal bell or an OS notification) when
   no terminal is showing rlord at all. Recommendation: leave out until
   the shadow run shows how often entries settle with nobody attached.
7. **Codex without trusted hooks.** Recommendation: say "can't tell" in
   the list and the tool, and never announce for it, rather than guess
   from a quiet pane.
8. **Fix the self-restart status in PR A** (`PostToolUse` while idle
   sets working, after a live check)? It changes the status engine, not
   only the queue. Recommendation: yes; it is the largest measured
   source of a wrong "your turn".
9. **A session with your tmux profile and the status bar off** shows no
   count. Recommendation: respect it; the message still appears.
