# robot-overlord (rlord)

## The overlord's instructions are part of every feature

The overlord (a long-running agent session wired to rlord's MCP server) is the
main way users drive rlord, and it does most of the upkeep: tagging sessions
into groups, writing descriptions and notes, organizing the list. It only
knows what its instructions and tool descriptions tell it.

The overlord is two sessions: the **overlord** (quick front desk, fast
model) and the **mastermind** (launches and questions that need thought,
strong model). Their built-in instructions are
`crates/daemon/src/overlord.md` and `crates/daemon/src/mastermind.md`. They
are compiled in and written to each one's folder as `CLAUDE.md`, `AGENTS.md`
and `GEMINI.md` on every start, with the user's
`~/.config/robot-overlord/overlord.md` appended.

When a change touches anything the overlord can see or do, update
`overlord.md` and/or `mastermind.md` (whichever seat does it) in the same
change:

- **New or changed MCP tool** (`crates/daemon/src/mcp.rs`): describe what it's
  for and when to use it. A test fails if a tool isn't mentioned.
- **New session meta-context** (a field, tags, groups, notes, a state): say
  how the overlord should maintain it, so upkeep stays its job rather than the
  user's.
- **New list/UI concept** the overlord can set (tabs, filters, sorts, views):
  update the syntax and values it's told about.
- **Removed or renamed features**: delete them from `overlord.md`; stale
  instructions make it call tools that don't exist.

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
  is `rlord-tidy`) and are written to `<data>/overlord/.claude/agents/` on
  every start. Give each a narrow `tools:` list of `mcp__rlord__*` tools (a
  test checks they exist), `omitClaudeMd: true`, and a cheaper `model:`.
- `overlord.md` tells the overlord when to delegate and what scope to pass,
  and keeps a fallback (do it yourself following the agent file) for agents
  without subagents.
- Codex (`.codex/agents/*.toml`) and Gemini (`.gemini/agents/*.md`) have
  their own formats; add those when their adapters land.

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

## Other rules

- Don't touch the user's default tmux server in tests or scripts. rlord runs
  on its own socket (`tmux -L rlord`); tests use scratch sockets.
