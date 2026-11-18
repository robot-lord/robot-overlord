# The relay (`rlord-relay`)

A relay lets a paired device (a phone, another computer) reach your rlord
host when neither can reach the other directly. Both dial out to the
relay; the relay joins their connections. It is a separate program from
`rlord`, and you run your own.

**The protocol is unreviewed.** It is our own composition (the wire
protocol is in `docs/working/e2e-protocol.md`). Use it with your own relay
and your own devices until someone has reviewed it.

Contents: [What the relay sees](#what-the-relay-sees) ·
[Running it](#running-it) · [DNS and TLS](#dns-and-tls) ·
[Registering hosts](#registering-hosts) · [Limits](#limits) ·
[Backup](#backup) · [Upgrades](#upgrades) · [Commands](#commands)

## What the relay sees

Content between a host and its devices is end-to-end encrypted. The relay
holds no content keys and has no code that could decrypt it.

**It never sees:** what you type or read, session names, device names, the
overlord's answers, or any key that protects them. It never writes a
payload to disk and never logs one.

**It does see** metadata:

- each host's and device's relay key fingerprint (a key used only to
  connect to this relay, not a content key);
- which devices belong to which host (the host sends its device list so
  the relay can refuse anyone else);
- connection times, IP addresses, and how much data passes and when (to
  the size bucket the padding leaves);
- that a host has something for a device (a flag with no content).

**Accounts.** A self-hosted relay puts every host in one account, `local`,
and knows only fingerprints. Tokens can carry an account label instead
(`token new --account team-a`) to count and suspend hosts per team. A
relay run as a service with logins would also learn who owns which hosts,
how many hosts and devices each person has, and, with billing, who pays.
It would still learn no content and no names. So a hosted relay is less
private than a self-hosted one in metadata, and equal in content.

An account never controls the keys. A device is allowed by its host (the
host's device list and its own records), never by an account. A relay or
account service that turns hostile can refuse service; it cannot add a
device to your host or read anything.

**What it keeps.** On disk, in the data directory: `relay.key` (its
identity) and `relay.db` (registered hosts with their account, status and
first and last seen; each host's device list; enrolment tokens, stored
hashed). In memory only: connections, pairings in progress, the "something
for you" flags and short mailbox queues of encrypted messages. A restart
forgets those; hosts and devices reconnect and resend.

It never connects out to anything. Hosts and devices always dial it.

## Running it

The relay needs a public DNS name and a TLS certificate for it. There are
two ways to run it.

### Docker compose (Caddy in front)

`deploy/relay/` has a `compose.yml` that runs the relay behind Caddy.
Caddy gets and renews the certificate; the relay runs in proxy mode on
the internal network.

1. Point the DNS name at the machine (below). Open ports 80 and 443.
2. Copy `compose.yml`, `Caddyfile` and `relay.toml.example` (as
   `relay.toml`) to a folder on the machine.
3. Put your name in `Caddyfile` and in `relay.toml` (`origin =
   "https://relay.example.com"`).
4. `docker compose up -d`.
5. `docker compose exec relay /rlord-relay address` prints the relay
   address to paste into `rlord relay setup`.

The image is `ghcr.io/robot-lord/rlord-relay:<version>` (and `:latest`
for the newest stable release), for linux/amd64 and linux/arm64. It holds
only the static binary and runs as a non-root user. The data directory
`/var/lib/rlord-relay` is a named volume.

### Tarball and systemd

Each release has `rlord-relay-v<version>-<target>.tar.gz` for x86_64 and
aarch64 Linux (static binaries). Check it against `SHA256SUMS` (and `gh
attestation verify`), then:

1. Copy `rlord-relay` to `/usr/local/bin/` and `relay.toml.example` to
   `/etc/rlord-relay/relay.toml`.
2. Edit `origin`. Either set `[tls] mode = "files"` with your certificate
   files (the relay serves TLS itself on 443), or keep proxy mode behind
   your own reverse proxy.
3. Install `rlord-relay.service` in `/etc/systemd/system/`, then
   `systemctl enable --now rlord-relay`.
4. `rlord-relay check --config /etc/rlord-relay/relay.toml` checks the
   setup; `rlord-relay address --config ...` prints the address.

## DNS and TLS

- **The origin is the pin.** Hosts and devices check that the relay they
  reach names exactly the origin they were given, and proves the key they
  were given. `origin` must be how they reach it: `https://`, the DNS name
  in lower case, no path, and a port only when it is not 443. A wrong
  origin fails every connection as "wrong relay". `rlord-relay check`
  prints the address it will announce.
- **The certificate** must be valid for that name from a public CA (hosts
  and devices check it like a browser does). A relay on premises can use
  a private CA if each host sets `[remote] ca_file`.
- **Files mode** (`[tls] mode = "files"`): the relay terminates TLS
  (TLS 1.3 and 1.2). It re-reads `cert` and `key` on SIGHUP (`systemctl
  reload rlord-relay`) and every 12 hours, so a renewal needs no restart.
  A broken file keeps the old certificate in use and logs a warning.
- **Proxy mode** (`[tls] mode = "proxy"`): plain HTTP for a reverse proxy
  that terminates TLS. It listens on `127.0.0.1:8080` unless `listen` says
  otherwise. The proxy must pass websocket upgrades on `/v1` and set
  `X-Forwarded-For` (the per-address limit uses its last entry). The proxy
  must not compress the websocket.
- The relay's key proves the relay, not the certificate. Someone who can
  get a valid certificate for your name and sit in front of the relay
  could see its metadata. They could not read content or pose as the
  relay.

## Registering hosts

`[hosts] registration`:

- `"token"` (the default): a host's first connection must carry a token.
  `rlord-relay token new` prints one (one use, good for 24 hours; `--uses`,
  `--ttl 7d`, `--account NAME`). Give it to `rlord relay setup` on the
  host. After that the host is known by its key.
- `"open"`: any host may register, up to `max_hosts`. Anyone who finds the
  relay can use it, so this is not recommended.
- `"closed"`: only hosts added by hand: `rlord-relay host add
  <fingerprint>`.

`rlord-relay host list` shows registered hosts. `host suspend` refuses a
host and its devices (close code 4402, which they don't retry) until
`host resume`; `host remove` forgets it and its device list. Both apply to
connected hosts within 30 seconds. A host not seen for `host_expiry_days`
(90; 0 = never) is forgotten.

Devices are never registered with the relay directly. A host adds them
when you pair and confirm them, and removes them when you revoke them.

## Limits

All under `[limits]` in `relay.toml`, with these defaults:

| Setting | Default | What |
| --- | --- | --- |
| `max_hosts` | 1000 | registered hosts |
| `max_connections` | 10000 | open connections; past it, HTTP 503 with `Retry-After` |
| `devices_per_host` | 16 | devices on one host's list |
| `unauthenticated_per_address` | 8 | connections from one address still connecting |
| `bytes_per_second`, `burst_bytes` | 262144, 1048576 | per connection; a faster sender is read more slowly |
| `mailbox_messages`, `mailbox_bytes`, `mailbox_ttl` | 32, 131072, 600 | each mailbox queue (per device and direction); `mailbox_ttl = 0` turns queues off |
| `rendezvous_per_host`, `rendezvous_ttl`, `rendezvous_attempts` | 4, 300, 3 | pairings a host has open, their longest life in seconds, and attempts each |
| `auth_timeout` | 5 | seconds to finish connecting |
| `ping_interval`, `idle_timeout` | 25, 60 | seconds; a silent connection is closed |
| `stall_timeout` | 10 | seconds a receiver may take no data before it is closed |
| `host_expiry_days` | 90 | forget unused hosts (0 = never) |

A full queue refuses new messages; it never drops older ones. The relay
never drops a live message either: when a receiver is slow it stops
reading the sender, and closes a receiver that takes nothing for
`stall_timeout`. Before closing for load, a limit or idleness it tells
the client how long to wait (`bye`).

## Backup

Back up the data directory (`/var/lib/rlord-relay`): `relay.key` and
`relay.db`.

- **Losing `relay.key` means every host sets up again.** Every host and
  device has pinned the key's fingerprint; a new key is a different relay
  to them. Keep a copy somewhere safe and private (it is a private key;
  the relay refuses to start if it is readable by others).
- Losing `relay.db` forgets registrations and device lists. Hosts send
  their device lists again on every connect, but each host needs a new
  token (or `host add`) to register again.
- Copy `relay.db` with the relay stopped, or with `sqlite3 relay.db
  ".backup copy.db"` while it runs.

## Upgrades

Replace the binary (or pull the new image) and restart. Hosts and devices
reconnect on their own; nothing in memory needs keeping. The relay link
has its own protocol version: a relay that doesn't speak a client's
version refuses it with close code 4426, and the host says so instead of
retrying.

## Commands

All take `--config PATH` (default `/etc/rlord-relay/relay.toml`).

| Command | What |
| --- | --- |
| `rlord-relay serve [--listen IP:PORT]` | run; prints `listening on …` and `address …` once bound |
| `rlord-relay address` | the relay address (origin and key fingerprint) to paste into `rlord relay setup` |
| `rlord-relay token new [--uses N] [--ttl 24h] [--account NAME]` | an enrolment token |
| `rlord-relay host list` / `add <fp> [--account NAME]` / `remove` / `suspend` / `resume` | registered hosts |
| `rlord-relay check` | load the config, open the store, prove the key, load the certificate |
| `rlord-relay --version` | |

The log goes to stderr (`RUST_LOG`, default `info`). It names keys by the
first 8 hex characters of their fingerprint and never logs payloads, even
at `trace`.
