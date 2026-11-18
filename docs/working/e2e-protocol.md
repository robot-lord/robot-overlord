**Status:** in progress (slice 3 of `web-ui-remote.md`; written while building, to be read by a reviewer, then promoted)

# rlord remote protocol, v1

The whole protocol in one place: the relay link, what the relay does, the
end-to-end layer between a host and a device, and how remote requests
reach the daemon. Decisions and their reasons are in `e2e-crypto.md` and
the slice 3 plan in `web-ui-remote.md`; this is what is built.

**Unreviewed.** It is a composition of our own (the hybrid handshake, the
mailbox cipher over raw keys, the relay's signed transcripts). Nobody but
the user runs it before a review.

## Parties and crates

| Party | Binary | Crates | Holds |
| --- | --- | --- | --- |
| Host | `rlord-web` (the opt-in plugin) | `rlord_e2e`, `rlord_relay_proto` | the host's X25519 static key, its P-256 relay key, device records, mailbox states |
| Device | the phone app (next slice); `rlord-device` (test client) | `rlord_e2e`, `rlord_relay_proto` | its X25519 static key, its P-256 relay key, the host's static key and the pin |
| Relay | `rlord-relay` | `rlord_relay_proto` only | its P-256 key, registrations, in-memory queues |
| Daemon | `rlord` | neither | no keys; forwards user commands to the plugin, answers ops |

Both invariants of `web-ui-remote.md` hold: everyone dials out to the relay
(the relay is the only listener reachable from the network; `rlord-web`'s
one listener stays `127.0.0.1`), and a host and its devices only ever talk
to one pinned relay.

## 1. The relay link (`crates/relay-proto`)

One websocket per peer, `wss://<host>/v1` (or `ws://127.0.0.1:<port>/v1`
for a loopback relay), opened by the peer. No redirects (a 3xx is a failed
connection), no `permessage-deflate` offered or accepted, exactly one URL,
built from the pin. TLS must validate for the origin's host against
`webpki-roots` plus `[remote] ca_file` when set. No channel binding to TLS
in v1.

### The relay address and the pin

`https://relay.example.com#k=<base64url fingerprint>`: the origin and
SHA-256 of the relay's SEC1 uncompressed P-256 public key. The origin is
normalised (`address::normalize_origin`): scheme `https` (`http` only for
`127.0.0.1` or `[::1]`), lower-case host, no userinfo, path, query or
fragment, no default port, at most 255 bytes. Peers compare origins byte
for byte.

### Connecting (5 s from upgrade to `ready`)

All control messages are JSON text frames (at most 4 KiB) tagged by `"t"`;
bytes are base64url without padding.

1. Peer: `{"t":"hello","v":1,"role":"host"|"device"|"pair","key":<SEC1
   P-256, 65 bytes>,"nonce":<32 bytes>,"host"?:<host fingerprint>,
   "rendezvous"?:<16 bytes>,"enroll"?:{"kind":"token","token":"…"}}`.
   A device names its host's relay key fingerprint; a pairing device the
   rendezvous id; a host enrolling for the first time an `enroll` object.
2. Relay: `{"t":"challenge","origin","key","nonce","sig"}`, `sig` over the
   relay transcript.
3. Peer checks, in this order and before sending anything else
   (`check_challenge`): origin equals the pin's byte for byte; the key's
   fingerprint equals the pin's; the signature verifies. Any miss closes the
   socket and is "wrong relay", never retried against anything else and
   not retried at all until the configuration changes. Then
   `{"t":"auth","sig"}` over the peer transcript.
4. Relay: `{"t":"ready","boot","host"?,"signal"?,"queued"?,"limits"?}`
   (the middle three for a device), or `{"t":"enroll","url","code",
   "expires"}` first (reserved for account login; a client shows it and
   waits for `ready`), or a close code.

Transcripts (`auth.rs`): a label, then each field as `u16` big-endian
length and bytes. ECDSA P-256 over SHA-256, RFC 6979, 64 bytes `r || s`.

- relay: `"rlord-relay/1 relay"`, origin, relay key, peer key, peer nonce,
  relay nonce.
- peer: `"rlord-relay/1 peer"`, role, origin, relay key, peer key, relay
  nonce, peer nonce, target (the host fingerprint for a device, the
  rendezvous id for a pairing device, empty for a host).

The origin and key a peer proved are what go into its Noise prologues.

Close codes: 4401 bad signature or bad hello, 4402 suspended, 4403 not
registered / not on the host's allow-list / no such rendezvous, 4408 too
slow or idle, 4409 replaced by a newer connection of the same identity,
4426 unsupported `v`, 4429 over a limit, 4503 overloaded. 4401, 4402, 4403
and 4426 are final (`close::is_final`): a client doesn't retry without a
change. Before closing for load, rate or idleness the relay sends
`{"t":"bye","code","retry_after"?}`; a client waits at least `retry_after`
seconds (plus jitter) before reconnecting.

### After `ready`

Requests carry a number `n`, answered `{"t":"ok","n"}` or
`{"t":"err","n","code"}` (codes in `msg::code`: `full`, `no_queue`,
`limit`, `bad`, `unknown`, `rate`).

| From | Message | Meaning |
| --- | --- | --- |
| host | `devices {n, allow: [fingerprint]}` | The whole allow-list, on every connect and change. A device not on it is refused (4403), its connection closed and its queues dropped. At most `devices_per_host`. |
| host | `rendezvous {n, id, ttl}` / `rendezvous_close {n, id}` | Open (at most 300 s, 4 at once per host) or close a pairing rendezvous. |
| host | `signal {n, device}` | Sets the device's flag. No content. A connected device gets `{"t":"signal"}`. |
| host | `drop {n, link}` | Close that device's connection. |
| host | `ack {n, seq, device}` | Delete that device's queued messages to the host up to `seq`. |
| device | `ack_signal {n}` | Clears its flag. |
| device | `ack {n, seq}` | Delete its queued messages up to `seq`. |
| relay → host | `link {id, device, kind: "device"|"pair", rendezvous?}` / `unlink {id}` | A device (or a pairing device) is connected while the host is; frames for it carry `id`. |
| relay → device | `host {state: "online"|"offline"}` | Its host came or went. |
| relay → either | `undeliverable {link?}` | A live frame had no other end. |

### Binary frames (at most 20 KiB; the lane first)

| Direction | Lane 1, live | Lane 2, mailbox |
| --- | --- | --- |
| device → relay | `1 \| payload` | `2 \| payload` |
| relay → device | `1 \| payload` | `2 \| seq u64 \| payload` |
| host → relay | `1 \| link u32 \| payload` | `2 \| device fingerprint (32) \| payload` |
| relay → host | `1 \| link u32 \| payload` | `2 \| device fingerprint (32) \| seq u64 \| payload` |

Big-endian integers. The payload is `rlord_e2e`'s bytes; the relay never
parses it. (The plan put the lane after the address; it comes first so a
frame parses without knowing its lane in advance.)

## 2. What the relay does (`crates/relay`)

- **Registration.** A host connects with its relay key. The `Authorizer`
  admits it (an account, its limits), refuses it (a close code) or (later)
  answers `enroll`. `StaticAuthorizer`: `token` (default: an unknown host
  must present an unused, unexpired token from `rlord-relay token new`; the
  host record is written with the token's account), `open` (any host, up
  to `max_hosts`), `closed` (only `rlord-relay host add`). A suspended host
  and its devices get 4402.
- **Devices.** A device connects naming its host's fingerprint. It is
  admitted only when its key fingerprint is on that host's allow-list (the
  last `devices` the host sent, stored), else 4403. It belongs to its
  host's account.
