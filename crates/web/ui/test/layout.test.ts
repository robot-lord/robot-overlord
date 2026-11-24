// The phone terminal's sizing (term/layout.ts): font steps kept per
// device, rows for the whole view, a viewer's scale, a drag split
// between sliding and the wheel.

import { describe, expect, it } from "vitest";
import {
  Drag,
  FONT_DEFAULT,
  fitScale,
  fontStep,
  fullHeight,
  loadFont,
  saveFont,
} from "../src/term/layout";

function memory() {
  const m = new Map<string, string>();
  return {
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
  };
}

describe("font size", () => {
  it("is 12 px until picked, steps 10 to 16, and is kept", () => {
    const s = memory();
    expect(loadFont(s)).toBe(FONT_DEFAULT);
    expect(FONT_DEFAULT).toBe(12);
    expect(fontStep(12, 1)).toBe(13);
    expect(fontStep(16, 1)).toBe(16);
    expect(fontStep(10, -1)).toBe(10);
    saveFont(s, 14);
    expect(loadFont(s)).toBe(14);
    s.setItem("rlord.termFont", "40");
    expect(loadFont(s)).toBe(FONT_DEFAULT);
    expect(loadFont(null)).toBe(FONT_DEFAULT);
  });
});

describe("the keyboard never resizes the terminal", () => {
  it("lays it out for the box as it is with the keyboard down", () => {
    // No keyboard: the box as it is.
    expect(fullHeight(600, 844, 844)).toBe(600);
    // The keyboard took 336 points from the visual viewport, and the box.
    expect(fullHeight(264, 844, 508)).toBe(600);
  });
});

describe("a viewer's scale", () => {
  it("fits the window's whole screen, never enlarged", () => {
    expect(fitScale(390, 600, 780, 400)).toBe(0.5);
    expect(fitScale(390, 600, 390, 1200)).toBe(0.5);
    expect(fitScale(800, 600, 400, 300)).toBe(1);
    expect(fitScale(0, 600, 400, 300)).toBe(1);
  });
});

describe("a vertical drag", () => {
  it("slides the terminal first, then becomes wheel rows", () => {
    const d = new Drag();
    // The keyboard hides 100 px of the top; a cell is 20 px.
    expect(d.move(60, 100, 20)).toEqual({ pan: 60, wheel: 0 });
    // 40 more px slide it to its top; 30 px past it are a row and a half.
    expect(d.move(70, 100, 20)).toEqual({ pan: 100, wheel: 1 });
    expect(d.move(10, 100, 20)).toEqual({ pan: 100, wheel: 1 });
    // Back up: wheel down only once it slid back to the bottom.
    d.end();
    expect(d.move(-90, 100, 20)).toEqual({ pan: 10, wheel: 0 });
    expect(d.move(-50, 100, 20)).toEqual({ pan: 0, wheel: -2 });
  });

  it("with nothing hidden, every row of drag is a wheel report", () => {
    const d = new Drag();
    expect(d.move(45, 0, 15)).toEqual({ pan: 0, wheel: 3 });
    expect(d.move(-14, 0, 15)).toEqual({ pan: 0, wheel: 0 });
    expect(d.move(-1, 0, 15)).toEqual({ pan: 0, wheel: -1 });
  });

  it("a keyboard going down clamps the slide", () => {
    const d = new Drag();
    d.move(80, 100, 20);
    expect(d.move(0, 30, 20)).toEqual({ pan: 30, wheel: 0 });
  });
});
