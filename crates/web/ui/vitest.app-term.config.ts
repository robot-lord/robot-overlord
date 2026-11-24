// The real Claude Code's terminal through the phone's pane link
// (`cargo xtask app test-term`, which starts a real-Claude demo on the
// fake model, the relay and `rlord-device bridge`, and names them in
// RLORD_BRIDGE_URL and RLORD_TERM_*). A separate project so `web check`
// needs none of it.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["test/app-term/**/*.test.ts"],
    environment: "node",
    fileParallelism: false,
    sequence: { concurrent: false },
    testTimeout: 120_000,
    hookTimeout: 60_000,
  },
});