- **Pairing devices.** Admitted only for an open rendezvous of a connected
  host; one pairing link at a time per rendezvous; the third attempt
  closes the rendezvous; it expires after its `ttl`.
- **Links.** While a host and one of its devices (or a pairing device) are
  both connected, the relay gives the pair a link id (unique per host
  connection) and tells the host `link`; when either goes, `unlink`. Lane-1
  frames are forwarded at once to the other end of the link, or answered
  `undeliverable`.
- **Backpressure, never dropping.** A live frame can't be dropped (a
  missing frame ends the Noise session). Each connection's outgoing buffer
  holds 64 frames; when the receiver's buffer is full the relay stops
  reading from the sender; a link whose receiver has not taken a frame for
  10 s is closed (`bye` with code `idle`, then the connection).
- **Mailbox queues** (in memory only; never on disk): per device and
  direction, at most 32 messages, 128 KiB and 600 s each
  (`mailbox_ttl = 0` turns queues off: `err` `no_queue`). A full queue
  refuses the new message (`err` `full`); it never drops an older one.
  Messages get increasing `seq` numbers per queue; a message is delivered
  when its recipient is connected (at once, or on connect after `ready`),
  and kept until acknowledged or expired.
- **Signals and `boot`.** The flag per device is in memory; `boot` is a
  random id per relay process. A device that reads `signal: 0`,
  `queued: 0` and the `boot` it saw last has nothing to fetch.
- **Identities.** One connection per identity; a newer one replaces the
  older (4409).
