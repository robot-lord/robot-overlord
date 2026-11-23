**Status:** shipped (residue)

# Auto-resume after limits: owed items

Reconciled into `docs/architecture.md` and `docs/guide.md` ("Usage limits")
and `crates/daemon/src/overlord.md`. The full working doc is in git history
(PR #89).

## Owed

- **Live OpenCode retry screen.** `opencode::retrying` reads "retrying in
  <digit>" only in the lines from the prompt box's closing `╹` line down.
  Check this against a live OpenCode screen that is retrying. If OpenCode
  draws the countdown somewhere else, the hold won't fire.
- **Live TUI try.** The "Auto-resume after limits" dialog and the setup
  form's Auto-resume row are unit-tested only. Try both in a running TUI.
