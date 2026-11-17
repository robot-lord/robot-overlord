**Status:** in progress (design proposed 2026-11-17; nothing built; waiting on the user's answers and the spike)

# End-to-end encryption for the relay: crypto core and the frontend path

Companion to `web-ui-remote.md` (the spec, the user's decisions and the
slice 1 plan). This doc settles how the encryption is built, with the
weight on the client side: one implementation that is fast enough for live
pane streams, safe in a browser, and loose enough to carry the user's
scheme (per-device keys, Noise handshake, QR pairing, a separate relay
identity, mailbox first, live panes later).

Planned by a Fable planning agent. Versions and browser facts were read on
2026-11-17 from crates.io, the npm registry and the linked pages, not from
memory. Where a fact could not be confirmed from a primary source it says
so.

## Network invariant: everyone dials out to the relay

This is the premise of the remote design, not an option:

- **The rlord host never accepts an incoming connection from the network.**
  Assume it sits behind NAT, a corporate firewall, or a laptop on hotel
  wifi. Its only listener is `rlord-web` on `127.0.0.1` for the local
  browser. Every remote path starts with the host opening an outbound TCP
  (TLS) connection to the relay and keeping it open.
- **The relay is the only publicly reachable piece.** A server with a
  domain and a TLS certificate: either the hosted one (run by the project
  author) or one a user or company runs themselves. It is a separate
  binary (`rlord-relay`), never part of `rlord`.
- **Devices also dial out to the relay.** The phone never connects to the
  host directly. No port forwarding, no UPnP, no direct peer-to-peer in v1
  (a hole-punched direct path is a possible later optimisation, never a
  requirement).
- **The relay splices outbound connections.** Host and device each hold a
  connection to the relay; the relay matches them by their relay identity
  and forwards opaque Noise frames between them. "Anything for me?"
  signals, mailbox delivery and live pane streams all ride on connections
  the host and device opened themselves.
- The relay sees who is connected and traffic metadata, never content
  (see `e2e-crypto.md`).

## Decisions in one table

| Question | Decision | Main reason |
| --- | --- | --- |
| Where the crypto lives | One sans-IO Rust crate, `crates/e2e` (`rlord_e2e`), used by every party | One state machine, one set of test vectors, one place to fix a bug |
| Browser | The same crate compiled to WASM with `wasm-bindgen`, running in a Web Worker | WebCrypto cannot express the protocol across browsers and is async per call |
| WebCrypto's role | Key-at-rest only: a non-extractable AES-GCM wrapping key in IndexedDB, optionally bound to a passkey (WebAuthn PRF) | The one thing WebCrypto does that WASM cannot |
| Phone app | The same crate through UniFFI (Swift, Kotlin) | Same reason as the browser; UniFFI is what Matrix and Wire use |
| Protocol | Noise, via `snow`. Pairing: `IKpsk2`. Reconnect: `KK`. Cipher suite `25519_ChaChaPoly_SHA256` | Two-party sessions only; fits the QR flow exactly; `snow` is the audited Rust option |
| MLS, Double Ratchet, HPKE | None of them in the first version | Sessions are always host to one device; the host is the source of truth, so nothing has to sit at the relay for long |
| Mailbox | The same Noise session, persisted, with a one-way rekey after every message; the relay queue is ordered and short-lived | Forward secrecy for queued messages with no second protocol |
| Post-quantum | Hybrid forward secrecy (ephemeral ML-KEM-768 mixed in through Noise's PSK slot) from the first protocol version, if the spike confirms it; static keys stay X25519 | The relay is the ideal place to record traffic for later; retrofitting means re-pairing every device |
| Relay identity | A separate P-256 ECDSA key per party; signed challenge over TLS by default, the same key as a self-signed client certificate for mTLS | P-256 is the one curve every hardware keystore and every TLS stack takes |
| Remote browser client | Not in the first remote slice. The page is served by the local `rlord-web` only; remote clients are the native app | A page served by the relay breaks E2E, and nothing shipped in browsers fixes that yet |
| WASM build | `wasm-bindgen` run from `cargo xtask` (its CLI library pinned in `Cargo.lock`), `--target web`, loaded by URL; no `wasm-pack`, no Vite plugin | Pinned by the lockfile, no tool download at build time, works under the strict CSP |

Each is argued below.

## Who talks to whom

Three parties, and only two of them hold content keys:

- **Host**: `rlord-web` on the user's machine (the plugin; the main
  `rlord` binary holds no keys and no crypto, see "Workspace").
- **Device**: the phone app first; a remote browser later.
- **Relay**: routes opaque frames. Holds relay-identity public keys and
  queues. Never a content key.

Every encrypted session is host to one device. Devices never talk to each
other, and there is no group. That fact drives most of the protocol
choices.

## One Rust core or WebCrypto

### What WebCrypto can do in late 2026

| Primitive | Chrome | Firefox | Safari | Usable for us |
| --- | --- | --- | --- | --- |
| AES-GCM, HKDF, SHA-2, ECDH/ECDSA P-256 | yes | yes | yes | yes |
| X25519 | 133 (desktop first) | 130 | enabled in WebKit 2024-12, shipped early 2025 | yes |
| Ed25519 | 137 | 130 | yes | yes |
| ChaCha20-Poly1305, ML-KEM, ML-DSA, X-Wing | 154 beta (2026-09); release notes file it under 155 | tests synced only; not confirmed shipped | nothing found | no: not interoperable |
| BLAKE2 | no | no | no | no |
| Non-extractable `CryptoKey` stored in IndexedDB | yes | yes | yes | yes |

Sources: Igalia's secure-curves posts, the Chrome X25519 and Ed25519
intents, the Chrome 154 beta post, the "Algorithm Updates in WebCrypto"
intent (which says outright that the new algorithms will not work across
browsers until others implement them), Mozilla bug 1999945.

So a pure WebCrypto client is limited to `Noise_*_25519_AESGCM_SHA256`
written by hand in TypeScript, with no post-quantum option outside Chrome.

### The three options

| | Shared Rust core (WASM) | WebCrypto only (TypeScript) | Hybrid |
| --- | --- | --- | --- |
| Implementations of the protocol | one | two or three (Rust, TS, maybe Swift/Kotlin) | one |
| Per-frame cost | a synchronous call | a promise and a thread hop per frame; ordering must be kept by hand | a synchronous call |
| ChaCha20-Poly1305, ML-KEM | yes | Chrome only | yes |
| Private keys can be non-extractable | no (bytes live in WASM memory) | yes | the wrapping key is; the Noise key is not, while a session is open |
| Extra download | a WASM file (size to be measured) | none | a WASM file |
| Test vectors | shared, byte-identical | must be reproduced per language | shared |

**Chosen: hybrid, with the protocol entirely in the Rust core.** WASM runs
the handshake, the transport and the framing. WebCrypto holds one
non-extractable AES-GCM key that wraps the device's keys at rest.

Why not put the device's static key in WebCrypto as a non-extractable
X25519 key (the strongest storage)? Every DH with it would be an async
`deriveBits`, and `snow`'s `Dh` trait is synchronous. Bridging needs
either WebAssembly JSPI (Chrome 137; Firefox planned for 153; Safari
unconfirmed) or a handshake written by us with the DH injected. Neither is
worth it for the first version. It stays a later hardening step, and the
core keeps DH behind a trait so it can be added.

### What comparable projects do

| Project | Shape | What it tells us |
| --- | --- | --- |
| Matrix (`vodozemac` 0.11.2, `matrix-sdk-crypto-wasm` 19.0.1) | Rust core; `wasm-bindgen` for the web; UniFFI for mobile | The shape proposed here, in production. `libolm` (C) was deprecated for it in 2024. The February 2026 dispute over missing checks (an all-zero DH output accepted) shows the value of one core: one fix, every platform. |
| Wire (`core-crypto`, `@wireapp/core-crypto` 11.0.1) | Rust core over OpenMLS; `wasm-bindgen` for JS, UniFFI for Swift and Kotlin; IndexedDB keystore encrypted with AES-256-GCM on the web | Same shape again. Their own docs say the web keystore's encryption at rest is "not validated nor audited": the browser store is the soft part for everyone. |
| Signal (`libsignal`, `@signalapp/libsignal-client` 0.108.0) | Rust core; hand-written JNI, Swift and Node bridges; no browser target | A Rust core with thin bridges scales. Signal ships no web client at all, because of code delivery. Also AGPL-3.0, so its code is not usable in an MIT/Apache project. |
| iroh 1.0 | Rust; compiles to WASM for browsers; browser traffic always goes through a relay that cannot decrypt it | A Rust networking and crypto stack in browser WASM, relayed and still E2E, is a shipped pattern. |
| age (`typage`) | A separate TypeScript implementation on WebCrypto and noble | Works because the format is small and frozen and has a shared vector suite. Our protocol is stateful and will change; a second implementation is a cost we don't need. |
| MLS (`openmls` 0.8.3) | Group key agreement | For groups with members joining and leaving. We have pairs. |

## Protocol

### Why Noise and not MLS, a Double Ratchet or HPKE

- **MLS** solves group membership. Every session here has two members. It
  also needs an ordered delivery service and brings a large dependency.
  Per-device pairwise sessions already give "revoke one phone without
  re-keying the rest".
- **Double Ratchet** (vodozemac's Olm) is for two parties who are rarely
  online together and exchange messages for years. Here the host is the
  thing being controlled: when it is offline there is nothing to say, and
  when it is online a fresh handshake is two small messages away.
- **HPKE** seals a message to a public key with no session. Nothing in the
  first version needs that. The one future use is a notification preview
  the phone can decrypt without opening a session (see "Later").

### The patterns

Cipher suite for all of them: `25519_ChaChaPoly_SHA256`. ChaCha20-Poly1305
is constant-time in software and identical on every target; pure-Rust
AES-GCM in WASM has no AES instructions to use. SHA-256 over BLAKE2s
because every platform has it, should anyone ever need a second
implementation; the hash only runs during handshakes.

| Use | Pattern | Messages | Why |
| --- | --- | --- | --- |
| Pairing | `Noise_IKpsk2` | device → host, host → device | The device knows the host's static key from the QR (that is `IK`). The host has never seen the device; the one-time secret from the QR, as the PSK, is what proves the device saw the QR. `IKpsk2` is WireGuard's handshake, the most analysed Noise pattern there is. The device's static key crosses the relay encrypted. |
| Reconnect (live or mailbox) | `Noise_KK` | device → host, host → device | Both static keys are already known, so neither is sent, encrypted or not. Mutual authentication in one round trip. A revoked device simply fails: the host no longer has its key. |

Rules that go with them:

- **The device always initiates.** Both sides only dial out to the relay;
  this fixes the roles.
- **No application data in handshake messages.** First-message payloads
  are replayable and have weaker forward secrecy. The handshake payloads
  carry only the ML-KEM material below.
- **The host registers a device only after the first transport message
  decrypts.** With `psk2` that is the first proof the device holds the
  pairing secret.
- **The prologue** binds the protocol version, the role and the pairing or
  mailbox id, so a frame replayed into another context fails.

### Rust crates

| Crate | Version | Status | Verdict |
| --- | --- | --- | --- |
| `snow` | 0.10.0 (2025-07) | Tracks Noise rev 34. Reviewed by Trail of Bits, paid for by 1Password: 10 findings (1 medium, 1 low, 8 informational), 8 fixed, including the nonce bug behind RUSTSEC-2024-0011 (fixed in 0.9.5). The README still says "no formal audit", and the 1Password post does not name the commit reviewed. 31 M downloads; used by libp2p. | **Use.** |
| `clatter` | 2.3.0 (2026-08) | `no_std`, pure Rust, native hybrid handshakes with ML-KEM. States plainly that it has had no audit. | Fallback for the hybrid handshake only. |
| `noise-protocol` | 0.2.1 | Small, little used. | No. |
| `reishi-handshake` | 0.4.0 | Hybrid `IK` only, a few thousand downloads. | No. |

Notes on `snow` 0.10.0 that matter here:

- Its default resolver still depends on the previous RustCrypto generation
  (`chacha20poly1305` 0.10, `curve25519-dalek` 4, `getrandom` 0.3), while
  current releases are `chacha20poly1305` 0.11.0 and `curve25519-dalek`
  5.0.0. Fine on its own; it means duplicate crates if anything else in
  the plugin uses the new generation. `Builder::with_resolver` lets us
  supply primitives ourselves if the spike shows a reason to (SIMD, size).
- On `wasm32-unknown-unknown`, `getrandom` 0.3 needs its `wasm_js` backend
  switched on by a cfg flag. A known step, done in the xtask build.
- Never the `hfs` feature: it is Kyber1024 through a C library, not
  ML-KEM, and would not build cleanly to WASM.

### Mailbox and live channel

| Property | Live channel (pane bytes, control while connected) | Mailbox (requests, replies, `changed`, while one side is polling) |
| --- | --- | --- |
| Carrier | one relayed websocket, frames forwarded at once | a queue per recipient at the relay; ordered; acknowledged to delete |
| Session | fresh `KK` per connection; keys in memory only | `KK` whose messages travel through the queue; state persisted on both sides |
| Forward secrecy | per connection, plus a rekey every 2^20 frames | rekey after every message (Noise's one-way `REKEY`): a key is deleted as soon as its message is sent or read |
| Recovery after a compromise | next connection | next handshake; the device starts one each time it comes to the foreground |
| Ordering and replay | implicit counter; the websocket is ordered; anything replayed, dropped or reordered fails to decrypt | the same, because the queue is ordered. A gap breaks the session; the device re-handshakes and unanswered requests are resent by id |
| Storage at the relay | none | minutes, bounded count and bytes |

**The mailbox is a slow, ordered pipe, not an archive.** The host already
holds everything (the daemon's state, the overlord's history). A phone
that was offline for a day gets a wake-up (no content), connects, runs
`KK` and asks for a `snapshot`. So the relay never needs to hold a backlog
of content, and "forward secrecy for stored messages" reduces to
per-message rekeying of a short queue. This is the assumption that lets
one mechanism cover both shapes; it is open question 2.

Mapping to `web-ui-remote.md`: the "anything for me?" signal is a
contentless poll or push answered with a queue length; the heavier
transfer is draining the queue or opening the live channel.

Inside a live session several streams share one Noise session: the first
plaintext byte of each frame is a stream id (control, pane 1, pane 2…), so
the relay cannot tell panes apart.

### Frames and padding

- Wire frame: 2-byte length, then the Noise ciphertext (16-byte tag). The
  Noise limit is 65535 bytes per message; plaintext is capped at 16 KiB so
  buffers are fixed and a big burst never delays a keystroke for long.
- The host coalesces PTY output before encrypting: up to 16 KiB or a few
  milliseconds, whichever comes first. Fewer, larger frames is the single
  biggest performance lever, ahead of anything in the cipher.
- Padding (length inside the ciphertext, zero fill):
  - Live frames: round up to a multiple of 32 bytes. Hides whether a key
    was one byte or an escape sequence; costs almost nothing.
  - Mailbox frames: Padmé buckets (at most about 12% overhead), since a
    message's size otherwise names its type.
  - Timing is not hidden. Keystroke timing is visible to the relay as it
    is to any SSH observer. Stated in the docs, not fixed.

### Post-quantum

The threat is recording now and decrypting later. The relay sees every
ciphertext and is untrusted by design, so it is exactly where that would
happen, and the content is source code and terminal sessions. Browsers'
TLS has been hybrid by default since Chrome 131; Apple's CryptoKit has
X-Wing HPKE from iOS 26.

**Proposed: hybrid forward secrecy in protocol version 1; static keys stay
X25519.** An active attacker with a quantum computer at handshake time is
out of scope (as it is for TLS today), so the QR stays small.

How, on top of `snow`, using the mechanism WireGuard reserved its PSK slot
for:

1. The device puts a fresh ML-KEM-768 encapsulation key (1184 bytes) in
   the payload of message 1, where the classical keys encrypt and
   authenticate it.
2. The host encapsulates, gets a shared secret and a ciphertext (1088
   bytes), and sends the ciphertext in clear in front of message 2.
3. Both set the PSK for message 2 to the shared secret (`set_psk`). For
   pairing the PSK is a hash of the pairing secret and the shared secret.
   The patterns become `IKpsk2` and `KKpsk2`.

Someone who later breaks X25519 can read the encapsulation key and the
ciphertext but not the shared secret, so not the session.

Caveats, all for the spike or a reviewer:

- This composition is ours. It is a known pattern, but it has not been
  reviewed in this exact form (the KEM ciphertext is bound through the
  shared secret, not hashed into the transcript). It needs a
  cryptographer's read before it carries real traffic.
- `set_psk` after message 1 has been processed must be confirmed against
  `snow` 0.10.0's behaviour; the docs don't state a timing rule.
- KEM crate: `libcrux-ml-kem` 0.0.11 (formally verified portable and AVX2
  code; in use at Signal and OpenSSH according to its vendor; WASM support
  not confirmed) or RustCrypto's `ml-kem` 0.3.4 (pure Rust, no audit
  found). The spike builds both to WASM and picks.
- If any of that fails: `clatter`'s native hybrid `KK`/`IK`, or ship
  version 1 classical. Either way the first byte of every handshake is a
  protocol version, so a later version can be added without a flag day.

## Performance

What is known before measuring:

- **Per-frame work is dominated by fixed costs, not the cipher.** A pane
  stream is kilobytes to a few megabytes per second. The costs that matter
  are the JS to WASM call, copies, allocation, and `postMessage`.
- **`wasm-bindgen`'s default byte-slice arguments copy and allocate on
  every call** (allocate in WASM memory, copy in, call, copy out, free).
  Avoided by design, below.
- **WebCrypto is asynchronous**: every frame is a promise resolved on
  another thread, and order has to be kept by chaining. Good for large
  blobs, poor for thousands of small frames. No published benchmark was
  found comparing it with WASM ChaCha20-Poly1305 on small frames; the
  spike measures it.
- **SIMD**: fixed-width WASM SIMD is in every current engine. The
  `chacha20` crate has a `simd128` backend, selected at compile time with
  `-C target-feature=+simd128`. Whether the version `snow` resolves has
  it, and how much it gains, is a spike item. Poly1305 has no WASM SIMD
  backend. Relaxed SIMD is not needed.
- **WASM on a phone** matters only if the app uses a WebView (open
  question 5).

### The WASM boundary

- One exported `Session` object owns a fixed buffer in WASM memory (one
  max frame plus headroom). JS holds a `Uint8Array` view of it and
  re-creates the view if memory grew.
- Decrypt: copy the websocket's bytes into the buffer, call
  `decrypt(len) -> plaintext_len` (in place), copy the plaintext out
  (xterm.js queues writes, so it must own its bytes). Two copies, no
  allocation in WASM, one call.
- Batch calls: `decrypt_batch` and `encrypt_batch` take several
  length-prefixed frames in the buffer in one call, for bursts.
- No `SharedArrayBuffer`: it needs cross-origin isolation headers and adds
  nothing here.

### Worker

The relay websocket, the WASM module and the keys live in one dedicated
Web Worker. The page thread gets plaintext as transferred `ArrayBuffer`s
(no copy), batched when frames arrive faster than it draws.

- Keeps decryption off the thread that renders the terminal.
- Keeps key bytes out of the page's JS realm. That is a real but partial
  barrier: injected script on the page can still ask the worker to act.
- Costs one `postMessage` hop per keystroke and per batch. The benchmark
  checks that against running everything on the page thread.

### Micro-benchmark to run

A static page (and the same cases under Node for CI), plus `criterion` on
the native side.

| Axis | Values |
| --- | --- |
| Plaintext size | 1, 16, 64, 256, 1024, 4096, 16384 bytes |
| Implementation | WASM ChaCha20-Poly1305 scalar; WASM with `+simd128`; WebCrypto AES-GCM awaited one by one; WebCrypto AES-GCM pipelined; `@noble/ciphers` 2.4.0 (pure JS baseline) |
| WASM calling style | copy-per-call (`&[u8]` in, `Vec<u8>` out); shared buffer; batched |
| Thread | page thread; worker with a message per frame; worker with batching and transfer |
| Browser | Chrome, Firefox, Safari on the Mac; Safari on a real iPhone; Chrome on a real Android phone |
| Workload | synthetic sizes above, and a recorded pane stream replayed as the bridge would chunk it (a large `cat`, scrolling in an editor, typing) |

Measured: frames per second, bytes per second, p50 and p99 time per
frame, time the page thread is blocked, WASM file size (raw and
compressed), time to instantiate.

It passes if, on the slowest phone tested: crypto takes under a tenth of
the time xterm.js spends on the same bytes during a burst; a keystroke's
added delay is under a millisecond at p99; and instantiating the module
does not visibly delay first paint.

## Browser threat model

### Code delivery

Whoever serves the JavaScript can read the keys. E2E in a browser is only
as strong as the origin of the page.

| Option | Holds against a hostile relay? | State in late 2026 |
| --- | --- | --- |
| Page served by the local `rlord-web` on `127.0.0.1` | yes, but it is local: no relay involved | slice 1 |
| Page served by the relay | no | never |
| Subresource Integrity | no: the HTML that carries the hashes comes from the same server | n/a |
| Installed PWA with a service worker caching pinned assets | no: the server can ship a new service worker whenever it likes | n/a |
| Signed bundles / Isolated Web Apps | yes | Chrome only, ChromeOS and managed devices |
| WAICT (integrity manifest plus transparency log) | yes, when it exists | prototype behind a pref in Firefox Nightly (2026-05); no Chrome or Safari commitment found |
| Browser extension holding the client | yes: store-signed, versioned | works today; one more thing to publish per browser |
| The page inside an installed native app (phone WebView, desktop shell) | yes: assets ship in the signed app | works today |

**Decision:** the first remote slice has no remote browser client. Remote
means the native phone app. The browser UI stays what slice 1 builds: a
page from the local plugin over plain localhost websockets, no E2E needed.

For a remote browser later, in order of preference: the page bundled in an
installed app or extension; then a static origin separate from the relay
once WAICT ships. Never the relay. The WASM path below is what makes any
of these cheap, and it is also what a phone WebView would run.

### Keeping the page ready for it

The page code talks to a `Transport` interface: `LocalTransport` (slice 1:
token, plain websockets) and later `E2eTransport` (worker, WASM, relay).
Messages above that line are the same (`web-ui-remote.md`, "Layer" table).

### Key storage in a browser device

- Device keys are generated in WASM, then encrypted with a non-extractable
  AES-GCM `CryptoKey` and stored, with that key, in IndexedDB.
- That protects against copying the profile's database files. It does not
  protect against script running in the page while it is open, which can
  ask for the unwrap. Nothing in a browser does, short of the
  non-extractable X25519 route above.
- Optional: derive the wrapping key from a passkey (WebAuthn PRF), so
  unlocking needs a user gesture. PRF is in WebAuthn Level 3 (a
  Recommendation since 2026-08) but support is uneven: no Firefox on
  Android, Windows Hello only after a February 2026 update, open WebKit
  bugs with security keys, password managers from perfect to broken. Opt
  in, never required.
- A browser device is its own revocable device. Give it an expiry when
  unused; revoking is the real remedy for a stolen profile.

### XSS, CSP, Trusted Types

- The slice 1 CSP stands. WASM needs exactly one addition:
  `script-src 'self' 'wasm-unsafe-eval'` (Chrome 97, Firefox 102; Safari
  16 reported by a secondary source only, so it is checked in the spike),
  plus `worker-src 'self'`.
- The `.wasm` file must be a hashed static asset served as
  `application/wasm`, never inlined as a `data:` URL (Vite inlines small
  assets by default; the build turns that off for `.wasm`).
- Add `require-trusted-types-for 'script'`. Trusted Types reached every
  engine in February 2026 (Chrome 83, Safari 26, Firefox 148). Preact
  writes no HTML strings; xterm.js has to be checked.
- No third-party script, no CDN, no analytics, as slice 1 already rules.

## Key model

| Key | Type | Held by | Stored | Seen by the relay |
| --- | --- | --- | --- | --- |
| Host static (content identity) | X25519 | host | file in the data dir, 0600 | never |
| Device static (content identity) | X25519 | device | wrapped by the platform keystore (below) | never (encrypted in pairing, absent afterwards) |
| Pairing secret | 32 random bytes, one use, short life | host; device via QR | host memory only | never |
| Session keys | from Noise | both ends | live: memory; mailbox: persisted, rekeyed per message | never |
| Host relay identity | P-256 ECDSA | host | file, 0600 | public key |
| Device relay identity | P-256 ECDSA | device | hardware keystore where there is one | public key |
| Relay TLS key | the relay's certificate | relay | relay | its own |

Platform storage for the device static key:

- **iOS:** the Secure Enclave holds only P-256 among classical curves, so
  the X25519 key is stored wrapped by an enclave key, in the Keychain,
  this device only.
- **Android:** Keystore's TEE supports Curve25519 but StrongBox was found
  not to; wrap with a Keystore AES key.
- **Browser:** as above.
- **Rejected for now:** P-256 as the Noise curve so the static key itself
  could be hardware-bound everywhere. It leaves the standard, audited
  `25519` path for a non-spec one. The version byte leaves room.

### Relay identity and "TLS or mTLS"

One key per party, two ways to present it:

- **Default: TLS plus a signed challenge.** The relay has an ordinary
  certificate (ACME). On connect it sends a nonce; the client signs the
  nonce with a fixed label and the relay's host name. Works in browsers
  (which cannot offer a JS-held client certificate), in WebViews, and
  behind any proxy or CDN a self-hoster puts in front.
- **Optional: mTLS.** The same P-256 key in a self-signed client
  certificate; the relay checks the key's fingerprint against its list,
  not a CA (`rustls` 0.23 with a custom client verifier; `rcgen` 0.14 to
  make the certificate). For native clients and relays that terminate TLS
  themselves. On iOS the key can stay in the Secure Enclave.

The relay stores fingerprints, queue ids and an allow-list per host (which
device identities may reach it). No accounts, no names.

The two identities are tied together only inside the encrypted session:
at pairing the device tells the host its relay public key, and the host
registers it with the relay. The relay cannot link a relay identity to a
content key, because no static key ever crosses it in clear.

### Pairing

1. The user runs the pair command. The plugin makes a pairing secret and a
   one-time rendezvous id and registers the id with the relay.
2. rlord shows a QR: version, relay URL, rendezvous id, host static public
   key, pairing secret, optionally a pin for the relay's TLS key. About
   150 bytes, a small code. The main binary only draws a string the plugin
   gave it.
3. The phone scans, makes its static key and relay identity, connects to
   the relay and sends `IKpsk2` message 1 to the rendezvous id.
4. The host answers. The phone's first transport message carries its name
   and relay public key.
5. The host stores the device, registers its relay identity with the
   relay, replies with the mailbox ids, and destroys the pairing secret
   and rendezvous id.
6. Both show a short code derived from the handshake hash. Whether the
   host must confirm it is open question 4.

A photographed QR is the weak point: whoever uses it first is paired, and
the real phone then fails. The device list and the confirmation step are
the answer.

### Adding and revoking

- **Add:** pairing again. Each device has its own keys and sessions.
- **Revoke:** the host deletes the device record, closes its sessions and
  tells the relay to drop its identity and queue. `KK` with a deleted key
  cannot complete. No other device is touched.
- **Lost host key:** every device pairs again.
- **Pairing and revoking are user-only.** Like the other `web.*` lifecycle
  methods they are never on the web op allow-list and never reachable from
  MCP: an agent must not be able to add a device. If the device list later
  becomes visible to the overlord, `overlord.md` changes in that PR.

A paired device can do what the web op allow-list permits, which includes
typing into agents. Pairing a phone is granting it a shell.

### What the relay sees

IP addresses; relay identities and queue ids; which host each device
reaches; when, how often, how many bytes (to the padding bucket); whether
each side is online; push tokens; how many devices a host has. Not names,
not content, not which pane, not content keys.

## Fitting the plan

### Workspace

| Crate | What | Built for |
| --- | --- | --- |
| `crates/e2e` (`robot-overlord-e2e`, lib `rlord_e2e`) | Sans-IO core: QR payload, pairing and reconnect handshakes, sessions (in-place encrypt and decrypt, rekey, padding), frame codec, key types that zeroize. No tokio, no sockets, no file access, `forbid(unsafe_code)`. Relay-identity signing behind a feature. | native, `wasm32-unknown-unknown`, iOS and Android targets |
| `crates/e2e-wasm` | `cdylib`; the `wasm-bindgen` surface (the buffer-owning `Session`, pairing, key export for wrapping) | `wasm32-unknown-unknown` only |
| `crates/e2e-ffi` | UniFFI 0.32.2 surface for Swift and Kotlin | when the app starts; not before |
| `crates/web` | depends on `rlord_e2e`; owns the keys, the relay client, device records | native |
| relay | its own binary and crate; does **not** depend on `rlord_e2e` | native |

- All `publish = false`, in `members`, not in `default-members`.
- **The main binary stays clean.** Nothing under
  `crates/{core,daemon,tui,rlord}` depends on these. `cargo xtask
  check-deps` gains `robot-overlord-e2e`, `snow` and the ML-KEM crate on
  its deny list. The QR reaches the terminal as an opaque string from the
  plugin.
- `rust-toolchain.toml` gains the `wasm32-unknown-unknown` target.

### Building the WASM

- `cargo xtask web wasm`: `cargo build --release --target
  wasm32-unknown-unknown -p robot-overlord-e2e-wasm` (with the `getrandom`
  backend flag and, if the benchmark says so, `+simd128`), then the
  bindgen step **in process**, through `wasm-bindgen-cli-support` pinned
  to the same `=0.2.130` as `wasm-bindgen`. Both are then pinned by
  `Cargo.lock`, and no tool is downloaded during a build.
- Not `wasm-pack` (0.15.1, alive again under new maintainers after the
  rustwasm organisation was archived in 2025): it fetches `wasm-bindgen`
  and `wasm-opt` binaries at build time, which the release build's rules
  don't allow.
- `wasm-opt` is skipped until the spike shows the size it would save; if
  it pays, the `binaryen` npm package, pinned, in the existing Node job.
- Output (`.js`, `.d.ts`, `.wasm`) goes to `crates/web/ui/src/wasm/`,
  generated and not committed. `cargo xtask web build` runs the WASM step
  first.

### Vite

- `--target web` output, loaded explicitly in the worker: the generated
  `init()` is given `new URL('./rlord_e2e_bg.wasm', import.meta.url)`,
  which Vite turns into a hashed asset and the browser compiles while
  streaming.
- No plugin. Vite 8.1 took `vite-plugin-wasm`'s ESM integration into core
  (`import { f } from './x.wasm'`), which would allow `--target bundler`
  instead; nothing found confirms `wasm-bindgen`'s bundler output against
  it, and the explicit URL form has fewer moving parts. `vite-plugin-wasm`
  3.6.0 supports Vite 8 if ever needed.
- The worker is created with `new Worker(new URL('./e2e.worker.ts',
  import.meta.url), { type: 'module' })`, the form Vite detects.
- `build.assetsInlineLimit` excludes `.wasm`; the plugin's asset table
  serves it as `application/wasm`; the asset test gains a check for both.

### Test vectors

- `crates/e2e/vectors/*.json`, committed: QR payload encode and decode;
  full pairing and reconnect handshakes with fixed ephemeral keys and KEM
  randomness; transport frames; padding; a rekey sequence; failures (wrong
  PSK, replayed frame, reordered frame, revoked key).
- Written by `cargo xtask e2e vectors` from the Rust core; a test fails if
  the committed files differ from a fresh run.
- Checked in three places: native `cargo test`; the WASM build under
  Vitest in Node; later Swift and Kotlin unit tests through UniFFI.
- `snow`'s own vector tests cover the base patterns.
- One interop test: Vitest starts a small native peer and runs pairing,
  reconnect and a few thousand frames against the WASM build.

### CI

- `check` job: `cargo test -p robot-overlord-e2e`, `check-deps` with the
  longer deny list.
- `web` job: adds the WASM target and `cargo xtask web wasm` before `web
  check` and `web build`; Vitest runs the vectors and the interop test.
- `cargo deny` (the repo has `deny.toml`) covers the new crates' licences
  and advisories.
- Release: the `web-assets` job builds the WASM once, like the page.

## First spike

Smallest thing that removes the real unknowns. Lives in `spikes/e2e/`
until it has answered them; the crates are created properly afterwards.

Build:

1. The core: `IKpsk2` pairing and `KK` reconnect on `snow`, the ML-KEM
   step, a `Session` with in-place encrypt and decrypt, rekey and padding.
2. The WASM wrapper with the shared-buffer and batch calls, built by an
   xtask-style script through `wasm-bindgen-cli-support`.
3. A native peer and a pass-through "relay" (a loop that forwards opaque
   frames between two sockets on localhost), and a page that pairs from a
   pasted QR string, reconnects, and echoes frames.
4. The benchmark page.
5. Optional and cheap: generate the Swift binding with UniFFI and call the
   QR payload parser from a Swift test on the Mac.

It must prove:

| # | Claim | How |
| --- | --- | --- |
| 1 | `snow` and an ML-KEM crate build for `wasm32-unknown-unknown` and run in Chrome, Firefox, Safari and Node | the page and Vitest |
| 2 | Native and WASM produce byte-identical handshakes and frames | shared vectors, both directions of the interop test |
| 3 | The PSK-slot hybrid works with `snow`'s public API (`set_psk` between messages) | a passing handshake, and failing ones with a wrong KEM ciphertext or wrong pairing secret |
| 4 | The relay needs no crypto code | the forwarder links nothing from the core |
| 5 | Frame throughput and latency are far inside the pane's needs; whether SIMD and the worker pay | the benchmark, including two real phones |
| 6 | WASM size and start-up are acceptable, with and without ML-KEM | measured |
| 7 | Vite 8 bundles it under the slice 1 CSP plus only `'wasm-unsafe-eval'`, in a module worker, including Safari | the built page served with those headers |
| 8 | Nothing reaches the `rlord` binary | `check-deps` |

Not in the spike: the real relay, persistence, device records, key
wrapping, UI, the phone app.

## Later

- A notification preview the phone can decrypt while locked: needs
  encryption to an offline device with no session, so HPKE (X-Wing where
  the platform has it) to a rotating device prekey. Only if wanted.
- The non-extractable X25519 device key in browsers (JSPI or an injected
  DH).
- Hardware-bound static keys on phones.
- An outside review of the protocol document before the relay is offered
  to anyone but the user.

## Risks

- **A protocol of our own.** Standard parts, but the assembly (pairing
  rules, mailbox rekeying, the hybrid step) is ours. Mitigated by keeping
  to `snow`'s public API, vectors, failure tests, a written protocol
  description, and a review before public use.
- **`snow`'s release pace.** 0.10.0 is from July 2025 and sits on the
  older RustCrypto generation. If it stalls, the resolver hook lets us
  supply primitives; `clatter` is the alternative.
- **Unaudited post-quantum code.** Whichever ML-KEM crate is chosen. The
  hybrid means a flaw there cannot make things worse than classical.
- **Mailbox sessions desynchronise** (a crash between advancing a key and
  saving it, a queue that drops a message). Designed to heal by
  re-handshaking; needs tests that kill each side at each step.
- **WASM memory is not a vault.** Keys are plain bytes in a worker.
  Zeroize helps little in a garbage-collected host. Stated, not solved.
- **Safari.** WASM under CSP, module workers and IndexedDB key storage all
  have Safari-specific history. The spike tests it early.
- **Three build targets for one crate** (native, WASM, mobile) make
  dependency features easy to get wrong. Separate `cargo build -p`
  invocations, as the release build already does.
- **Bundle size** with ML-KEM in WASM is unknown until measured.
- **Scope.** A remote browser client is the expensive, risky part. The
  decision above defers it, but the WASM path is still built and tested so
  it doesn't rot.

## Open questions for the user

1. **Post-quantum from the first version?** Proposed: yes, hybrid forward
   secrecy (ML-KEM-768 mixed into the Noise handshake), static keys
   classical, if the spike confirms it. The alternative is classical now
   with a version byte for later, which means recorded traffic stays
   exposed and devices re-handshake under a new version when it lands.
2. **Does anything need to wait at the relay for hours or days?** The
   design assumes no: the host holds the truth, the relay queue is short,
   an offline phone gets a contentless wake-up and pulls on connect. If
   you want, say, messages delivered to a phone while the host is off, the
   mailbox needs a real ratchet (vodozemac's Olm) instead.
3. **Remote browser client: defer?** Proposed: the first remote slice is
   the native app only; the browser UI stays local. If a remote browser
   matters early, the choices are an extension, a desktop shell, or
   accepting a page hosted somewhere you trust.
4. **Pairing confirmation.** Should the host require a confirm (device
   name plus a short code shown on both) before a newly paired device can
   do anything? Proposed: yes; it defeats a photographed QR.
5. **Phone UI.** Fully native screens with a native terminal view, or a
   native shell around the same Preact page in a WebView? It decides
   whether the phone runs the core through UniFFI (native keys, native
   speed) or through WASM, and whether the WASM path is exercised from the
   start.
6. **Relay auth default.** Proposed: TLS plus signed challenge by default,
   mTLS as an option for native clients, P-256 for the relay identity.
   Fine?
7. **Who reviews the protocol?** Before public use, a paid review, an
   informal one from someone you trust, or none for the personal-use
   phase?
8. **Notification previews** (text on the lock screen) or contentless
   wake-ups only? Previews bring HPKE and prekeys.

## Sources

Registries (read 2026-11-17): crates.io for `snow`, `clatter`,
`wasm-bindgen`, `wasm-pack`, `uniffi`, `chacha20poly1305`, `ml-kem`,
`libcrux-ml-kem`, `x25519-dalek`, `curve25519-dalek`, `vodozemac`,
`openmls`, `rustls`, `rcgen`, `noise-protocol`, `reishi-handshake`; npm for
`vite-plugin-wasm`, `@noble/ciphers`, `@matrix-org/matrix-sdk-crypto-wasm`,
`@signalapp/libsignal-client`, `@wireapp/core-crypto`.

- Noise: [specification, rev 34](https://noiseprotocol.org/noise.html);
  [snow](https://github.com/mcginty/snow);
  [snow `HandshakeState`](https://docs.rs/snow/0.10.0/snow/struct.HandshakeState.html);
  [1Password on the snow review](https://1password.com/blog/strengthening-snow-for-the-open-source-community);
  [Trail of Bits report](https://github.com/trailofbits/publications/blob/master/reviews/2024-03-agilebits-snow-securityreview.pdf);
  [RUSTSEC-2024-0011](https://rustsec.org/advisories/RUSTSEC-2024-0011.html);
  [clatter](https://docs.rs/crate/clatter/2.3.0)
- WebCrypto:
  [Igalia, "Can I use Secure Curves in the Web Platform?"](https://blogs.igalia.com/jfernandez/?p=1913);
  [Intent to Ship: X25519](https://groups.google.com/a/chromium.org/g/blink-dev/c/A7lbRONS1lY);
  [Intent to Ship: Ed25519](https://groups.google.com/a/chromium.org/g/blink-dev/c/T2kriFdjXsg);
  [Ed25519 in Chrome (IPFS)](https://blog.ipfs.tech/2025-08-ed25519/);
  [Intent to Experiment: Algorithm Updates in WebCrypto](https://groups.google.com/a/chromium.org/g/blink-dev/c/TF8y175J3YU);
  [Chrome 154 beta](https://developer.chrome.com/blog/chrome-154-beta);
  [Mozilla bug 1999945](https://bugzilla.mozilla.org/show_bug.cgi?id=1999945);
  [MDN `deriveBits`](https://developer.mozilla.org/en-US/docs/Web/API/SubtleCrypto/deriveBits)
- Comparable projects:
  [Wire CoreCrypto architecture](https://wireapp.github.io/core-crypto/ARCHITECTURE.html);
  [libsignal](https://www.github.com/signalapp/libsignal);
  [Matrix, libolm deprecation](https://matrix.org/blog/2024/08/libolm-deprecation/);
  [Matrix, analysis of reported issues in vodozemac](https://matrix.org/blog/2026/02/analysis-of-reported-issues-in-vodozemac/);
  [Least Authority vodozemac audit](https://leastauthority.com/static/publications/LeastAuthority-Matrix_vodozemac_Final_Audit_Report.pdf);
  [iroh in the browser](https://docs.iroh.computer/deployment/wasm-browser-support);
  [iroh 1.0](https://iroh.computer/blog/v1)
- WASM tooling:
  [Sunsetting the rustwasm org](https://blog.rust-lang.org/inside-rust/2025/07/21/sunsetting-the-rustwasm-github-org);
  [Life after wasm-pack](https://nickb-dev.pages.dev/blog/life-after-wasm-pack-an-opinionated-deconstruction);
  [Vite 8.1](https://vite.dev/blog/announcing-vite8-1);
  [Vite features guide](https://vite.dev/guide/features);
  [vite-plugin-wasm](https://github.com/Menci/vite-plugin-wasm/);
  [chacha20 crate backends](https://github.com/RustCrypto/stream-ciphers/tree/master/chacha20);
  [WebAssembly SIMD support](https://caniuse.com/wasm-simd);
  [JSPI support](https://web-platform-dx.github.io/web-features-explorer/features/wasm-jspi);
  [Firefox intent to ship JSPI](https://groups.google.com/a/mozilla.org/g/dev-platform/c/8cOxM-dWaNs);
  [wasm-bindgen copy overhead](https://github.com/rustwasm/wasm-bindgen/issues/1119)
- Browser security:
  [Mozilla, Trustworthy JavaScript for the Open Web (WAICT)](https://hacks.mozilla.org/2026/05/trustworthy-javascript-for-the-open-web/);
  [`wasm-unsafe-eval` support](https://caniuse.com/mdn-http_headers_content-security-policy_script-src_wasm-unsafe-eval);
  [Chrome intent, WebAssembly CSP](https://groups.google.com/a/chromium.org/g/blink-dev/c/5U_SgZ3r8QI/m/pnOzK2S3BAAJ);
  [Trusted Types](https://web.dev/articles/trusted-types);
  [Firefox intent to ship Trusted Types](https://groups.google.com/a/mozilla.org/g/dev-platform/c/WA8vLAbvzB0);
  [WebAuthn PRF support (Corbado)](https://www.corbado.com/blog/passkeys-prf-webauthn);
  [Yubico PRF guide](https://developers.yubico.com/WebAuthn/Concepts/PRF_Extension/Developers_Guide_to_PRF.html)
- Post-quantum and platforms:
  [libcrux ML-KEM verification](https://cryspen.com/post/ml-kem-verification/);
  [libcrux-ml-kem](https://docs.rs/libcrux-ml-kem);
  [Apple WWDC25 session 314, quantum-secure CryptoKit](https://developer.apple.com/videos/play/wwdc2025/314/);
  [Android Keystore](https://developer.android.com/training/articles/keystore)

Not confirmed from a primary source, and marked so above: Safari's first
version for `'wasm-unsafe-eval'`; stable-channel status of Chrome's new
WebCrypto algorithms; Firefox's and Safari's JSPI status; the commit of
`snow` that Trail of Bits reviewed; `libcrux-ml-kem` on WASM; StrongBox and
Curve25519 on current Android; a published WASM against WebCrypto
small-frame benchmark (none found).

## Answers (user, 2026-11-17)

1. Post-quantum hybrid (X25519 + ML-KEM-768) from v1, if the spike
   confirms it works.
2. No long backlog at the relay: the host is the source of truth and a
   device re-syncs on connect.
3. Phone app: a native shell around the same Preact page in a WebView.
   Crypto runs as WASM there too; the native side keeps keys at rest, push
   and the pinned code bundle. UniFFI shrinks to what the shell needs.
4. Host confirmation (device name plus short code) before a newly paired
   device can act.

Defaults taken, not asked: remote browser client deferred; relay auth is
TLS plus signed challenge, mTLS optional, P-256 relay identity;
contentless wake-ups only (no lock-screen previews). Open: who reviews the
protocol before public use.
