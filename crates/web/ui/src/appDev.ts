// The phone page in a desktop browser (app-dev.html, `cargo xtask app
// dev`): the dev bridge plays the shell over `rlord-device bridge`'s
// loopback websocket, then the phone entry runs as it does in the app.
// Never in the phone app's bundle (dist-app/).

import { installDevBridge } from "./devBridge";
import type { BridgeHost } from "./remote";

const port = String(import.meta.env.VITE_RLORD_BRIDGE_PORT ?? "");
installDevBridge(window as unknown as BridgeHost, `ws://127.0.0.1:${port}/`, WebSocket, () => {
  // The bridge stopped or another tab took it: start over shortly.
  setTimeout(() => location.reload(), 2000);
});
// After the bridge is in place: the entry looks for it as it starts.
await import("./app");
