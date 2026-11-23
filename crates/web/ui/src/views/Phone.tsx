// The phone app's own screens: pairing (scan, name, the six digits) and
// the status screen (`#/status`: the computer, the relay, the connection,
// this phone's keys, notifications and app lock, Unpair, versions), and
// the header's connection line.

import { useEffect, useState } from "preact/hooks";
import { ConfirmButton } from "../components/Actions";
import {
  connBad,
  connWords,
  deviceName,
  keysWords,
  NAME_MAX,
  type Phone,
  relayName,
  spaced,
} from "../phone";
import type { AppMessage } from "../proto";

export type Send = (m: AppMessage) => void;

/** The header's connection line: the state in words, with the host's name. */
export function ConnLine({ phone }: { phone: Phone }) {
  const c = phone.conn.value;
  const bad = c ? connBad(c.state) : false;
  const cls = !c ? "connecting" : c.state === "live" ? "open" : bad ? "closed" : "connecting";
  return <span class={`conn conn-${cls}`}>{connWords(c)}</span>;
}

function NameStep({
  relay,
  name,
  send,
  cancel,
}: {
  relay: string;
  name: string;
  send: Send;
  cancel: () => void;
}) {
  const [n, setN] = useState(name);
  return (
    <form
      class="pair-form"
      onSubmit={(e) => {
        e.preventDefault();
        send({ app: "pair_continue", name: deviceName(n) || name });
      }}
    >
      <p>
        This computer is reached through <strong>{relayName(relay)}</strong>.
      </p>
      <label class="field wide">
        <span class="field-name">A name for this phone</span>
        <input
          type="text"
          value={n}
          maxLength={NAME_MAX}
          autocomplete="off"
          onInput={(e) => setN(e.currentTarget.value)}
        />
      </label>
      <div class="buttons">
        <button type="submit" class="primary">
          Continue
        </button>
        <button type="button" onClick={cancel}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** Pairing, from the first screen to the six digits. */
export function Pairing({ phone, send }: { phone: Phone; send: Send }) {
  const p = phone.pairing.value;
  const cancel = () => {
    send({ app: "pair_cancel" });
    phone.pairing.value = { kind: "start", reason: null };
  };
  let body;
  switch (p.kind) {
    case "start":
    case "done":
      body = (
        <>
          <p>
            On your computer run <code>rlord relay pair</code> (or open Devices in rlord), then scan
            the code it shows.
          </p>
          {p.kind === "start" && p.reason && <p class="err-line">{p.reason}</p>}
          <button type="button" class="primary big" onClick={() => send({ app: "scan" })}>
            Scan
          </button>
        </>
      );
      break;
    case "scanned":
      body = <NameStep relay={p.relay} name={p.name} send={send} cancel={cancel} />;
      break;
    case "connecting":
      body = (
        <>
          <p>Reaching your computer…</p>
          <button type="button" onClick={cancel}>
            Cancel
          </button>
        </>
      );
      break;
    case "waiting":
      body = (
        <>
          <p>Check that your computer shows</p>
          <p class="code" aria-label={`code ${p.code.split("").join(" ")}`}>
            {spaced(p.code)}
          </p>
          <p>and confirm there.</p>
          <button type="button" onClick={cancel}>
            Cancel
          </button>
        </>
      );
      break;
  }
  return (
    <section class="pair">
      <h1>Pair this phone</h1>
      {body}
    </section>
  );
}

function Toggle({
  label,
  note,
  on,
  set,
}: {
  label: string;
  note: string;
  on: boolean | null;
  set: (v: boolean) => void;
}) {
  return (
    <label class="toggle">
      <span>
        {label}
        <span class="dim small toggle-note">{note}</span>
      </span>
      <input
        type="checkbox"
        checked={on ?? false}
        disabled={on === null}
        onChange={(e) => set(e.currentTarget.checked)}
      />
    </label>
  );
}

/** This phone and its link to the computer (`#/status`). */
export function StatusScreen({ phone, send }: { phone: Phone; send: Send }) {
  useEffect(() => {
    send({ app: "status" });
    // Sets nothing: the shell answers with its settings.
    send({ app: "settings" });
  }, []);
  const st = phone.status.value;
  const c = phone.conn.value;
  const set = phone.settings.value;
  const hello = phone.hello.value;
  const host = st?.host ?? c?.host;
  const relay = st?.relay ?? c?.relay;
  return (
    <section class="view status-screen">
      <header class="view-head">
        <a class="back" href="#/" aria-label="back to the list">
          ‹
        </a>
        <h1>This phone</h1>
      </header>
      <dl class="facts">
        <dt>Computer</dt>
        <dd>{host ?? "not paired"}</dd>
        <dt>Relay</dt>
        <dd>{relay ? relayName(relay) : "none"}</dd>
        <dt>Connection</dt>
        <dd>{connWords(c)}</dd>
        <dt>This phone</dt>
        <dd>
          {st?.device_name ?? "…"}
          {st?.device_id && <span class="dim"> · {st.device_id}</span>}
        </dd>
        <dt>Keys</dt>
        <dd>{st ? keysWords(st.keys) : "…"}</dd>
        <dt>Versions</dt>
        <dd>
          {st ? `app ${st.version} · API ${st.api.min}–${st.api.max}` : "…"}
          {hello && ` · rlord-web ${hello.version}`}
        </dd>
      </dl>
      <div class="toggles">
        <Toggle
          label="Notifications"
          note="A count of sessions that need you, when the phone lets the app look (not at once)."
          on={set?.notifications ?? null}
          set={(v) => send({ app: "settings", notifications: v })}
        />
        <Toggle
          label="App lock"
          note={
            set?.platform === "android"
              ? "Your fingerprint, face or screen lock after 5 minutes away."
              : "Face ID, Touch ID or the passcode after 5 minutes away."
          }
          on={set?.app_lock ?? null}
          set={(v) => send({ app: "settings", app_lock: v })}
        />
      </div>
      {host && (
        <div class="actions unpair">
          <ConfirmButton
            label="Unpair this phone"
            ask="Unpair? Pairing again needs a new code from your computer."
            onConfirm={() => send({ app: "unpair" })}
          />
        </div>
      )}
    </section>
  );
}
