**Status:** in progress

# Auto-resume after usage limits (`[limits] auto_resume`)

Opt-in: rlord schedules the same resume `resume_after_limit` does, for
every session stopped on a usage limit, without being asked. Unset means
off, and rlord asks the user once to choose.

## Research (why rlord does it)

No agent CLI rlord runs picks a session up when a usage limit resets.

| Agent | Behaviour at a usage limit | Source |
| --- | --- | --- |
| Claude Code | "Auto-continue when limits reset" exists only in the Desktop app; no CLI setting or flag. | https://code.claude.com/docs/en/whats-new/2026-w33 |
| Codex | Retries transient 429s only; a usage limit ends the turn. | Codex source (`codex_error_info: usage_limit_exceeded` ends the turn; see "Usage limits" in `docs/architecture.md`) |
| OpenCode | Retries on the same provider, at times with a long "retrying in Ns" countdown; no resume at the reset. | OpenCode 2.2.7 source (`session.step.failed`, retry loop) |

## Design

- Config `[limits] auto_resume`: `Option<bool>` (`LimitsConfig::auto_resume_on`).
  Unset or false: nothing automatic.
- `limits::auto_schedule` runs each tick between `scan` and `run_jobs`.
  It gives every limited session without a job a job via the shared
  `limits::schedule` (the one `limits.resume` uses), marked `auto: true`.
  Overlords, archived sessions and stand-down agents are skipped.
- Once per limit: kv `limit_auto` `{id: limit at}`. A cancelled job is
  not remade for the same limit; a new limit is scheduled again.
- Must-choose prompt (unset only):
  - first-run setup form: an "Auto-resume" row; Save refuses until it's
    picked; saved by `setup.apply {auto_resume}`. Sending the form to the
    overlord saves the answer at once (`limits.auto_resume`), since the
    overlord's reopened form doesn't carry it.
  - TUI dialog when a listed (non-overlord) session is limited: `y` on,
    `n` off, Esc asks again next TUI start.
  - Both write rlord's `config.toml` only (`Config::set_auto_resume_at`,
    toml_edit, keeps the file as written) and the daemon takes it at once
    (`update_config`).
- OpenCode guard (auto and manual): `step` waits while an OpenCode
  session reads `working`; `send::type_into` holds a `Source::Resume`
  message while `opencode::retrying` sees "retrying in <digit>" in the
  bottom 12 lines.
- Stand-down slot: `limits::native_auto_resume(agent, cfg)`, false for all
  three; true makes `resume`, `auto_schedule` and `run_job` skip the
  session.

## Progress

- [x] config, daemon scheduling, RPC, setup.apply/status, OpenCode guard,
      stand-down, tests
- [x] TUI dialog, setup-form row
- [x] docs (guide, architecture), overlord.md

## Owed / open

- The retry-countdown text is matched loosely ("retrying in" + digit);
  not checked against a live OpenCode screen.
- No MCP tool for the overlord to flip the setting (by design: the user's
  choice).
