// The page's build: static files into dist/, embedded in rlord-web by its
// build.rs. No dev server (its origin would fail the plugin's Origin
// check): `cargo xtask web watch` rebuilds into dist/ for a debug plugin
// run with RLORD_WEB_ASSETS.
import { defineConfig } from "vitest/config";

export default defineConfig({
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // Everything as files: the CSP allows 'self' (no data: fonts or
    // scripts), and the plugin's asset test checks every file is local.
    assetsInlineLimit: 0,
    // No polyfill code for <link rel=modulepreload>: every browser the
    // page targets has it.
    modulePreload: { polyfill: false },
    sourcemap: false,
    reportCompressedSize: false,
    // The bundled packages' licenses, shipped in the plugin's tarball
    // (`cargo xtask build-release`) and served beside the page.
    license: { fileName: "THIRD-PARTY-LICENSES.md" },
  },
  test: {
    include: ["test/**/*.test.ts"],
    // The WASM crypto core's tests need its build (`cargo xtask e2e
    // test-wasm` runs them with vitest.e2e.config.ts).
    exclude: ["test/e2e/**", "node_modules/**"],
    environment: "node",
  },
});
