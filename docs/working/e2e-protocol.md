**Status:** in progress (slices 3 and 4 of `web-ui-remote.md`; written while building, the device side as built by the engine in slice 4; to be read by a reviewer, then promoted)

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
control; 1 to 250 are pane streams ("Panes" below); 251 to 255 are
reserved, and a frame on one is a protocol error (the link is closed).
Flag bit 0 is "more follows"; a control message up to 1 MiB is
fragmented and reassembled; other bits must be zero. A pane frame stands
alone: at most 16 382 bytes, "more" on a pane stream is a protocol
error, and a pane frame may come between two fragments of a control
message. A pane frame for a stream that isn't open (input still in
flight when the pane ended, output after the device closed it) is
decrypted and discarded, never an error.

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

Only `overlord.new_session` travels by the mailbox (`overlord.ask` left
the remote API at `api` 2), one frame each, when the device finds its host offline or has no live
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

### Panes

A pane is a session's (or the overlord's) tmux pane, live, as a stream
of the device's session (slice 5; the plan is in
`docs/working/web-ui-remote.md`, "Slice 5 plan" and "Slice 5 build").

- **Open**: the op `{"id","op":"pane.open","args":{"stream","target",
  "cols","rows"}}` (stream 1 to 250 and not open on this session; sizes
  2 to 1000). The host refuses a bad one (`bad_request`) and one past its
  limits (2 per device, 8 remote panes in all: `too_many`), asks the
  daemon (`web.call`, which resolves the target, refuses `not_found`, says
  `running` and applies control, below), attaches a view client and
  answers `{"stream","id","name","overlord","input","control","cols",
  "rows","window":{"cols","rows"}}` (`not_running` when it isn't).
  Opening never starts anything. It counts as an op against the rate
  limit; it is never kept in the request store or the outbox.
- **Order**: the answer goes before the stream's first byte and
  `{"event":"pane","stream","type":"closed","reason","message"}` after
  its last, in the session's one sequence. Reasons: `ended` (the client
  exited: the session stopped, the overlord restarted), `closed` (the
  device asked, or its typing was switched), `overflow` (more than 64
  input frames waiting for the PTY), `failed` (an `ack` past what was
  sent). A device reuses a stream id only after its `closed`. A
  revoked device's link is dropped, and its panes with it.
- **Data**: pane frames, host to device the view client's output, device
  to host input (written to the PTY only when the device holds the pane
  and may type; otherwise dropped).
- **Events, device to host**: `{"event":"pane","stream","type":"resize",
  "cols","rows"}`, `{…"type":"ack","bytes","frames"}` (cumulative totals
  the page has parsed), `{…"type":"close"}`. Events, not ops: no rate
  limit, no answer.
