// Devices (remote access), from "Devices" in the header: the paired
// devices, a pairing code to scan, the device waiting for the user with its
// code, and confirm, turn away and unpair. Local pages only (the daemon
// refuses these ops from a device). The page holds no keys: it draws the
// code's modules the daemon sends (`qr_rows`).

import { useEffect, useState } from "preact/hooks";
import { Modal } from "../components/Modal";
import { ago } from "../components/time";
import type { Device, DeviceList, PairOffer } from "../proto";
import type { Store } from "../store";

/** Modules of light margin around the code (what scanners want). */
const QUIET = 4;

/**
 * The code's dark modules as one SVG path (a run of dark modules in a row
 * is one rectangle), in a `size` × `size` box with the quiet zone.
 */
export function qrPath(rows: string[], quiet = QUIET): { size: number; d: string } {
  const parts: string[] = [];
  rows.forEach((row, y) => {
    let x = 0;
    while (x < row.length) {
      if (row[x] !== "1") {
        x++;
        continue;
      }
      let n = 1;
      while (row[x + n] === "1") n++;
      parts.push(`M${x + quiet} ${y + quiet}h${n}v1h-${n}z`);
      x += n;
    }
  });
  return { size: rows.length + 2 * quiet, d: parts.join("") };
}

/** A six-digit code as two groups of three. */
export function spacedCode(code: string): string {
  return /^\d{6}$/.test(code) ? `${code.slice(0, 3)} ${code.slice(3)}` : code;
}

/** The relay and the link, in a line. */
export function linkText(l: DeviceList): string {
  if (!l.relay) return "No relay set up: run `rlord relay setup <relay address>` in a terminal.";
  if (!l.enabled) return `${l.relay}: off (\`rlord relay setup\` turns it on).`;
  if (!l.plugin.installed) return `${l.relay}: ${l.plugin.problem ?? "the web plugin is missing"}`;
  if (!l.reported) return `${l.relay}: waiting for the web plugin to report.`;
  if (l.connected) return `${l.relay}: connected.`;
  return `${l.relay}: not connected${l.error ? ` (${l.error})` : ""}.`;
}

/** When a device was last seen (`now` in ms). */
export function seenText(d: Device, now: number): string {
  return d.last_seen === null ? "never seen" : `seen ${ago(d.last_seen * 1000, now)} ago`;
}

