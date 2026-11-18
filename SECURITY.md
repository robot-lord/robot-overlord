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

Remote access is opt-in on top of the plugin (`rlord relay setup`): the
plugin then dials out to one relay you choose and pin, and nothing
remote ever connects to your machine. A paired device can do what a
local page can, except change `config.toml`, see the keys or manage
devices: pairing a device grants it control of your sessions, so each
pairing needs your confirmation on the host (a six-digit code shown on
both). The relay never holds a content key: it sees IP addresses, key
fingerprints, which device reaches which host, when and how much (to the
padding bucket), and, on a relay with accounts, which account owns which
hosts; never names, sessions or content. The protocol
(`docs/working/e2e-protocol.md`) has not been reviewed yet. Reports in
scope: a relay (or anything in between) reading or changing content,
reaching a host through another relay than its pin, adding a device
without the user's confirmation, or a device reaching an op it isn't
given; and in `rlord-relay` itself, anything that lets one host or
device affect another's traffic or registration.
