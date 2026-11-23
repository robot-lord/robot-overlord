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

A third holds for delivery (§4): **one websocket link is one Noise session
(per device link) with implicit nonces; delivery within a connection is
lossless and in order; any reconnect is a fresh `KKpsk2` handshake; and
recovery is resync by the snapshot's `rev` plus resend by request id.**
Nothing switches to explicit nonces to let a session outlive a lost frame:
whatever would lose one closes the link or the connection instead.

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

Close codes: 4205 reset (this connection's link ended under it: the host
dropped it, or it stalled in either direction; the end-to-end session on
it is gone, so reconnect at once with a fresh handshake, backing off only
if it keeps happening), 4401 bad signature or bad hello, 4402 suspended,
4403 not registered / not on the host's allow-list / no such rendezvous,
4408 too slow or idle, 4409 replaced by a newer connection of the same
identity, 4426 unsupported `v`, 4429 over a limit, 4503 overloaded. A
peer that isn't reading may see the socket cut (1006) instead of 4205:
the close frame can't reach it. 4401, 4402, 4403
and 4426 are final (`close::is_final`): a client doesn't retry without a
change. Before closing for load, rate or idleness the relay sends
`{"t":"bye","code","retry_after"?}`; a client waits at least `retry_after`
seconds (plus jitter) before reconnecting.

### After `ready`

