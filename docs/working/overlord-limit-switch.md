**Status:** in progress

# Overlord usage-limit agent switch

When the active overlord's agent hits a usage limit, rlord offers the user a
switch to another agent's overlord. Only the user switches the active
overlord, so this is an offer they confirm, never automatic.

## Design

- **Detection:** none new. The active overlord's row already gets `limit`
  from `limits::annotate` (Claude transcript `usage_limit_reached` /
  `rejected`, Codex `task_complete` `usage_limit_exceeded` plus an exhausted
  `token_count` window, OpenCode's errored assistant row). See "Usage
  limits" in `docs/architecture.md`.
- **Offer data:** `overlord.get` adds `limit.switch_to` and `limit.offer`
  (`overlord::limit_offer`, pure over the roster and `limits::limited`).
  Targets: installed, not active, overlord not itself limited.
- **UX:** the list opens a dialog ("Overlord out of usage") on refresh.
  `y` switches (`overlord.switch`, creating the target overlord the first
  time); tab cycles targets; esc declines; either answer records for this limit (kv
  `overlord_limit_offer_declined` = its `resets_at`). A later limit asks
  again.
- `overlord.md`: one line saying the list offers the switch.

## Owed / open

- Not tested against a live limit on any agent (unit tests only). Codex and
  OpenCode limit detection were written from source; no real record seen.
- `overlord.ask` (the `n` form, `rlord overlord ask`, Remote Control) still
  pastes into a limited overlord. Option: refuse or return `limited` with
  `switch_to` so the caller can prompt. Not done: refusing risks blocking a
  user whose plan was topped up.
- The overlay (the overlord's popup) shows "out of usage until …: switch to
  another" but no dialog; a user who lives in the overlay sees the dialog
  only when back in the list.
- Only the list's refresh opens the dialog; a TUI not running sees nothing
  until it starts.
