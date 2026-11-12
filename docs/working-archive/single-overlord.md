# One overlord: what is still owed

Residue of `docs/working/single-overlord.md` (in git history), reconciled
2026-11-12 with 0.21.0-dev.1. How the overlord works now is in
`docs/guide.md` ("The overlord") and `docs/architecture.md` ("The
overlord"); the rules for changing it are in `CLAUDE.md`.

## Open question

- **Is low effort enough?** Opus at low effort is an experiment Brandon
  asked for. If the overlord proves too shallow (wrong repo picked, hints
  misread, delegating what it should just do), the fallback is Opus at
  medium: change the default in `OverlordConfig::defaults_for`
  (`crates/core/src/config.rs`) and the template, nothing else.

## Owed

- **Codex and OpenCode overlords.** The plan gives each agent a default
  (Codex: its own default model with `model_reasoning_effort = "low"`;
  OpenCode: its default model, no effort), and that part is built: the
  defaults (`OverlordConfig::defaults_for`), the effort flag per agent
  (`overlord::effort_args`) and the setup dialog's agent row, which lists
  every installed agent that can run the overlord. But only Claude Code can
  run it today (`Agent::runs_overlord`): wiring rlord's MCP server and the
  tool allow-list for the other two is owed by their own working docs
  (`docs/working/codex-support.md` phase 4, `docs/working/opencode-support.md`
  "Owed"). When one lands, flipping `runs_overlord` makes it pickable.
- **Screenshots.** `docs/images/overlord.png` (the README's first
  screenshot) is from the two-session overlord: its overlay title offers
  the mastermind, its text says "Handed to the mastermind", and the
  overlord's row says `sonnet`. The other list screenshots show that row
  too. The demo (`scripts/demo/demo.sh`) is updated; retake with
  `scripts/demo/screenshots.sh`.
- **The demo has no earlier conversations**, so a retaken screenshot won't
  show "fresh 2h ago · N earlier" or the list in the details pane. Seeding
  a few (events plus stand-in transcripts) would show the feature.
- **Hover.** The plan mentioned a hover on the overlord's bar saying why it
  restarted. The reason is in the details pane instead; there is no hover.