Requests carry a number `n` (from 1), answered `{"t":"ok","n"}` or
`{"t":"err","n","code"}` (codes in `msg::code`: `full`, `no_queue`,
`limit`, `bad`, `unknown`, `rate`). A lane-2 frame carries no number;
the relay answers each one, in the order they were sent,
`{"t":"queued","seq","device"?}` when it is queued or
`{"t":"refused","code","device"?}` when it is not (`full`, `no_queue`,
`unknown`, `bad`); `device` (the queue's device fingerprint) only to a
host. A refused frame is not kept: resend it later, before any newer one,
or give up. Only an undecodable binary frame is answered `err` with `n: 0`
(`bad`). Both ends ping (the relay every 25 s, the host every 15 s); any
message, a pong included, says the other end is there.

| From | Message | Meaning |
| --- | --- | --- |
| host | `devices {n, allow: [fingerprint]}` | The whole allow-list, on every connect and change. A device not on it is refused (4403), its connection closed and its queues dropped. At most `devices_per_host`. |
| host | `rendezvous {n, id, ttl}` / `rendezvous_close {n, id}` | Open (at most 300 s, 4 at once per host) or close a pairing rendezvous. |
| host | `signal {n, device}` | Sets the device's flag. No content. A connected device gets `{"t":"signal"}`. |
| host | `drop {n, link}` | Close that device's connection (4205: reconnect now). Queued after that link's frames, so an answer sent just before it arrives first. |
| host | `ack {n, seq, device}` | Delete that device's queued messages to the host up to `seq`. |
| device | `ack_signal {n}` | Clears its flag. |
| device | `ack {n, seq}` | Delete its queued messages up to `seq`. |
| relay → host | `link {id, device, kind: "device"|"pair", rendezvous?}` / `unlink {id}` | A device (or a pairing device) is connected while the host is; frames for it carry `id`. |
| relay → device | `host {state: "online"|"offline"}` | Its host came or went. |
| relay → either | `queued {seq, device?}` / `refused {code, device?}` | The answer to each lane-2 frame, in order (`device` to a host only). |
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
  both connected, the relay gives the pair a link id (never reused while
  the relay runs, even across a host's reconnects) and tells the host
  `link`; when either goes, `unlink`. Lane-1 frames are forwarded at once
  to the other end of the link, or answered `undeliverable`.
- **Backpressure per link, never dropping silently.** A live frame can't
  be dropped (a missing frame ends the Noise session), and one device must
  not hold up its host's others. Device to host: each link has its own
  share of the host's outgoing buffer (64 frames); while a device's share
  is full the relay stops reading that device, and a link whose host takes
  none of its frames for 10 s (`stall`) closes the device (4205), never the
  host. Host to device: the relay never stops reading a host for one link;
  each device's buffer holds 128 frames (a whole 1 MiB control message
  fits), and a device that lets it fill is closed (4205) and the host told
  `undeliverable {link}`. A host's read rate and burst are its
  per-connection limit times (1 + its linked devices), both in
  `ready.limits` (`bytes_per_sec`, `burst_bytes`); sharing them fairly
  between links is the host's job (its writer takes turns between
  per-link queues, and keeps under that rate so its backlog stays in
  them rather than in the sockets' buffers).
- **Mailbox queues** (in memory only; never on disk): per device and
  direction, at most 32 messages, 128 KiB and 600 s each
  (`mailbox_ttl = 0` turns queues off: `refused` `no_queue`). A full queue
  refuses the new message (`refused` `full`); it never drops an older one.
  A restart forgets every queue, and an expired message is gone: the
  receiver's cipher skips such gaps (§3, Frames).
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
  connections per address 8, 256 KiB/s per connection (burst 1 MiB; a
  host's times 1 + its links),
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
current one. One message per frame. The receiver accepts a frame up to
`MAX_SKIP` (64, at least the relay's 32-message queue) messages past its
counter: it steps the counter and `REKEY` once per missing message and
discards the skipped keys. Counters only move forward: a replayed or
older frame, a gap over 64, or any other decrypt failure discards the
mailbox on both ends, and the next live link keys a new one. What a lost
message carried comes back by the device's resend under the same
request id (§4).

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
   `{"event":"hello","version","api","rev","page":"<id>","device":"<id>",
   "mailbox":"<id>"|null}` (`api`: the remote API level; `page`: the
   device's id, the one its `page.report`s are kept under; `mailbox`: the
   base64url 8-byte id of the mailbox the host holds for this device, null
   when it holds none; see step 4), and after that
   `{"event":"changed","rev"}` on every change of the daemon's state, and
   `{"event":"ui","cmd"}` for each overlord UI command that lands on this
   device (§5).
3. Requests are the local control socket's: `{"id","op","args"}`,
   answered `{"id","ok"}` or `{"id","error":{"code","message"}}`. The host
   passes each to the daemon as `web.call {op, args, via: "remote",
   device}`. A device may send 20 ops per second (burst 40); over that the
   answer is `rate_limited`. An id is unique for its device; an op that
   changes something needs one (else `bad_request`) and runs at most once
   per id (§4).
4. Mailbox keying, on the live session: `{"event":"mailbox_key","msg":
   "<kind-3 message 1>"}`, answered `{"event":"mailbox_key","msg":"<message
   2>"}`; the device makes its mailbox, saves it, and sends
   `{"event":"mailbox_ready","frame":"<its first mailbox frame>"}`; when
   that decrypts the host saves the mailbox (replacing any older one).
   A device keys a mailbox when it has none, and when the host's `hello`
   names another one or none (`mailbox`): the host discarded the device's
   (it failed there, or its saved state lost it), so frames sealed under
   it would be dropped by id and their answers never come. The device
   then discards its own first (the same as a failure on its end: a
   refused frame sealed under it is not offered again, and every
   unanswered request goes again by id, live on this session and by the
   new mailbox when the host is next away; the host's request store
   answers a repeat once). A `hello` without the field (an older host)
   changes nothing, and an older device ignores it; the field is additive,
   so `api` stays 1 (the API level is the ops, the snapshot and the `ui`
   commands; this is the protocol beneath them).
5. While a device has no live session and the daemon's `rev` moves, the
   host sends the relay `signal` for it, at most once per 30 s; a move
   inside that window is owed one more, sent when the window ends (unless
   the device went live or was revoked meanwhile). A move of
   the relay status alone (the mirror below: a pairing, a device's link
   coming or going) signals no one: no device sees it.

### Requests by mailbox

Only `overlord.ask` and `overlord.new_session` travel by the mailbox, one
frame each, when the device finds its host offline or has no live
session. The host, for each lane-2 frame: acknowledges and ignores a
frame whose relay `boot` and `seq` it has handled already (delivered
again after a crash); drops (and acknowledges) a frame whose id is not
its current mailbox's; decrypts (a failure discards the mailbox); looks
the request id up in the device's request store (§4) and, if new, records
it as running; saves that, the frame's `(boot, seq)` and the advanced
state in one atomic write (`remote/mailbox-<device id>.json`);
acknowledges the relay; refuses another op (`not_allowed`), else acts (or
answers from the store); and answers by the mailbox with the usual
answer. A crash before the save means the frame is handled again as new;
a crash after it never repeats the op, and the device's resend is
answered `interrupted` (the host can't know whether it happened). (The
plan said the receiver saves after acting; saving the id first is what
makes "never twice" hold.)

The device does the same with what the host sends: it ignores a frame
whose `(boot, seq)` it has opened already (the relay delivers it again
until acknowledged), and acknowledges what it has opened.

Answers leave one at a time per device: the next is encrypted (state
saved first) and sent only once the relay said `queued` for the one
before, so they reach the device in counter order. One refused `full` is
sent again, the same bytes, when the device next links and every 30 s
(at most 20 times; after that the device's resend gets it from the
store). One whose fate a lost connection hides is encrypted afresh: the
device skips the gap, or drops a second answer to the same id.

## 4. Delivery and recovery

The rule: **one websocket link is one Noise session per device link,
with implicit nonces; delivery within a connection is lossless and in
order; any reconnect is a fresh `KKpsk2` handshake; recovery is resync by
snapshot `rev` plus resend by request id.** No explicit nonces: a session
never outlives a lost frame, so nothing may be lost quietly.

- **Host, sending** (`crates/web/src/remote/outq.rs`). Bounded queues per
  connection: control messages (256), mailbox frames (64), and one per
  link (4 MiB). A writer task sends control first, then mailbox frames,
  then one frame per link in turn. A link whose queue overflows is
  dropped (`drop`; only that device's session dies; what was queued for
  it goes). A control message that can't be queued closes the connection
  (it is dialled again; every device does a fresh handshake). A link's
  `drop` waits behind that link's frames.
- **Host, receiving.** A binary frame it can't parse closes the
  connection (skipping it could skip one of a session's frames). An
  unknown text message is skipped (a newer relay's). A relay silent for
  45 s (pings both ways) is gone.
- **Host asleep.** `Instant` doesn't advance while macOS sleeps (Rust's is
  `CLOCK_UPTIME_RAW`; on Linux `CLOCK_MONOTONIC`, which doesn't count
  suspend either), so silence alone would notice a dead connection late
  after a wake. Every second the host compares the wall clock with the
  monotonic one: a tick where the wall clock ran 10 s ahead (or went
  back), or where the monotonic clock itself jumped 10 s (a platform that
  counts sleep, or a stall), closes the connection and dials again at
  once.
- **Relay.** Flow control per link, never blocking a host for one device;
  4205 for a link that ended under a device (§2).
- **Requests at most once** (`crates/web/src/remote/requests.rs`). Each
  device has one bounded request store (64 ids, answers up to 8 KiB, a
  larger one kept as a `too_large` note; the oldest answered id makes
  room), saved in `remote/mailbox-<device id>.json` with the mailbox
  state, and used by both the live and the mailbox path. An op the daemon
  marks idempotent (`web::Op.idempotent`: the look-ups) just runs. Any
  other needs an id; a new id is recorded as running and saved before the
  op runs; a known one gets its stored answer, or `in_progress` while it
  runs, and the answer follows by every way it was asked (the live link
  it came on if still the same, and the mailbox if a resend came that
  way). A request still running when the plugin stopped is answered
  `interrupted`.
- **Device** (the phone slice; `rlord-device` does the protocol parts):
  reconnect on 4205, on a network change and on coming to the
  foreground; run `KKpsk2` each time its host comes online; compare the
  snapshot's `rev` and fetch on a difference; resend unanswered requests
  by id; key a new mailbox when the `hello`'s `mailbox` isn't the one it
  holds (§3, step 4); when the relay's `boot` changes, resend every mailbox request it
  has no answer to (the queue is gone); drop mailbox frames by `(boot,
  seq)` and answers by request id it already has.

## 5. Host and daemon

The daemon holds no keys and does no crypto.

- **Remote requests.** `web.call {op, args, via, device?}`: `via` is
  `"local"` (a page; with `page`) or `"remote"` (a device; with `device`,
  its id); anything else is refused. `web::Op` has `remote: bool`; an op
  with `remote: false` is refused (`NotAllowed`) from a device. A device
  is a surface of its own (docs/architecture.md, "The phone as a
  surface"): its id stands for its page in `page.report` and
  `page.pending`, its user-driven ops count as its use, and UI commands
  for it come on the `web.subscribe` stream as `{"notify":"ui","device",
  "n","cmd"}`; the plugin sends them on its live session and says
  `web.ui_delivered {device, n}`, or signals it. `device.leave` removes
  the calling device. `plugin::WEB_PROTOCOL` is 4. `web::Op.idempotent`
  marks the look-ups (and `page.report`); `web.hello`'s answer lists the
  remote ones (`"idempotent": [op]`) and gives `"api"`, and the plugin
  runs any op not listed at most once per request id (§4).
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
  `relay.devices` from it, calls `changed_relay()` (moves `rev`; the
  `web.subscribe` line says `"relay": true` when nothing else changed),
  and sends a notice to every surface when a new `pending` appears.
- **Local page ops** (`remote: false`): `device.list`, `device.pair`,
  `device.pair_cancel`, `device.confirm`, `device.reject`,
  `device.revoke`, answered by the same path.
