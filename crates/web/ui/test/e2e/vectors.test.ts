// The committed vectors, run by the WASM build: byte for byte what the
// native core wrote (crates/e2e/vectors/v1.json).
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { load } from "./wasm";

const committed = JSON.parse(
  readFileSync(new URL("../../../../e2e/vectors/v1.json", import.meta.url), "utf8"),
) as { inputs: unknown; outputs: { failures: Record<string, string> } };

describe("the WASM core", () => {
  it("reproduces the committed vectors", async () => {
    const e2e = await load();
    expect(e2e.protocol_info()).toContain("rlord-e2e v1");
    const out = JSON.parse(e2e.run_vectors(JSON.stringify(committed.inputs))) as unknown;
    expect(out).toEqual(committed.outputs);
    const passing = Object.entries(committed.outputs.failures).filter(([, r]) => r === "ok");
    expect(passing).toEqual([]);
  });

  it("fragments a large message into frames", async () => {
    const e2e = await load();
    const records = e2e.fragment(0, new Uint8Array(40_000));
    // Three frames: u16 length | stream | flags | chunk.
    let n = 0;
    for (let i = 0; i < records.length; n++) {
      const len = ((records[i] ?? 0) << 8) | (records[i + 1] ?? 0);
      expect(records[i + 3]).toBe(i + 2 + len < records.length ? 1 : 0);
      i += 2 + len;
    }
    expect(n).toBe(3);
  });
});