- **What it keeps.** On disk, through the `Store` trait (SQLite in v1):
  hosts (fingerprint, account, status, first and last seen, allow-list),
  enrolment tokens (hashed, with their account, uses and expiry), and
  `relay.key`. In memory: connections, links, rendezvous, flags, queues,
  the `Meter`. Never content keys, names, plaintext or payload logs; the
  log names fingerprints by their first 8 hex characters.
- **Limits** (`relay.toml` `[limits]`, per account through the
  `Authorizer`): hosts 1000, devices per host 16, unauthenticated
  connections per address 8, 256 KiB/s per connection (burst 1 MiB),
  queues as above, rendezvous 300 s and 4 per host, ping every 25 s and
  closed after 60 s of silence, unused hosts expire after 90 days,
  `max_connections` (at the cap an upgrade gets HTTP 503 with
  `Retry-After` before any signature work).

## 3. Host and device (`crates/e2e`, carried in lane payloads)

### Handshakes

`Noise_IKpsk2_25519_ChaChaPoly_SHA256` (pairing, kind 1) and
`Noise_KKpsk2_25519_ChaChaPoly_SHA256` (live reconnect, kind 2; mailbox
keying, kind 3). Message 1: `1 | kind | noise` with the device's fresh
ML-KEM-768 encapsulation key as its encrypted payload. Message 2:
`1 | kind | KEM ciphertext (1088) | noise`, empty payload; the PSK for
`psk2` is `SHA-256("rlord-e2e/1 pair psk" | pairing secret | ss | ct)`
(pairing) or `SHA-256("rlord-e2e/1 kk psk" | ss | ct)`. Prologue:
`"rlord-e2e/1 device->host" | 1 | kind | origin length (u8) | origin |
relay key fingerprint | id`, the id being the rendezvous id (pairing) or
`channel_id = SHA-256("rlord-e2e/1 channel" | host static | device
static)[..16]`. The origin and fingerprint are those this connection
proved. A session exists only once the device's first transport frame
decrypts at the host.

### Frames

Live: `u16 length | ciphertext`, the plaintext `u16 length | content |
zero fill` padded to a multiple of 32; rekey every 2^20 frames each way.
Any decrypt failure (a malformed frame included) ends the session: the
owner closes the link. Content: `stream id | flags | bytes`. Stream 0 is
control; 1 and up are reserved for panes (the host closes a link that uses
one). Flag bit 0 is "more follows"; a control message up to 1 MiB is
fragmented and reassembled; other bits must be zero.

Mailbox: the same frame shape padded to a Padmé bucket, its own cipher
state (ChaCha20-Poly1305 with a counter nonce, Noise's `REKEY` after every
message) over the kind-3 handshake's raw split keys, exported as 89 bytes
(`1 | id (8) | send key | send counter u64 | receive key | receive counter
u64`). A lane-2 payload is `mailbox id (8) | frame`, the id
`SHA-256("rlord-e2e/1 mailbox id" | handshake hash)[..8]` in clear, so a
frame for a replaced mailbox is dropped by id rather than poisoning the
current one. One message per frame. A decrypt failure or a gap discards
the mailbox on both ends; the next live link keys a new one.

### Pairing (a `pair` link)

1. The host opens a rendezvous and shows `RLORD1:` + base64url(`1 |
   rendezvous id (16) | host static (32) | pairing secret (32) | relay
   fingerprint (32) | origin`), valid for 300 s and one device.
