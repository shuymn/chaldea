// The production CSP (docs/design/w1-pwa.md §2.1; W1-11): the built app raises no violation
// while it pairs, lists, decodes events with the generated validators, and reads a live
// socket; an injected sink is blocked.

import type { Page } from "@playwright/test";

import { caughtUpWith, expect, inputsOf, openLink, selectThread, statusOf, test } from "./harness";

declare global {
  interface Window {
    __violations?: string[];
    __inlineRan?: boolean;
  }
}

/** Records every `securitypolicyviolation` from the first script of each document on. */
async function recordViolations(page: Page): Promise<void> {
  await page.addInitScript(() => {
    window.__violations = [];
    document.addEventListener("securitypolicyviolation", (e) => {
      window.__violations?.push(`${e.effectiveDirective} ${e.blockedURI}`);
    });
  });
}

const violations = (page: Page) => page.evaluate(() => window.__violations ?? null);

test("csp_clean", async ({ daemon, page }) => {
  test.setTimeout(120_000);
  await recordViolations(page);
  const { thread } = await daemon.spawnThread({ prompt: "CSP の確認 🎌" });
  await openLink(page, await daemon.pairUrl());
  await selectThread(page, thread);
  // Typed rows: the page decoded the events with the validators under this CSP.
  const timeline = page.locator("main ol");
  await expect(timeline.getByText("thread registered").or(timeline.getByText("launch reserved")).first()).toBeVisible();
  await expect(timeline.getByText("CSP の確認 🎌").first()).toBeVisible();
  await expect(statusOf(page)).toHaveText("live");
  // A live event over the socket.
  await daemon.chaldea(["send", thread, "live over the socket"]);
  await expect(timeline.getByText("live over the socket").first()).toBeVisible();
  await expect(page.getByText("unknown event")).toHaveCount(0);
  // The spawn prompt's writes end with its last state, `acked`, or `unconfirmed` past the
  // 30 s ack window when its hook came before the write completed; the echo CLI never ends
  // the turn, so the CLI's input stays queued. Then the page holds them and the composer is ready.
  await caughtUpWith(
    page,
    daemon,
    thread,
    (xs) => {
      const [spawned, sent] = inputsOf(xs);
      return ["acked", "unconfirmed"].includes(spawned?.state ?? "") && sent?.state === "queued";
    },
    45_000,
  );
  // The composer, a command, and a native dialog under the same CSP (W2-12).
  await page.getByLabel("Message", { exact: true }).fill("CSP の送信 🎌");
  await page.getByRole("form", { name: "Composer" }).getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.locator(`[data-slot="send:${thread}"]`)).toHaveAttribute("data-phase", "settled");
  await page.getByRole("region", { name: "Thread actions" }).getByRole("button", { name: "Stop" }).click();
  const dialog = page.getByRole("dialog", { name: "Stop this launch?" });
  await expect(dialog).toBeVisible();
  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(dialog).toBeHidden();
  expect(await violations(page)).toEqual([]);
});

test("csp_enforced", async ({ daemon, page, browserName }) => {
  await recordViolations(page);
  await openLink(page, await daemon.pairUrl());
  await expect(statusOf(page)).toHaveText("live");
  if (browserName === "chromium") {
    // Trusted Types with no policy: a string to an HTML sink throws.
    const r = await page.evaluate(() => {
      try {
        document.body.insertAdjacentHTML("beforeend", "<b id='injected'>x</b>");
        return "assigned";
      } catch (e) {
        return e instanceof TypeError ? "TypeError" : String(e);
      }
    });
    expect(r).toBe("TypeError");
    const inner = await page.evaluate(() => {
      try {
        document.body.innerHTML = "<b id='injected'>x</b>";
        return "assigned";
      } catch (e) {
        return e instanceof TypeError ? "TypeError" : String(e);
      }
    });
    expect(inner).toBe("TypeError");
    await expect(page.locator("#injected")).toHaveCount(0);
  }
  // An inline script does not run, and the browser reports it.
  const ran = await page.evaluate(async () => {
    const s = document.createElement("script");
    s.appendChild(document.createTextNode("window.__inlineRan = true;"));
    document.head.appendChild(s);
    await new Promise((ok) => setTimeout(ok, 200));
    return window.__inlineRan === true;
  });
  expect(ran, "an inline script ran").toBe(false);
  await expect.poll(async () => (await violations(page))?.length ?? 0).toBeGreaterThan(0);
  // The page still works.
  await expect(statusOf(page)).toHaveText("live");
});
