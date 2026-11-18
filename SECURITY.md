# Security

Please report security problems privately, not in a public issue: use
GitHub's **Report a vulnerability** button on this repository's Security
tab. Include what you found, how to reproduce it, and the rlord version
(`rlord --version`).

You'll get a reply once it has been looked at. Fixes ship as a new
release; `rlord update` installs it.

rlord is meant to be run by its user on a machine they trust, or over a
remote connection they trust. The overlord acts on the user's sessions
without asking (see `docs/guide.md`); reports that depend on an attacker
already controlling that machine or the user's agent sessions are out of
scope.

The web UI is an opt-in plugin (`rlord web install`); without it, rlord
has no network listener. With it, the plugin listens on `127.0.0.1`
only, and its link's token gives whoever holds it the same control of
your sessions as your terminal: the page can launch and fork sessions
directly (in any folder, with any prompt) and save the code folder to
`config.toml`, as the list's own forms do. Reports about the plugin reaching
beyond `127.0.0.1`, a page from another site driving it, or rlord running
a plugin other than the one it was released with are in scope.
