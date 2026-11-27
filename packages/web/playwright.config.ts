// Playwright against the release `chaldea` (docs/design/w1-pwa.md §3 D8, §4 W1-4, W1-8..W1-12):
// Chromium and WebKit, each at a desktop size and at 390 × 844. `e2e/global-setup.ts` builds
// the binaries; packages/web/dist must already be built (`mise run release` does both).
//
// Traces and videos are off: a trace records request and response bodies, and the pairing
// exchange carries the pair code and the token (`/api/pair`). Screenshots, kept only on
// failure, show the page, which never displays either.

import { defineConfig, devices } from "@playwright/test";

const mobile = { viewport: { width: 390, height: 844 } };

export default defineConfig({
  testDir: "e2e",
  globalSetup: "./e2e/global-setup.ts",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: 0,
  workers: process.env.CI ? 3 : 4,
  timeout: 60_000,
  reporter: [["list"]],
  use: {
    trace: "off",
    video: "off",
    screenshot: "only-on-failure",
  },
  projects: [
    { name: "chromium-desktop", use: { ...devices["Desktop Chrome"] } },
    { name: "webkit-desktop", use: { ...devices["Desktop Safari"] } },
    { name: "chromium-390", use: { ...devices["Desktop Chrome"], ...mobile } },
    { name: "webkit-390", use: { ...devices["Desktop Safari"], ...mobile } },
  ],
});
