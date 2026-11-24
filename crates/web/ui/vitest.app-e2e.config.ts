// The phone page's scripted end-to-end tests (`cargo xtask app test-e2e`,
// which starts the loopback host and `rlord-device bridge` and names them
// in RLORD_BRIDGE_URL and RLORD_STACK_CONTROL). A separate project so
// `web check` needs neither.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["test/app-e2e/**/*.test.ts"],
    environment: "node",
    // One host, one engine: the steps run in order.
    fileParallelism: false,
    sequence: { concurrent: false },
    testTimeout: 120_000,
    hookTimeout: 60_000,
  },
});
