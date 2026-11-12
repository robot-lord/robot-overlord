# One overlord: what is still owed

Residue of `docs/working/single-overlord.md` (in git history), reconciled
2026-11-12 with 0.21.0-dev.1. How the overlord works now is in
`docs/guide.md` ("The overlord") and `docs/architecture.md` ("The
overlord"); the rules for changing it are in `CLAUDE.md`.

## Owed

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
