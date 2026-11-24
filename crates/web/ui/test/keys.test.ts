// The key row's bytes and the touch wheel's (term/keys.ts): what Claude
// Code and tmux expect of a terminal.

import { describe, expect, it } from "vitest";
import {
  applyMods,
  type Key,
  keyBytes,
  PASTE_MAX,
  tooLong,
  wheel,
  withCtrl,
} from "../src/term/keys";

const plain = { ctrl: false, alt: false, appCursor: false };

describe("the key row's keys", () => {
  it("send what a terminal sends", () => {
    const want: [Key, string][] = [
      ["esc", "\x1b"],
      ["tab", "\t"],
      ["backtab", "\x1b[Z"],
      ["enter", "\r"],
      ["ctrl_c", "\x03"],
      ["up", "\x1b[A"],
      ["down", "\x1b[B"],
      ["right", "\x1b[C"],
      ["left", "\x1b[D"],
    ];
    for (const [k, b] of want) expect([k, keyBytes(k, plain)]).toEqual([k, b]);
  });

  it("arrows follow application cursor mode", () => {
    const app = { ...plain, appCursor: true };
    expect(["up", "down", "right", "left"].map((k) => keyBytes(k as Key, app))).toEqual([
      "\x1bOA",
      "\x1bOB",
      "\x1bOC",
      "\x1bOD",
    ]);
  });

  it("modified arrows take xterm's parameter, in either cursor mode", () => {
    for (const appCursor of [false, true]) {
      expect(keyBytes("up", { ctrl: true, alt: false, appCursor })).toBe("\x1b[1;5A");
      expect(keyBytes("left", { ctrl: false, alt: true, appCursor })).toBe("\x1b[1;3D");
      expect(keyBytes("right", { ctrl: true, alt: true, appCursor })).toBe("\x1b[1;7C");
    }
  });

  it("Alt prefixes Escape; ^C and ⇧Tab ignore the modifiers", () => {
    const alt = { ...plain, alt: true };
    expect(keyBytes("esc", alt)).toBe("\x1b\x1b");
    expect(keyBytes("enter", alt)).toBe("\x1b\r");
    expect(keyBytes("tab", alt)).toBe("\x1b\t");
    const both = { ctrl: true, alt: true, appCursor: true };
    expect(keyBytes("ctrl_c", both)).toBe("\x03");
    expect(keyBytes("backtab", both)).toBe("\x1b[Z");
  });
});

describe("Ctrl and Alt on typed keys", () => {
  it("Ctrl makes a letter its control character, either case", () => {
    expect(withCtrl("c")).toBe("\x03");
    expect(withCtrl("C")).toBe("\x03");
    expect(withCtrl("a")).toBe("\x01");
    expect(withCtrl("z")).toBe("\x1a");
    expect(withCtrl("d")).toBe("\x04");
  });

  it("and the symbols and digits a terminal maps", () => {
    expect(withCtrl("@")).toBe("\x00");
    expect(withCtrl(" ")).toBe("\x00");
    expect(withCtrl("[")).toBe("\x1b");
    expect(withCtrl("\\")).toBe("\x1c");
    expect(withCtrl("]")).toBe("\x1d");
    expect(withCtrl("^")).toBe("\x1e");
    expect(withCtrl("_")).toBe("\x1f");
    expect(withCtrl("?")).toBe("\x7f");
    expect(withCtrl("2")).toBe("\x00");
    expect(withCtrl("8")).toBe("\x7f");
  });

  it("leaves what has no control character, and only the first character changes", () => {
    expect(withCtrl("é")).toBe("é");
    expect(withCtrl("1")).toBe("1");
    expect(withCtrl("\x1b[A")).toBe("\x1b[A");
    expect(withCtrl("")).toBe("");
    expect(withCtrl("cd")).toBe("\x03d");
  });

  it("applies both: Ctrl first, then the Escape prefix", () => {
    expect(applyMods("x", { ctrl: false, alt: false })).toBe("x");
    expect(applyMods("b", { ctrl: false, alt: true })).toBe("\x1bb");
    expect(applyMods("c", { ctrl: true, alt: false })).toBe("\x03");
    expect(applyMods("c", { ctrl: true, alt: true })).toBe("\x1b\x03");
    expect(applyMods("", { ctrl: true, alt: true })).toBe("");
  });
});

describe("the touch wheel and paste", () => {
  it("is SGR wheel reports, 64 up and 65 down, at a 1-based cell", () => {
    expect(wheel(true, 10, 5)).toBe("\x1b[<64;10;5M");
    expect(wheel(false, 1, 1)).toBe("\x1b[<65;1;1M");
    expect(wheel(true, 0, -3)).toBe("\x1b[<64;1;1M");
  });

  it("a paste over 64 KiB of UTF-8 is too long", () => {
    expect(PASTE_MAX).toBe(65_536);
    expect(tooLong("a".repeat(PASTE_MAX))).toBe(false);
    expect(tooLong("a".repeat(PASTE_MAX + 1))).toBe(true);
    // 3 bytes each: 21 846 of them are 65 538 bytes.
    expect(tooLong("€".repeat(21_845))).toBe(false);
    expect(tooLong("€".repeat(21_846))).toBe(true);
  });
});
