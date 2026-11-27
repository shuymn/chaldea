// The PWA build (docs/design/w1-pwa.md §2.1). The output must pass the daemon's CSP
// (`default-src 'self'`, Trusted Types required with no policy allowed): no inline script
// or style, no `data:` asset, no `eval`. The daemon embeds `dist/` at compile time
// (crates/daemon/build.rs), which accepts only `index.html` and `assets/`.

import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  base: "/",
  publicDir: false,
  plugins: [react(), tailwindcss()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsDir: "assets",
    // Every asset is a file: an inlined one would be a `data:` URL.
    assetsInlineLimit: 0,
    // The preload polyfill is a helper the page does not need: every supported browser
    // has modulepreload.
    modulePreload: { polyfill: false },
    sourcemap: false,
    // One chunk, most of it the standalone validators of every wire schema.
    chunkSizeWarningLimit: 1536,
    target: "es2023",
  },
  test: {
    include: ["src/**/*.test.{ts,tsx}", "e2e/**/*.test.ts"],
    environment: "node",
  },
});
