// The phone app's own state, from the engine's and shell's events: the
// connection, pairing, the outbox, the status screen's facts and the
// shell's settings. The words the screens use for them are here too,
// pure, so tests read them by event sequence.

import { signal } from "@preact/signals";
import type {
  AppEvent,
  ConnEvent,
  ConnKind,
  Hello,
  OutboxItem,
  PairingEvent,
  SettingsEvent,
  StatusEvent,
} from "./proto";

/** What the pairing screens show. */
export type PairScreen =
  /** Unpaired: how to start, and Scan. `reason`: why the last try ended. */
  | { kind: "start"; reason: string | null }
  /** The QR read: the relay's origin and a name, then Continue. */
  | { kind: "scanned"; relay: string; name: string }
  | { kind: "connecting" }
  /** The six digits to check on the computer. */
  | { kind: "waiting"; code: string }
  | { kind: "done" };

/** A device name: at most this many characters. */
export const NAME_MAX = 40;

/** The name as sent: trimmed, at most 40 characters. */
export function deviceName(s: string): string {
  // (By code points, as the engine counts.)
  return Array.from(s.trim()).slice(0, NAME_MAX).join("");
}

/** The pairing screen after `ev`. */
export function pairStep(ev: PairingEvent): PairScreen {
  switch (ev.state) {
    case "scanned":
      return { kind: "scanned", relay: ev.relay ?? "", name: deviceName(ev.name ?? "") };
    case "connecting":
      return { kind: "connecting" };
    case "waiting":
      return { kind: "waiting", code: ev.code ?? "" };
    case "done":
      return { kind: "done" };
    case "rejected":
      return { kind: "start", reason: ev.reason ?? "Your computer turned this phone down." };
    case "failed":
      return { kind: "start", reason: ev.reason ?? "Pairing failed." };
  }
}

/** A relay's origin as people read it: without its scheme. */
export function relayName(origin: string): string {
  return origin.replace(/^[a-z][a-z0-9+.-]*:\/\//i, "").replace(/\/$/, "");
}

/** "123 456". */
export function spaced(code: string): string {
  return /^\d{6}$/.test(code) ? `${code.slice(0, 3)} ${code.slice(3)}` : code;
}

/** Which screen the app shows. */
export type Screen = "starting" | "pair" | "main";

export function screen(conn: ConnEvent | null, pair: PairScreen): Screen {
  if (pair.kind === "scanned" || pair.kind === "connecting" || pair.kind === "waiting") {
    return "pair";
  }
  if (!conn) return "starting";
  if (conn.state === "unpaired" || conn.state === "revoked") return "pair";
  return "main";
}

/** The host's name, or a stand-in. */
function hostName(c: { host?: string } | null): string {
  return c?.host || "your computer";
}

/** The connection in words, with the host's name (the header). */
export function connWords(c: ConnEvent | null): string {
  if (!c) return "starting…";
  const host = hostName(c);
  const base = ((): string => {
    switch (c.state) {
      case "unpaired":
        return "not paired";
      case "offline":
        return "no network";
      case "connecting":
        return `connecting to ${host}…`;
      case "relay":
        return `reached the relay, waiting for ${host}…`;
      case "host_offline":
        return `${host} is offline`;
      case "live":
        return `${host}: connected`;
      case "wrong_relay":
        return `the relay isn't the one ${host} uses: pair again`;
      case "revoked":
        return `${host} no longer knows this phone`;
      case "update_app":
        return `update this app to talk to ${host}`;
      case "update_host":
        return `update rlord on ${host}`;
      case "suspended":
        return "the relay suspended this phone";
    }
  })();
  return c.detail ? `${base} (${c.detail})` : base;
}

/** A state the user has to act on (the header shows it as an error). */
export function connBad(k: ConnKind): boolean {
  return (
    k === "wrong_relay" ||
    k === "revoked" ||
    k === "update_app" ||
    k === "update_host" ||
    k === "suspended"
  );
}

/** Where the keys are, in words. */
export function keysWords(k: StatusEvent["keys"]): string {
  return k === "hardware"
    ? "in the phone's secure hardware"
    : "in software (no secure hardware: a simulator or debug build)";
}

/**
 * The page's box, from the visual viewport: the part of the screen the
 * keyboard leaves. phone.css lays the app over it, so the composer stays
 * above the keyboard.
 */
export function viewportVars(vv: { height: number; offsetTop: number }): Record<string, string> {
  return {
    "--vvh": `${Math.max(0, Math.round(vv.height))}px`,
    "--vvtop": `${Math.max(0, Math.round(vv.offsetTop))}px`,
  };
}

/** The phone's own state, as signals. */
export class Phone {
  readonly conn = signal<ConnEvent | null>(null);
  readonly pairing = signal<PairScreen>({ kind: "start", reason: null });
  readonly outbox = signal<OutboxItem[]>([]);
  readonly status = signal<StatusEvent | null>(null);
  readonly settings = signal<SettingsEvent | null>(null);
  /** The host's last hello (its version, for the status screen). */
  readonly hello = signal<Hello | null>(null);

  apply(ev: AppEvent): void {
    switch (ev.event) {
      case "conn": {
        const was = this.conn.peek();
        this.conn.value = ev;
        if (ev.state === "revoked") {
          this.pairing.value = {
            kind: "start",
            reason: `This phone is no longer paired with ${hostName(ev.host ? ev : was)}. Pair it again to use it.`,
          };
        } else if (ev.state === "unpaired" && this.pairing.peek().kind === "done") {
          this.pairing.value = { kind: "start", reason: null };
        }
        if (ev.state === "unpaired") this.hello.value = null;
        return;
      }
      case "pairing":
        this.pairing.value = pairStep(ev);
        return;
      case "outbox":
        this.outbox.value = ev.items;
        return;
      case "status":
        this.status.value = ev;
        return;
      case "settings":
        this.settings.value = ev;
        return;
    }
  }
}
