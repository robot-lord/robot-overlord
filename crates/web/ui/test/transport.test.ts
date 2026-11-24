import { describe, expect, it } from "vitest";
import { LocalTransport } from "../src/api";
import { LOCAL_CAPS, menu, REMOTE_CAPS } from "../src/transport";
import { FakeSock } from "./fake";

describe("caps", () => {
  it("the local page can do everything but hold an outbox", () => {
    const t = new LocalTransport(
      "ws://127.0.0.1:1/ws",
      "a".repeat(64),
      {
        state: () => {},
        hello: () => {},
        changed: () => {},
      },
      (url) => new FakeSock(url),
    );
    expect(t.caps).toEqual({
      panes: true,
      tell: true,
      setup: true,
      keys: true,
      devices: true,
      pending: false,
    });
    expect(menu(t.caps)).toEqual(["setup", "keys", "devices"]);
    t.stop();
  });

  it("a device gets panes, but no tell box and none of the local-only screens", () => {
    expect(REMOTE_CAPS).toEqual({
      panes: true,
      tell: false,
      setup: false,
      keys: false,
      devices: false,
      pending: true,
    });
    expect(menu(REMOTE_CAPS)).toEqual([]);
    expect(menu({ ...LOCAL_CAPS, keys: false })).toEqual(["setup", "devices"]);
  });
});
