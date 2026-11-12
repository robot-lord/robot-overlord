# robot-overlord (rlord)

## The overlord's instructions are part of every feature

The overlord (a long-running agent session wired to rlord's MCP server) is the
main way users drive rlord, and it does most of the upkeep: tagging sessions
into groups, writing descriptions and notes, organizing the list. It only
knows what its instructions and tool descriptions tell it.

The overlord is one session, on a strong model at medium effort. Its built-in
instructions are `crates/daemon/src/overlord.md`. They are compiled in and
written to its folder as `CLAUDE.md`, `AGENTS.md` and `GEMINI.md` on every
start, with the user's `~/.config/robot-overlord/overlord.md` appended.
It runs on Claude Code, Codex or OpenCode (`[overlord] agent`), so a
feature that gives the overlord something (a tool, an allowed action, a
subagent) must be wired for each agent: `overlord::write_files`, and
"The overlord" in `docs/architecture.md` for how each differs.

`overlord.md` opens with one rule that every feature must respect: the overlord does only light work itself (look-ups,
small edits to rlord's metadata, short answers); anything heavier goes to a
subagent or a new session. A feature that needs the overlord to think hard
in its own conversation is designed wrong: give it a cheap tool, a subagent,
or a session to launch instead.

When a change touches anything the overlord can see or do, update
`overlord.md` in the same change:

- **New or changed MCP tool** (`crates/daemon/src/mcp.rs`): describe what it's
  for and when to use it. A test fails if a tool isn't mentioned.
- **New session meta-context** (a field, tags, groups, notes, a state): say
  how the overlord should maintain it, so upkeep stays its job rather than the
  user's.
- **New list/UI concept** the overlord can set (tabs, filters, sorts, views):
  update the syntax and values it's told about.
- **Removed or renamed features**: delete them from `overlord.md`; stale
  instructions make it call tools that don't exist. A test fails if its
  tool list names one that is gone.
- **Where a new kind of request goes**: add it to the routing table at the
  top (itself, a subagent, a new session, an existing session).

What belongs in `overlord.md`:

- what the tools are for, when to prefer one over another, and its standing
  duties (keeping the list tidy);
- behaviour rules: be brief, don't touch sessions the user didn't mention,
  don't overwrite the user's own edits.

What doesn't belong there:

- parameter details: those go in the tool's JSON schema description in
  `mcp.rs`, which the agent sees anyway;
- anything user- or machine-specific: that is the user's `overlord.md`;
- agent-specific syntax: the same file is read by Claude, Codex and Gemini.

Keep it short. It's loaded into every overlord turn.

## Plain code is the overlord's backbone

The overlord's AI time is the expensive part. Anything that can be decided
by ordinary code should be: give the overlord a cheap rlord tool that does
the deterministic part and hands it only what needs judgment.

- Example: `recall` (`crates/daemon/src/recall.rs`). The overlord restarts
  on a fresh conversation after sitting idle, so it can't carry its past.
  Rather than summarize it with a model or preload it, rlord searches the
  overlord's earlier transcripts and hands back short dated snippets, only
  when asked.
- Example: `triage` (`crates/daemon/src/triage.rs`). Instead of asking every
  session's agent for a report, rlord works out which sessions changed since
  the overlord last reviewed them, what their meta-context is missing, and
  excerpts from their transcripts. The overlord spends tokens only on those.
- Signals come from data rlord already has: DB timestamps, hook events, kv
  watermarks, transcript files read directly. Don't add an AI call inside
  rlord to produce them.
- Keep such tools bounded: limits, clipped excerpts, tail-reads of large
  files. They run on every overlord pass.
- When you find the overlord doing repetitive look-ups or bulk edits, build
  the tool, then point `overlord.md` at it.

## Bulk upkeep runs in subagents

Passes over many sessions (tidying names, descriptions, tags, notes) run in
a subagent, not in the overlord's own conversation. The subagent gets only
what it needs and a fresh context, the overlord's context stays small, and
the user can keep talking to the overlord while it runs.

- Subagent definitions live next to the instructions (`crates/daemon/src/tidy.md`
  is `rlord-tidy`), written once in Claude's format. Give each a narrow
  `tools:` list of `mcp__rlord__*` tools (a test checks they exist),
  `omitClaudeMd: true`, and a cheaper `model:`.
- On every start they are written to the overlord's folder for each agent
  (`overlord::write_files`): `.claude/agents/*.md` as they are,
  `.codex/agents/*.toml` and `.opencode/agents/*.md` converted (name,
  description, instructions; those agents' subagents inherit the
  overlord's tools), and `subagents/*.md` as plain instructions. So keep
  the body agent-neutral: tools by their rlord name, no agent's syntax.
- `overlord.md` tells the overlord when to delegate and what scope to pass,
  and keeps a fallback (do it yourself following `subagents/<name>.md`)
  for an overlord that can't start subagents.
- Gemini (`.gemini/agents/*.md`) has its own format; add it when its
  adapter lands.

## Input: describe, then confirm

The standard way rlord takes config-style input (a new session today; any
form with options tomorrow) is a conversation with the overlord, not a
terminal form to fill in:

1. **Describe.** The user says what they want in their own words. Option
   fields sit below as *optional hints*: shown with their defaults, visibly
   secondary, and labelled so the user knows they can skip them.
2. **The description wins.** The form sends the description plus each hint
   and whether the user picked it or left the default.
3. **Propose.** If the description contradicts a hint, the overlord doesn't
   act on either. It reopens the same form with the hints as it would apply
   them, marks what it changed, and says why in one line.
4. **Confirm.** The user checks it, edits if they like, and sends it back
   marked confirmed. Then the overlord acts exactly as shown.

The user stays in control of every option without having to set any of
them. A new form or option should follow this loop: a describe field, hint
fields with defaults, a `confirm_*` MCP tool that reopens the form, and
`overlord.md` rules for when to use it. Reference implementation: `n` in the
list (`tui/src/newform.rs`, `confirm_new_session`, `ask::format_request`).
A manual form (`N`) can exist alongside it for users who want no overlord.

## Review every PR before it merges

After opening a PR (and after pushing more to it before merge), run the
Claude Code review on it: `/code-review <PR number>`. Fix what's real,
push, and say in one line per finding what was fixed or why it wasn't.
Only then merge (when the user has said to). Don't merge a PR the review
hasn't seen.

## Releases: build locally after every merge to main

Binaries are built on the maintainer's Apple Silicon Mac and uploaded to
GitHub Releases; there is no release CI. Linux builds are cross-compiled
there (`cargo zigbuild`, static musl) and smoke-tested in Docker. Every PR
merged to `main` ships as a release. CONTRIBUTING.md has the full setup.

- **In each PR:** bump `[workspace.package] version` in `Cargo.toml` and
  commit the regenerated `Cargo.lock`. Ordinary PRs bump to a **development
  build**: the next version with `-dev.N` (`0.18.0` → `0.19.0-dev.1` for a
  feature or `0.18.1-dev.1` for a fix, then `-dev.2`, … for later PRs
  toward it). A **stable release** (`X.Y.Z`, no suffix) only when the user
  asks for one. `scripts/check-version-bump.sh` enforces a semver bump: CI
  runs it on every PR, and `.githooks/pre-push` before pushing a branch
  (enable once per clone with `git config core.hooksPath .githooks`).
- **After the PR merges, cutting the release is Claude's job.** Don't
  hand it back to the user. If you merge the PR, release right after. If
  you opened it, check `gh pr view <n> --json state` when you next act
  and release once it reads `MERGED`. If the user tells you it merged,
  release then. Release from the merged `main` (detached, so it works in a
  worktree while `main` is checked out elsewhere):

  ```bash
  git fetch origin && git checkout --detach origin/main
  scripts/release.sh
  ```

  Then report the release URL to the user.

  The version decides the kind: a dev build is macOS only and a GitHub
  pre-release (installed by `[update] channel = "dev"`); a stable release
  builds macOS + Linux tarballs, `.deb`/`.rpm`, smoke-tests them in Docker
  (needs `colima start`), uploads with `SHA256SUMS`, updates the Homebrew
  tap and publishes to crates.io. `release.sh` refuses a dirty tree, HEAD
  not on `origin/main`, or an existing tag (the PR forgot the bump).

## Documentation

- `docs/` holds the official docs (indexed from `docs/README.md`): how
  rlord works now. They outrank working docs.
- Any effort bigger than a small fix gets a working doc in
  `docs/working/<effort>.md`, started at the beginning and kept current,
  with a `**Status:** in progress | shipped | parked | abandoned` top line.
  Deferred findings and owed follow-ups go there.
- When the effort is done, the PR that finishes it reconciles it: promote
  what lasts into `docs/` (and CLAUDE.md), repoint citations, `git rm` the
  working doc, and keep a trimmed residue in `docs/working-archive/` (with
  a row in its README) only if something is still owed.
- Any PR that makes an official doc wrong fixes it in the same PR.

## Dependency updates

Dependabot opens one grouped PR per week for Cargo and one for GitHub
Actions (`.github/dependabot.yml`), here and in `robot-lord/website`. Its
PRs don't bump the version, so `version-bump` fails on them: they need a
bump commit (next `-dev.N`) before they can merge. CI's `advisories` job
(cargo-deny, `deny.toml`) fails on known-vulnerable dependencies; an
exception in `deny.toml` needs a reason.

**Whenever you open or update a PR in this repo, launch a subagent that
resolves open Dependabot PRs** in `robot-lord/robot-overlord` and
`robot-lord/website`. Don't wait to be asked: this is how Dependabot PRs
get handled without the user looking. For each one:

- robot-overlord: push a version-bump commit with the regenerated
  `Cargo.lock` (stay below any version another open branch has claimed);
- build, `cargo clippy --all-targets -- -D warnings` and test; fix
  breakage from major bumps in the same PR, or drop an update whose fix
  would be large or risky and say why;
- check the repo's Actions allow-list still permits every action used;
- wait for CI to go green, review the diff (changelogs of major bumps),
  squash-merge, and release (robot-overlord) as above. Website merges
  deploy through Vercel; confirm the site still loads.

## Other rules

- Don't touch the user's default tmux server in tests or scripts. rlord runs
  on its own socket (`tmux -L rlord`); tests use scratch sockets.
