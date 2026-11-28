// The end-to-end fixtures (docs/design/w1-pwa.md §3 D8): each test gets its own daemon
// (e2e/daemon.ts) under a scratch root in the OS temp directory, removed on every exit path.
// Pair codes and tokens never reach a test title, step, message, or attachment: assertions
// on them report booleans, and diagnostics pass through `redact`.

import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { test as base, expect, type Page, type WebSocketRoute } from "@playwright/test";

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

/** Pairs `page` and selects `thread`; resolves once the page is live and the thread caught up. */
export async function openThread(page: Page, daemon: Daemon, thread: string): Promise<void> {
  await openLink(page, await daemon.pairUrl());
  await selectThread(page, thread);
  await expect(statusOf(page)).toHaveText("live");
}

/** Selects `thread` in the list and waits until its composer is mounted and caught up. */
export async function selectThread(page: Page, thread: string): Promise<void> {
  await page.locator(`li[data-thread="${thread}"] button`).first().click();
  await caughtUp(page, thread);
}

/** The composer's hint when sending is not locked (Composer.tsx). */
const READY_HINT = "Enter sends on a desktop pointer";

/**
 * `thread` is the selected row, its timeline's last row is at least `#<seq>`, and its mounted
 * composer shows the ready hint (the page is live with a current copy of the thread, no lock
 * hint), read in one check: the state a send, ready, or input command needs at the gesture.
 * A gesture made earlier is refused with a notice, and typing into a just-mounted composer
 * can be lost to WebKit's fill.
 */
export async function caughtUp(page: Page, thread: string, seq = 0): Promise<void> {
  await expect
    .poll(
      () =>
        page.evaluate(
          ({ thread, seq, hint }) => {
            const row = document.querySelector(`li[data-thread="${thread}"] button`);
            const seqs = document.querySelectorAll("main ol > li p:first-child > span:first-child");
            const last = Number(seqs[seqs.length - 1]?.textContent?.replace(/^#/, "") ?? 0);
            const box = document.querySelector('form[aria-label="Composer"] textarea');
            // The textarea's own hint: the first element its `aria-describedby` names.
            const hintId = box?.getAttribute("aria-describedby")?.split(" ")[0] ?? "";
            const shown = hintId === "" ? "" : (document.getElementById(hintId)?.textContent ?? "");
            return row?.getAttribute("aria-current") === "true" && last >= seq && shown.startsWith(hint);
          },
          { thread, seq, hint: READY_HINT },
        ),
      { timeout: 20_000, message: `the thread's composer never became ready through #${seq}` },
    )
    .toBe(true);
}

/**
 * `caughtUp` after a change made outside the page (a CLI command, a daemon wait), through the
 * daemon's last event of the thread: the ready hint alone can show while an announce of those
 * events is still on its way, and one that lands between Playwright's check and the click
 * disables the control under it. `settled`, when given, is the change's last write, waited
 * for first; the guarantee is through the head sampled then.
 */
export async function caughtUpWith(
  page: Page,
  daemon: Daemon,
  thread: string,
  settled?: Parameters<Daemon["waitEvents"]>[1],
  ms?: number,
): Promise<void> {
  const es = settled ? await daemon.waitEvents(thread, settled, ms) : await daemon.events(thread);
  const head = es.at(-1)?.seq;
  if (head === undefined) throw new Error("the thread has no events to catch up through");
  await caughtUp(page, thread, head);
}

/** The thread's inputs from its events, in creation order: id, launch, seq created, latest state. */
export function inputsOf(es: Awaited<ReturnType<Daemon["events"]>>) {
  const out = new Map<string, { id: string; launch: string | null; seq: number; state: string }>();
  for (const e of es) {
    const id = e.payload.input_id as string;
    if (e.kind === "input_created") {
      out.set(id, { id, launch: e.launch_id, seq: e.seq, state: e.payload.initial_state as string });
    } else if (e.kind === "input_state_changed" && out.has(id)) {
      out.set(id, { ...out.get(id)!, state: e.payload.to as string });
    }
  }
  return [...out.values()];
}

/**
 * The thread's `n`th input is a two-line prompt that is `sent`, and FakeCli's two per-line
 * prompt hooks of its launch (matching no input) are in: a stop now leaves neither in flight.
 */
export const linesHooked =
  (n: number) =>
  (es: Awaited<ReturnType<Daemon["events"]>>): boolean => {
    const i = inputsOf(es)[n];
    const hooks = es.filter(
      (e) => e.kind === "user_message" && e.payload.input_id === null && e.launch_id === i?.launch && e.seq > i.seq,
    );
    return i?.state === "sent" && hooks.length >= 2;
  };

/** Waits until the timeline's last row is at least `#<seq>`. */
export async function shownAtLeast(page: Page, seq: number): Promise<void> {
  await expect
    .poll(async () => (await shownSeqs(page)).at(-1) ?? 0, { timeout: 20_000, message: `timeline never reached #${seq}` })
    .toBeGreaterThanOrEqual(seq);
}

/**
 * How long a check for an absent request waits after a gesture that must not make one. Only
 * silence is being proven, so no condition exists to wait on: a request the page would start
 * synchronously from the gesture is reported to the test well within this.
 */
export const SILENCE_MS = 500;

/** Counts the composer form's `submit` events in the page (`window.__submits`). */
export async function countSubmits(page: Page): Promise<() => Promise<number>> {
  await page.evaluate(() => {
    const w = window as unknown as { __submits?: number };
    if (w.__submits !== undefined) return;
    w.__submits = 0;
    document.addEventListener(
      "submit",
      (e) => {
        if ((e.target as Element).matches('form[aria-label="Composer"]')) w.__submits = (w.__submits ?? 0) + 1;
      },
      true,
    );
  });
  return () => page.evaluate(() => (window as unknown as { __submits?: number }).__submits ?? 0);
}

/** A thread whose launch is ready (an open gate: no prompt has been written yet). */
export async function openGate(daemon: Daemon): Promise<string> {
  const { thread } = await daemon.spawnThread();
  await daemon.waitEvents(thread, (es) => es.some((e) => e.kind === "readiness_observed"));
  return thread;
}

/**
 * The page's events socket, passed through. `drop()` closes it so the page opens a new
 * one; `opened()` counts the sockets opened so far.
 */
export async function socketTap(page: Page): Promise<{ drop: () => void; opened: () => number }> {
  let current: WebSocketRoute | null = null;
  let n = 0;
  await page.routeWebSocket(/\/api\/ws(\?|$)/, (ws) => {
    const server = ws.connectToServer();
    current = ws;
    n += 1;
    ws.onMessage((m) => server.send(m));
    server.onMessage((m) => ws.send(m));
  });
  return {
    drop: () => {
      void current?.close();
      current = null;
    },
    opened: () => n,
  };
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