- **Credit**: the host sends on a stream while the device has under 64 KiB
  and 48 frames unacknowledged (`hello`'s `"panes":{"max":2,"window":
  65536,"frames":48}`), so a stalled phone never nears the relay's
  128-frame cliff because of a pane.
- **Reconnect**: a stream lives in one session. When the link goes the
  host ends that link's view clients; the device opens the pane again on
  its next session and tmux draws the whole screen for the new client
  first. Nothing is resumed.
- **Control**: each pane has at most one holder (`terminal`, `page:<id>`
  or `device:<id>`), kept by the daemon. A free pane goes to the surface
  that opens it (not to a view-only device); a held one opens as a
  viewer, at the window's size. `pane.take {target, cols, rows}` takes
  it (refused `not_allowed` for a view-only device). Every session in
  the snapshot carries `control` (`null` or `{"holder","by","name",
  "away","cols","rows"}`); a change moves `rev`.
- **View only**: a device the user set to view only (`rlord relay input
  <device> off`) gets `"input": false` in its `hello` and `pane.open`
  answers; its view clients are read-only.

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
- **Host, panes** (`crates/web/src/remote/panes.rs`). A pane frame is
  encrypted only while the stream has credit (above), its link's queue
  holds under 32 KiB (`outq::LOW_MARK`; the writer wakes the actor when
  a watched link's queue falls below it), and the pane's own rate allows
  (64 KiB/s, 256 KiB burst). Frames are encrypted when queued, so that
  mark is what bounds how long an answer waits behind a busy pane (32
  KiB); between links the writer's turn-taking and pacing are unchanged.
  The pane's task reads its view client always (an unread tmux client
  makes the tmux server buffer a flood whole): what it may not send yet
  waits in a hold of at most 256 KiB; past that the hold is dropped and
  the client ended, and when the stream may send again a fresh client is
  attached at the same size, its first frame starting with `CAN ESC c`
  ahead of tmux's full redraw. Only bytes never sent are dropped. Reads
  after 15 ms of quiet go at once; otherwise they gather for up to 15 ms
  or a full frame. Panes end with their link (dropped, unlinked, the
  connection lost, the device revoked).
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
- **Device** (the device engine, `crates/e2e-client`: `machine` holds
  every rule below, each with a unit test on a scripted link and a fake
  clock; the phone app and `rlord-device` both run it).
  - *Dialling.* One URL, built from the pin; no other address is ever
    tried. Backoff 250 ms doubling to 30 s with up to 20% jitter, reset by
    a link `ready` for 10 s. The first 4205 redials at once, another
    within 60 s takes the backoff. A network change (`network(up)` or a
    changed path) redials at once; `network(down)` stops dialling. Coming
    to the foreground dials at once; going to the background sends what
    is queued and closes (1000). The final codes (4401, 4402, 4403, 4426)
    and a wrong relay stop the loop until the user acts. `bye
    {retry_after}` is honoured with jitter; 4429 and 4503 take the
    backoff; 4409 (replaced by a newer connection of this identity)
    waits and doesn't fight; `enroll` is shown and waited on.
  - *Silence.* Pings every 15 s; a link silent 45 s is gone; a 10 s jump
    between the wall and monotonic clocks redials at once (as the host).
    *Input stall*: when the page sent pane input and the link has heard
    nothing since for 5 s, the device pings at once; nothing heard within
    5 s more and the link is given up and dialled again at once (keys
    must not go nowhere for 45 s). Anything heard (a pong, the echo)
    clears it.
  - *Sessions.* `KKpsk2` each time the host comes online, never reusing
    a session. No message 2 within 10 s, or `undeliverable`, retries the
    handshake on the same link (1 s doubling to 30 s). Any decrypt
    failure ends the session and the link; an unknown text message is
    skipped, a binary frame it can't parse closes the link.
  - *State.* The last `rev` is kept; `hello.rev` and `changed` go to the
    page, whose store asks `snapshot {rev}`.
  - *The outbox.* Each request gets 16 random bytes as its id, saved
    before it is sent. Only `overlord.new_session` may wait without a
    live session (it goes by mailbox); any other op needs one. After any
    reconnect every unanswered entry goes again, same id, oldest first,
    once per link; an op that changes something is resent for 2
    minutes, then ends `interrupted`; a new session waits up to 24
    hours. At most 16 entries (`outbox_full`). `in_progress` waits;
    `interrupted` ends the entry. Look-ups (the daemon's idempotent ops,
    `page.report`, `pane.open` and `pane.take` among them) are never
    kept: refused `disconnected` or `host_offline` without a live
    session, and one in flight when the link dies fails `disconnected`.
  - *Panes.* The engine passes them through for the page: the page's
    `{"pane":"input"|"resize"|"ack"|"close","stream",…}` become data
    frames (input split into frames of at most 16 382 bytes) and `pane`
    events; each host frame on a pane stream goes to the page as one
    `{"event":"pane","stream","type":"data","b"}` (standard base64 with
    padding), so the page counts frames by messages; the host's `closed`
    passes on. The engine knows a stream as open from `pane.open`'s
    answer; a frame for a stream that isn't open is discarded, and so is
    the page's input for one. **Pane streams end with their session**:
    when it ends (the link drops, the host goes away, a decrypt fails,
    the app goes to the background), the page hears `closed` with
    `reason: "link"` for every stream still open. **Reopen on
    reconnect**: after the next `hello` the page opens what it shows
    again, with `pane.open` (a new view client, whose first bytes are a
    full redraw); nothing is resumed. A page that reloads (`ready`)
    loses its streams: the engine closes them at the host and drops what
    they still send; the new page's `pane.open` of one of those stream
    ids waits for the host's `closed` of the old one.
  - *The mailbox.* Keyed on the first live session and whenever there is
    none, and again when the `hello`'s `mailbox` isn't the one it holds
    (§3, step 4). One frame out at a time, sealed and saved before it
    leaves; the next after `queued`; `refused full` is sent again first
    on the next link and every 30 s; `no_queue` fails the request
    `host_offline`; `unknown` or `bad` discards the mailbox. When the
    relay's `boot` changes, every mailbox request without an answer goes
    again (the queue is gone). Frames are dropped by `(boot, seq)` and by
    mailbox id, answers by request id it already has.
  - *In the background* (`background_check`, the OS's refresh): one
    dial, the cheap check (`ready`'s `signal`, `boot`); only if something
    waits a session, a snapshot and a count of sessions newly waiting.
  - Owed: no watchdog for an answer on a live session (a request whose
    frame the host never got waits until the next reconnect). The input
    stall rule covers the case that matters most (typing into a pane).

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
  the calling device. `plugin::WEB_PROTOCOL` is 5. `web::Op.idempotent`
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
- **Panes and control** (slice 5). `pane.open` from a device reaches the
  daemon as `web.call` and is answered with `tmux_session` too (the
  plugin keeps it); a local page's pane socket uses `web.pane_open
  {target, surface, cols, rows}` (same answer). `web.pane_size {target,
  surface, cols, rows}` (the surface's terminal changed size: pinned
  again if it holds the pane) and `web.pane_closed {target, surface}`
  (it stopped showing it). The daemon pushes `{"notify":"control",
  "sessions":{"<session id>":{"holder","by","name","away","cols","rows",
  "tmux_session"}}}` (every held pane, on every change): the plugin
  passes input on only for the holder and draws viewers at the holder's
  size. `web.remote_state`'s devices carry `input` (view only when
  false); `{cmd:"input", device, input}` sets it, as `revoke` does.
- **Local page ops** (`remote: false`): `device.list`, `device.pair`,
  `device.pair_cancel`, `device.confirm`, `device.reject`,
  `device.revoke`, answered by the same path.
