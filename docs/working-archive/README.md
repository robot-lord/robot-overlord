# Archived working docs

A working doc (`docs/working/<effort>.md`) is removed when its effort is
done: what lasts is promoted into `docs/`, and the original stays in git
history. A residue file is kept here only when something is still owed.

| Doc | Subject | Reconciled into | Residue | Date |
|---|---|---|---|---|
| `single-overlord.md` | The two-session overlord (front desk + mastermind) becomes one session | `docs/guide.md` ("The overlord"), `docs/architecture.md` ("The overlord"), `CLAUDE.md` | [single-overlord.md](single-overlord.md): owed items | 2026-11-12 |
| `overlord-swap.md` | One overlord per agent with a hot swap; resume after a usage limit | `docs/architecture.md` ("Usage limits", "One per agent, one active"), `docs/guide.md` | [overlord-swap.md](overlord-swap.md): owed items | 2026-11-12 |
| `codex-daemon.md` | Codex sessions on Codex's shared background server (daemon mode); Codex Remote Control deferred | `docs/architecture.md` ("Codex on its shared background server", and the "Typing into a pane" bullet), `docs/guide.md` ("Codex sessions") | [codex-daemon.md](codex-daemon.md): owed items (before-launch checks in `docs/working/public-launch.md` item 7), Remote Control findings, branch `codex-remote` | 2026-11-13 |
| `usage-column.md` | The list's Usage column (turns, tokens, context %), its sort and measure switch | `docs/architecture.md` ("Usage (the list's Usage column)") | none (context-window upkeep: issue #56) | 2026-11-14 |
| `own-prompts.md` | rlord's own pastes no longer count as the user's prompts | `docs/architecture.md` ("rlord's own prompts") | [own-prompts.md](own-prompts.md): owed live samples and matching gaps | 2026-11-14 |
| `linux-and-packaging.md` | Linux builds, packages, installers, release channels | `CONTRIBUTING.md`, `xtask/src/version.rs` | [linux-and-packaging.md](linux-and-packaging.md): tmux minimum audit, Arch, git author emails | 2026-11-14 |
