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