2. The device connects with role `pair` (checking the relay against the
   QR's pin), sends pairing message 1 on lane 1; the host answers message
   2 (not yet a session).
3. The device's first transport frame, control stream:
   `{"event":"pair","name":"…","key":"<its relay public key>"}`. When it
   decrypts the host checks the key's fingerprint is the link's `device`
   (else the relay swapped something: drop), clips the name (40
   characters, control characters removed), and answers
   `{"event":"pair","state":"waiting","code":"123456"}`. The device shows
   the same six digits (`short_code`).
4. The user confirms on the host (CLI, TUI or local page). The host stores
   the device, sends the relay the new allow-list, answers
   `{"event":"pair","state":"done","host":"<host name>","host_key":"<host
   relay fingerprint>"}`, closes the rendezvous and drops the link. On
   reject, 120 s without an answer, or the link ending first:
   `{"event":"pair","state":"rejected"}`, nothing stored.
5. A pairing link that ends before its first transport frame decrypts
   counts as a failed attempt; the third closes the rendezvous.

The device id is `device_id(static)`: 16 hex characters of a labelled hash.

### A live session (a `device` link)

1. The device sends reconnect message 1 when it wants a session (not on
   every connect: the cheap check needs none). The host refuses a link
   whose fingerprint is not one of its devices (`drop`) and answers with
   the device's stored static key, so a revoked or swapped device fails.
2. The device's first frame: `{"event":"hello"}`. The host answers
   `{"event":"hello","version","rev","device":"<id>"}`, and after that
   `{"event":"changed","rev"}` on every change of the daemon's state.
3. Requests are the local control socket's: `{"id","op","args"}`,
   answered `{"id","ok"}` or `{"id","error":{"code","message"}}`. The host
   passes each to the daemon as `web.call {op, args, via: "remote",
   device}`. A device may send 20 ops per second (burst 40); over that the
   answer is `rate_limited`.
4. Mailbox keying, on the live session: `{"event":"mailbox_key","msg":
   "<kind-3 message 1>"}`, answered `{"event":"mailbox_key","msg":"<message
   2>"}`; the device makes its mailbox, saves it, and sends
   `{"event":"mailbox_ready","frame":"<its first mailbox frame>"}`; when
   that decrypts the host saves the mailbox (replacing any older one).
   A device keys a mailbox when it has none.
5. While a device has no live session and the daemon's `rev` moves, the
   host sends the relay `signal` for it, at most once per 30 s.

### Requests by mailbox

Only `overlord.ask` and `overlord.new_session` travel by the mailbox, one
frame each, when the device finds its host offline or has no live
session. The host, for each lane-2 frame: drops (and acknowledges) a frame
whose id is not its current mailbox's; decrypts (a failure discards the
mailbox); drops a request id it has seen among the device's last 64; saves
the state and the id; acknowledges the relay; acts; answers by the mailbox
with the usual answer (saving the advanced state before sending). A crash
loses at most that message; the device resends by id, and a repeat is
answered `{"error":{"code":"repeat"}}` rather than acted on twice.

## 4. Host and daemon

The daemon holds no keys and does no crypto.

- **Remote requests.** `web.call {op, args, via, device?}`: `via` is
  `"local"` (a page; with `page`) or `"remote"` (a device; with `device`,
  its id); anything else is refused. `web::Op` has `remote: bool`; an op
  with `remote: false` is refused (`NotAllowed`) from a device. A remote
  call carries no `page`, never counts as use of a screen and gets no `ui`
  commands. `plugin::WEB_PROTOCOL` is 3.
- **User commands** (`relay.*`, `crates/daemon/src/relay.rs`; never on
  `OPS`, never in `mcp.rs`): `relay.status`, `relay.setup {address,
  token?, reset?}`, `relay.pair`, `relay.pair_cancel`, `relay.confirm`,
  `relay.reject`, `relay.devices`, `relay.revoke {device}`,
  `relay.disable`. Those that need the plugin are forwarded on the
  `web.subscribe` stream as `{"notify":"remote","id":"<n>","req":{"cmd":
  "setup"|"pair"|"pair_cancel"|"confirm"|"reject"|"revoke"|"disable", …}}`
  (never coalesced), and the daemon waits up to 10 s for the plugin's
  `web.remote_reply {id, ok: <value>}` or `{id, error: "<message>"}`.

  | `req` | `ok` | Notes |
  | --- | --- | --- |
  | `{cmd:"setup", address, token?, reset}` | `{origin, fingerprint}` | The plugin parses the address, makes the keys and `pin.json` (`reset`: deletes `remote/` first; without it, refuses when keys for another relay exist). The daemon then writes `[remote] relay`, `relay_key` and `enabled = true`. The daemon starts the plugin first if it isn't running. |
  | `{cmd:"pair"}` | `{qr, expires}` | `expires` in Unix seconds. The daemon adds `qr_rows` (the QR matrix, `rlord_core::qr`) for the page. |
  | `{cmd:"pair_cancel"}` | `{}` | |
  | `{cmd:"confirm"}` / `{cmd:"reject"}` | `{device: {id, name}}` / `{}` | The one pairing waiting for confirmation. |
  | `{cmd:"revoke", device}` | `{}` | `device`: its id. |
  | `{cmd:"disable"}` | `{}` | The daemon also writes `enabled = false`. |

  Times in the mirror (`paired_at`, `last_seen`, `expires`) are Unix
  seconds; `last_seen` is null for a device never seen since pairing.
- **The mirror.** The plugin reports with `web.remote_state {relay,
  connected, error, devices: [{id, name, paired_at, last_seen, linked}],
  pairing: {expires} | null, pending: {name, code, expires} | null}` on
  every change. The daemon keeps it, answers `relay.status` and
  `relay.devices` from it, calls `changed()`, and sends a notice to every
  surface when a new `pending` appears.
- **Local page ops** (`remote: false`): `device.list`, `device.pair`,
  `device.pair_cancel`, `device.confirm`, `device.reject`,
  `device.revoke`, answered by the same path.
