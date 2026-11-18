**Status:** in progress

# First-run import offer

Guide new users to importing their existing agent conversations, which
until now only the overlord offered when asked.

## Flow

1. When the code-folder form closes (first run or `:setup`; saved, Ctrl-D
   or sent to the overlord; not Esc), or at startup when setup is already
   answered, the TUI calls `setup.import_offer`.
2. The daemon offers only while kv `setup_import_offer` is unset, an
   overlord exists, there are no sessions besides overlords (archived ones
   count), and at least one Claude Code conversation exists outside rlord's
   own and temp folders (`import::has_claude_conversations`: names only,
   bounded, stops at the first). Only Claude Code can be imported so far.
3. The dialog explains import: `y` import my sessions, `d` don't ask
   again, Esc later. Keys in its first 800ms are dropped; quitting and
   staying brings it back. On first run the overlay waits until the offer
   closes, so the offer never opens under it.
4. `y`: the overlay opens and, in the background, `setup.import_answer
   {answer:"yes"}` sends the overlord the fixed `[rlord import request]`
   text, then records `yes:`. A failed send records nothing and the offer
   reopens with the error. An answer already `yes` sends nothing again.
   `d` records `no:`; if that fails, the offer reopens with the error.
5. `overlord.md` routes `[rlord import request]` to "Importing existing
   work": `rlord-import` proposes, the user approves, then it imports. If
   nothing is found, the overlord says only Claude Code import is supported
   so far.

## Decisions (user's)

- Trigger only during initial setup (no sessions besides the overlord).
- The request text is short and agent-neutral; `overlord.md` carries the
  procedure.

## Owed / untested live

- The `y` path (overlay + delivery to a live overlord) and the dialog's
  look are untested against a live daemon and overlord.
- A UI push (e.g. the overlord's `confirm_setup`) can replace the open
  offer; it closes unrecorded and asks again next start.
- The offer is checked once per run; if the overlord is created after that
  check, it waits for the next start.
- `offer` lists sessions instead of a COUNT query; `y`/`d` could share a
  `spawn_call` helper with the other background sends.
