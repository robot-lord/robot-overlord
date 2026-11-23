**Status:** shipped (residue)

# Auto-resume after limits: owed items

Reconciled into `docs/architecture.md` and `docs/guide.md` ("Usage limits")
and `crates/daemon/src/overlord.md`. The full working doc is in git history
(PR #89).

## Live checks

- **OpenCode retry screen.** `opencode::retrying` reads "retrying in
  <digit>" only from the prompt box's closing `╹` line down. Check it on a
  live OpenCode screen that is retrying: if the countdown is drawn
  elsewhere, the hold never fires.
- **TUI.** The "Auto-resume after limits" dialog and the setup form's
  optional Auto-resume row are only unit-tested. Try both in a running TUI.

## Owed fixes

- **Resumes and sends**
  - Send OpenCode resumes through `send::post` (OpenCode's own queue)
    instead of scraping its screen for the retry countdown.
  - The OpenCode retry hold covers only `Source::Resume`; extend it to
    `Source::Send`.
- **`auto_schedule` and `limit_auto`**
  - The `limit_auto` marker is pruned when a session is archived or
    unarchived across a restart, so its limit can be scheduled again.
  - Return early from `auto_schedule` when no session is limited.
  - `drop_auto_jobs` reads the jobs map on every tick while auto-resume
    is off. Run it only when the setting changes.
- **Stand-down check**
  - `native_auto_resume` has three call sites; make it a single check.
  - Its always-false match arms are dead branches.
- **Config**
  - The config write helpers (`set_value`, `set_overlord`,
    `set_setup_at`, `set_auto_resume_at`) repeat the same read and write
    steps; share them.
- **TUI**
  - `wants_ask` runs on the list's filtered view, so a filter can hide a
    limited session from it.
  - `offer_auto_resume` makes a blocking daemon call inside refresh.
  - Ctrl+D ("don't ask again") in the setup form drops the auto-resume
    pick made on that form.
