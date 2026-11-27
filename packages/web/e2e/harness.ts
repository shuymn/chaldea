// The end-to-end fixtures (docs/design/w1-pwa.md §3 D8): each test gets its own daemon
// (e2e/daemon.ts) under a scratch root in the OS temp directory, removed on every exit path.
// Pair codes and tokens never reach a test title, step, message, or attachment: assertions
// on them report booleans, and diagnostics pass through `redact`.

import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { test as base, expect, type Page } from "@playwright/test";

import { Daemon, freePort } from "./daemon";

export { Daemon, expect };

/**
 * Opens a pairing link in `page` without handing the URL to a Playwright navigation, whose
 * call log would print the fragment: the page navigates itself from `about:blank`.
 */
export async function openLink(page: Page, url: string): Promise<void> {
  await page.goto("about:blank");
  await page.evaluate((u) => {
    location.href = u;
  }, url);
  await page.waitForFunction(() => location.pathname === "/" && document.getElementById("root") !== null);
}

/** Whether the page's URL still holds a pairing fragment (only the boolean leaves the page). */
export const hasFragment = (page: Page) => page.evaluate(() => location.hash !== "");

/** The status the header shows (`data-status`). */
export const statusOf = (page: Page) => page.locator("[data-status]");

/** The `#<seq>` of every timeline row, in page order. */
export async function shownSeqs(page: Page): Promise<number[]> {
  const texts = await page.locator("main ol > li p:first-child > span:first-child").allTextContents();
  return texts.map((t) => Number(t.replace(/^#/, "")));
}

/** Waits until the timeline's last row is `#<seq>`. */
export async function untilShown(page: Page, seq: number): Promise<void> {
  await expect(page.locator("main ol > li").last().locator("span").first()).toHaveText(`#${seq}`, {
    timeout: 20_000,
  });
}

export const test = base.extend<{ daemon: Daemon }>({
  daemon: async ({}, use, info) => {
    const root = mkdtempSync(join(tmpdir(), "chaldea-e2e-"));
    let d: Daemon | null = null;
    let stopFailure: unknown = null;
    try {
      d = new Daemon({ root, port: await freePort() });
      await d.start();
      await use(d);
    } finally {
      try {
        await d?.stop();
      } catch (e) {
        stopFailure = e;
      }
      try {
        if (d && info.status !== info.expectedStatus) {
          await info.attach("daemon.log", { body: d.log, contentType: "text/plain" });
        }
      } catch {
        // The attachment is a diagnostic; the cleanup below runs regardless.
      }
      rmSync(root, { recursive: true, force: true });
    }
    if (stopFailure) throw stopFailure;
  },
  baseURL: async ({ daemon }, use) => {
    await use(daemon.origin);
  },
});
