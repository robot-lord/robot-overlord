// The page's build: static files into dist/, embedded in rlord-web by its
// build.rs. No dev server (its origin would fail the plugin's Origin
// check): `cargo xtask web watch` rebuilds into dist/ for a debug plugin
// run with RLORD_WEB_ASSETS.
//
// `--mode app` builds the phone app's page instead (app.html, src/app.tsx)
// into dist-app/, which the app bundles and serves itself: `cargo xtask
// web build --app` names it index.html, writes its asset table and runs
// the checks on it (no network, xterm.js for its terminal, no LocalTransport).
//
// `--mode app-dev` builds the same page for a desktop browser
// (app-dev.html, src/appDev.ts) into dist-app-dev/, with the dev bridge
// to `rlord-device bridge` on VITE_RLORD_BRIDGE_PORT: `cargo xtask app
// dev` only, never shipped.
import { defineConfig } from "vitest/config";

export default defineConfig(({ mode }) => {
  const entries: Record<string, string> = { app: "app.html", "app-dev": "app-dev.html" };
  const entry = entries[mode];
  return {
    build: {
      outDir: entry ? `dist-${mode}` : "dist",
      emptyOutDir: true,
      ...(entry ? { rolldownOptions: { input: entry } } : {}),
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
      // test-wasm` runs them with vitest.e2e.config.ts); the phone page's
      // end-to-end tests need a host and the bridge (`cargo xtask app
      // test-e2e`, vitest.app-e2e.config.ts), and its terminal tests a
      // real-Claude demo too (`app test-term`, vitest.app-term.config.ts).
      exclude: ["test/e2e/**", "test/app-e2e/**", "test/app-term/**", "node_modules/**"],
      environment: "node",
    },
  };
});