/** "4:05" left until `expires` (Unix seconds); null once it has passed. */
export function leftText(expires: number | null, now: number): string | null {
  if (expires === null) return null;
  const s = Math.floor(expires - now / 1000);
  if (s <= 0) return null;
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

/** How long an offer this page made stands before the plugin reports it. */
const OFFER_GRACE_MS = 3000;

/**
 * Whether the offer this page made (at `madeAt`, ms) still stands: no
 * device has taken it, and the plugin still has a pairing open (or hasn't
 * reported the new one yet).
 */
export function offerStands(l: DeviceList, madeAt: number, now: number): boolean {
  if (l.pending !== null) return false;
  return l.pairing !== null || now - madeAt < OFFER_GRACE_MS;
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function Devices({ store }: { store: Store }) {
  const [list, setList] = useState<DeviceList | null>(null);
  const [offer, setOffer] = useState<(PairOffer & { madeAt: number }) | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<Device | null>(null);
  const [now, setNow] = useState(Date.now());
  const rev = store.snapshot.value?.rev;

  const load = async (): Promise<void> => {
    try {
      const l = await store.request<DeviceList>("device.list");
      setList(l);
      setOffer((o) => (o && offerStands(l, o.madeAt, Date.now()) ? o : null));
    } catch (e: unknown) {
      setNote(message(e));
    }
  };
  // Again on every change the daemon announces (a device came, a link).
  useEffect(() => {
    void load();
  }, [rev]);
  // The offer's countdown.
  useEffect(() => {
    if (!offer) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [offer]);

  const run = (
    op: "device.pair" | "device.pair_cancel" | "device.confirm" | "device.reject" | "device.revoke",
    args: object,
    done: (v: unknown) => string | null,
  ): void => {
    const go = async () => {
      setBusy(true);
      setNote(null);
      try {
        const v = await store.request(op, args);
        setNote(done(v));
        await load();
      } catch (e: unknown) {
        setNote(message(e));
      } finally {
        setBusy(false);
      }
    };
    void go();
  };

  const close = () => (store.form.value = null);
  const left = offer ? leftText(offer.expires, now) : null;
  const code = offer ? qrPath(offer.qr_rows) : null;
  const pending = list?.pending ?? null;
  return (
    <Modal title="Devices" onClose={close} busy={busy}>
      {!list && <p class="hint">{note ?? "loading…"}</p>}
      {list && (
        <div class="devices">
          <p class="dim">{linkText(list)}</p>
          {pending && (
            <div class="note pending" role="alert">
              <p>
                "{pending.name}" wants to pair. Its code:{" "}
                <strong class="code">{spacedCode(pending.code)}</strong>
              </p>
              <p class="small dim">Check the device shows the same code before you pair it.</p>
              <div class="buttons">
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => run("device.reject", {}, () => "Turned the device away.")}
                >
                  Turn away
                </button>
                <button
                  type="button"
                  class="primary"
                  disabled={busy}
                  onClick={() =>
                    run("device.confirm", {}, (v) => {
                      const d = (v as { device?: { name?: string } }).device;
                      return `Paired "${d?.name ?? "the device"}".`;
                    })
                  }
                >
                  Pair it
                </button>
              </div>
            </div>
          )}
          {code && !pending && (
            <figure class="qr">
              <svg
                viewBox={`0 0 ${code.size} ${code.size}`}
                role="img"
                aria-label="pairing code"
                shape-rendering="crispEdges"
              >
                <rect width="100%" height="100%" fill="#fff" />
                <path d={code.d} fill="#000" />
              </svg>
              <figcaption class="small dim">
                Scan it on the device you are pairing: one device
                {left ? `, ${left} left` : ", it has run out"}.{" "}
                <button
                  type="button"
                  class="link"
                  disabled={busy}
                  onClick={() =>
                    run("device.pair_cancel", {}, () => {
                      setOffer(null);
                      return "The code is cancelled.";
                    })
                  }
                >
                  Cancel it
                </button>
              </figcaption>
            </figure>
          )}
          <h3>Paired devices</h3>
          {list.devices.length === 0 ? (
            <p class="hint">None yet.</p>
          ) : (
            <table class="keys">
              <tbody>
                {list.devices.map((d) => (
                  <tr key={d.id}>
                    <td>
                      {d.linked ? "● " : ""}
                      {d.name}
                      <div class="small dim">
                        {d.id} · {seenText(d, now)}
                      </div>
                    </td>
                    <td>
                      {revoking?.id === d.id ? (
                        <span class="small">
                          Unpair? It has to pair again to come back.{" "}
                          <button
                            type="button"
                            disabled={busy}
                            onClick={() => {
                              setRevoking(null);
                              run("device.revoke", { device: d.id }, () => `Unpaired "${d.name}".`);
                            }}
                          >
                            Unpair
                          </button>{" "}
                          <button type="button" class="dim" onClick={() => setRevoking(null)}>
                            Keep
                          </button>
                        </span>
                      ) : (
                        <button type="button" class="dim" onClick={() => setRevoking(d)}>
                          Unpair…
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {note && <p class="small">{note}</p>}
          <div class="buttons">
            <button
              type="button"
              class="primary"
              disabled={busy || !list.relay || !list.enabled}
              onClick={() =>
                run("device.pair", {}, (v) => {
                  setOffer({ ...(v as PairOffer), madeAt: Date.now() });
                  return null;
                })
              }
            >
              {offer ? "A new code" : "Pair a device"}
            </button>
          </div>
        </div>
      )}
    </Modal>
  );
}
