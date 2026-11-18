// Loads the WASM crypto core built by `cargo xtask e2e wasm` (bindgen
// `--target web` output) into Node, from bytes.
import { readFileSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";

const pkg = fileURLToPath(new URL("../../../../e2e-wasm/pkg/", import.meta.url));

/** The parts of the generated module the tests use. */
export interface Established {
  short_code(): number;
  handshake_hash(): Uint8Array;
  remote_static(): Uint8Array;
  into_session(): Session;
  into_mailbox(): Mailbox;
}
export interface Session {
  buffer_ptr(): number;
  region_size(): number;
  encrypt(len: number): number;
  decrypt(len: number): number;
  is_poisoned(): boolean;
}
export interface Mailbox {
  export(): Uint8Array;
  id(): Uint8Array;
  encrypt(p: Uint8Array): Uint8Array;
  decrypt(f: Uint8Array): Uint8Array;
}
export interface Handshake {
  message(): Uint8Array;
  finish(msg2: Uint8Array): Established;
}
export interface Answer {
  message(): Uint8Array;
  established(): Established;
}
export interface E2e {
  memory: WebAssembly.Memory;
  protocol_info(): string;
  generate_static_key(): Uint8Array;
  device_id(pub: Uint8Array): string;
  run_vectors(inputs: string): string;
  fragment(stream: number, msg: Uint8Array): Uint8Array;
  DeviceHandshake: {
    pair(qr: string, priv: Uint8Array, origin: string, fp: Uint8Array): Handshake;
    reconnect(priv: Uint8Array, host: Uint8Array, origin: string, fp: Uint8Array): Handshake;
    mailbox(priv: Uint8Array, host: Uint8Array, origin: string, fp: Uint8Array): Handshake;
  };
  HostAnswer: {
    pair(priv: Uint8Array, qr: string, origin: string, fp: Uint8Array, m1: Uint8Array): Answer;
    reconnect(
      priv: Uint8Array,
      dev: Uint8Array,
      origin: string,
      fp: Uint8Array,
      m1: Uint8Array,
    ): Answer;
  };
  E2eMailbox: { import(b: Uint8Array): Mailbox };
}

export async function load(): Promise<E2e> {
  const mod = (await import(pathToFileURL(`${pkg}rlord_e2e_wasm.js`).href)) as {
    initSync(o: { module: Uint8Array }): { memory: WebAssembly.Memory };
  } & Omit<E2e, "memory">;
  const exports = mod.initSync({ module: readFileSync(`${pkg}rlord_e2e_wasm_bg.wasm`) });
  return { ...mod, memory: exports.memory };
}
