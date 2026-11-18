// The WASM crypto core's tests (`cargo xtask e2e test-wasm`, which builds
// crates/e2e-wasm/pkg first). A separate project so `web check` doesn't
// need the WASM build.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["test/e2e/**/*.test.ts"],
    environment: "node",
    testTimeout: 60_000,
  },
});
