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

/** Pairs `page` and selects `thread`; resolves once the page is live. */
export async function openThread(page: Page, daemon: Daemon, thread: string): Promise<void> {
  await openLink(page, await daemon.pairUrl());
  await page.locator(`li[data-thread="${thread}"] button`).first().click();
  await expect(statusOf(page)).toHaveText("live");
}

/** The bodies of every `POST /api/commands/<name>` the page sends from now on. */
export function commandPosts(page: Page, name: string): string[] {
  const bodies: string[] = [];
  page.on("request", (r) => {
    if (r.method() === "POST" && new URL(r.url()).pathname === `/api/commands/${name}`) bodies.push(r.postData() ?? "");
  });
  return bodies;
}

/** The selected thread's composer textarea and its Send button. */
export const composer = (page: Page) => page.getByLabel("Message", { exact: true });
export const sendButton = (page: Page) => page.getByRole("form", { name: "Composer" }).getByRole("button", { name: "Send", exact: true });

/** The command line of a slot (`send:<thread>`, `spawn`, …). */
export const commandLine = (page: Page, slot: string) => page.locator(`[data-slot="${slot}"]`);

export const test = base.extend<{ daemon: Daemon; claude: "echo" | "missing" }>({
  // What runs as `claude` (e2e/daemon.ts): FakeCli `echo`, or nothing (`test.use({ claude: "missing" })`).
  claude: ["echo", { option: true }],
  daemon: async ({ claude }, use, info) => {
    const root = mkdtempSync(join(tmpdir(), "chaldea-e2e-"));
    let d: Daemon | null = null;
    let stopFailure: unknown = null;
    try {
      d = new Daemon({ root, port: await freePort(), claude });
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
