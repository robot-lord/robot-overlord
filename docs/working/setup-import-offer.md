**Status:** in progress

# First-run import offer

Guide new users to importing their existing agent conversations, which
until now only the overlord offered when asked.

## Flow

1. After the first-run code-folder form closes (saved, Ctrl-D, or sent to
   the overlord; not Esc), or at startup when setup is already answered,
   the TUI calls `setup.import_offer` (cheap: one kv row, the overlord row,
   the session list). Any setup-form close counts (first-run or `:setup`).
2. The daemon offers only while kv `setup_import_offer` is unset, an
   overlord exists, and there are no sessions besides overlords (archived
   ones count). No scan in rlord: the overlord's flow does that.
3. Once nothing else is open, the TUI shows a dialog explaining import:
   `y` import my sessions, `d` don't ask again, Esc later. Keys in its first 800ms are dropped; quitting and staying brings it back.
4. `y`: the overlay opens and, in the background, `setup.import_answer
   {answer:"yes"}` sends the overlord the fixed `[rlord import request]`
   text, then records `yes:` in kv. A failed send records nothing (offered
   again next start; the offer reopens with the error). `d` records `no:`.
5. `overlord.md` routes `[rlord import request]` to "Importing existing
   work": `rlord-import` proposes (scan covers every importable agent; it
   says which agents had any and which installed agents can't be imported
   yet), the user approves, then it imports.

## Decisions (user's)

- Trigger only during initial setup (no sessions besides the overlord), no
  count in the TUI.
- The request text is short and agent-neutral; `overlord.md` carries the
  procedure.

## Owed / untested live

- The `y` path (overlay + delivery to a live overlord) and the dialog's
  look are untested against a live daemon and overlord.
- If the overlord's `confirm_setup` reply reopens the setup form while the
  offer is showing, the offer closes unrecorded and asks again next start.
- Codex/OpenCode import isn't supported yet; the overlord says so.
