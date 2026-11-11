# robot-overlord (rlord)

## The overlord's instructions are part of every feature

The overlord (a long-running agent session wired to rlord's MCP server) is the
main way users drive rlord, and it does most of the upkeep: tagging sessions
into groups, writing descriptions and notes, organizing the list. It only
knows what its instructions and tool descriptions tell it.

Its built-in instructions are `crates/daemon/src/overlord.md`. They are
compiled in and written to the overlord's folder as `CLAUDE.md`, `AGENTS.md`
and `GEMINI.md` on every start, with the user's
`~/.config/robot-overlord/overlord.md` appended.

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

## Releases: build locally after every merge to main

Binaries are built on this Mac and uploaded to GitHub Releases. There is no
release CI (macOS runner minutes cost 10×, and cross-compiling from Linux
risks signing breakage). Every PR merged to `main` ships as a new release.

- **In each PR:** bump `[workspace.package] version` in `Cargo.toml`
  (pre-1.0: patch for fixes, minor for features or breaking changes) and
  commit the regenerated `Cargo.lock`. `scripts/check-version-bump.sh`
  enforces this: CI runs it on every PR, and `.githooks/pre-push` runs it
  before pushing a branch (enable once per clone with
  `git config core.hooksPath .githooks`).
- **After the PR merges:** release from an up-to-date `main`:

  ```bash
  git checkout main && git pull --ff-only
  scripts/release.sh
  ```

  `release.sh` builds `aarch64-apple-darwin`, ad-hoc signs, uploads
  `rlord-v<version>-aarch64-apple-darwin.tar.gz`, and creates tag
  `v<version>`. It refuses a dirty tree, an unpushed HEAD, or an existing
  tag. If the tag exists, the PR forgot the version bump.
- Installing on another Mac: see "Installing a release" in `README.md`.

## Other rules

- Don't touch the user's default tmux server in tests or scripts. rlord runs
  on its own socket (`tmux -L rlord`); tests use scratch sockets.
