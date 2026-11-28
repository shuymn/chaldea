// The read path against the daemon: generic rows (W1-8), the restart (W1-3, W1-12), and the
// 390 × 844 layout (W1-4, and W2-12 for the composer, inputs, banner, and dialogs).

import { join } from "node:path";

import type { Page } from "@playwright/test";

import { expect, openLink, selectThread, shownSeqs, statusOf, test, untilShown } from "./harness";

test("generic_rows", async ({ daemon, page }) => {
  const { thread } = await daemon.spawnThread({ prompt: "typed neighbour" });
  const events = await daemon.waitEvents(thread, (es) => es.some((e) => e.kind === "user_message"));
  const seqOf = (kind: string) => {
    const e = events.find((x) => x.kind === kind);
    if (!e) throw new Error(`no ${kind}`);
    return e.seq;
  };
  // Each generic case of TypedEvent::decode on a real envelope (crates/core/tests/golden/decode).
  const unknownKind = seqOf("launch_started");
  const newerSchema = seqOf("session_bound");
  const badPayload = seqOf("readiness_observed");
  const typed = [seqOf("launch_reserved"), seqOf("user_message")];
  const author = (e: Record<string, unknown>) => {
    if (e.seq === unknownKind) return { ...e, kind: "artifact_diff", payload: { files: [1, 2, 3] } };
    if (e.seq === newerSchema) return { ...e, schema_version: 999 };
    if (e.seq === badPayload) return { ...e, payload: { evidence: 42 } };
    return e;
  };
  await page.route(
    (u) => u.pathname === "/api/events" && u.searchParams.get("stream_id") === `thread:${thread}`,
    async (route) => {
      const res = await route.fetch();
      const body = (await res.json()) as { mode: string; events?: Record<string, unknown>[] };
      if (body.mode === "replay" && body.events) body.events = body.events.map(author);
      await route.fulfill({ response: res, json: body });
    },
  );
  await openLink(page, await daemon.pairUrl());
  await page.locator(`li[data-thread="${thread}"] button`).first().click();
  await untilShown(page, events.at(-1)!.seq);
  const row = (seq: number) => page.locator("main ol > li").filter({ has: page.getByText(`#${seq}`, { exact: true }) });
  for (const [seq, kind] of [
    [unknownKind, "artifact_diff"],
    [newerSchema, "session_bound"],
    [badPayload, "readiness_observed"],
  ] as const) {
    await expect(row(seq).locator("span.font-semibold"), `#${seq}`).toHaveText("unknown event");
    await expect(row(seq).locator("p").nth(1), `#${seq}`).toHaveText(kind);
  }
  await expect(row(typed[0]!).locator("span.font-semibold")).toHaveText("launch reserved");
  await expect(row(typed[1]!).locator("span.font-semibold")).toHaveText("user");
  await expect(page.locator("main ol > li")).toHaveCount(events.length);
  // The page kept the stream: no failure, no retry.
  await expect(page.getByRole("button", { name: "Retry" })).toHaveCount(0);
  await expect(statusOf(page)).toHaveText("live");
});

test("offline_then_catch_up", async ({ daemon, page }) => {
  test.setTimeout(120_000);
  const { thread } = await daemon.spawnThread({ prompt: "first" });
  await daemon.waitEvents(thread, (es) => es.some((e) => e.kind === "user_message"));
  // More events than one history page (100), so "Older" has something to load.
  for (let i = 1; i <= 110; i++) await daemon.chaldea(["send", thread, `queued ${i}`]);
  // The launch ends before the daemon stops, so the stop itself commits nothing.
  await daemon.chaldea(["stop", thread]);
  const before = await daemon.waitEvents(thread, (es) => es.some((e) => e.kind === "launch_death_confirmed"));
  const head = before.at(-1)!.seq;

  // The socket passes through the test. Once `holding` is set, every typed message other
  // than `hello` (`heads`, `stream_advanced`) is dropped; frames that do not parse or carry
  // no type pass unchanged. After the restart the page learns of new events only from the
  // refetch its socket open schedules.
  let holding = false;
  let droppedHeads = 0;
  await page.routeWebSocket(/\/api\/ws(\?|$)/, (ws) => {
    const server = ws.connectToServer();
    ws.onMessage((m) => server.send(m));
    server.onMessage((m) => {
      if (holding) {
        // Only frames read as JSON with a type are filtered; anything else passes unchanged.
        let type: unknown;
        try {
          type = typeof m === "string" ? (JSON.parse(m) as { type?: unknown }).type : undefined;
        } catch {
          type = undefined;
        }
        if (typeof type === "string" && type !== "hello") {
          if (type === "heads") droppedHeads += 1;
          return;
        }
      }
      ws.send(m);
    });
  });

  await openLink(page, await daemon.pairUrl());
  await page.locator(`li[data-thread="${thread}"] button`).first().click();
  await untilShown(page, head);
  await expect(statusOf(page)).toHaveText("live");
  const shown = await shownSeqs(page);
  const lo = shown[0]!;
  expect(lo).toBeGreaterThan(1);
  expect(shown).toEqual(range(lo, head));
  const synced = await page.getByText(/^last synced /).textContent();

  // A stop seen from a quiescent page: the socket closes, the ticket POST fails in
  // transport, and the page shows offline with its content and last-synced time.
  await daemon.stop();
  await expect(statusOf(page)).toHaveText("offline", { timeout: 20_000 });
  await expect(page.getByRole("alert")).toBeVisible();
  await expect(page.getByText(/^last synced /)).toHaveText(synced!);
  expect(await shownSeqs(page)).toEqual(shown);

  // The page cannot reach the daemon again until the witness is committed and settled.
  holding = true;
  await page.route("**/api/**", (route) => route.abort());
  await daemon.start();
  const { launch_id: resumed } = (await daemon.chaldea(["resume", thread])) as { launch_id: string };
  await daemon.waitEvents(thread, (es) =>
    es.some((e) => e.kind === "readiness_observed" && e.launch_id === resumed),
  );
  // The durable range through the resumed launch: no new event for a second.
  let durable = await daemon.events(thread);
  for (let settled = false, tries = 0; !settled; tries++) {
    if (tries === 20) throw new Error("the thread kept growing after the resume");
    await page.waitForTimeout(1_000);
    const again = await daemon.events(thread);
    settled = again.length === durable.length;
    durable = again;
  }
  const last = durable.at(-1)!.seq;
  const witness = durable.find((e) => e.kind === "launch_started" && e.launch_id === resumed)!.seq;
  expect(witness).toBeGreaterThan(head);
  await expect(statusOf(page)).toHaveText("offline");
  await page.unroute("**/api/**");

  // Catch-up through the reconnect's refetch: the rows are exactly the durable range from
  // the window's first row through the resumed launch, each once and in order.
  await expect
    .poll(() => shownSeqs(page), { message: "rows after catch-up", timeout: 45_000 })
    .toEqual(range(lo, last));
  await expect(statusOf(page)).toHaveText("live", { timeout: 45_000 });
  await expect(page.getByText(/^last synced /)).not.toHaveText(synced!);
  // The reconnected socket's subscribe was answered with heads, which the page never saw.
  await expect.poll(() => droppedHeads, { message: "no heads frame was dropped", timeout: 15_000 }).toBeGreaterThan(0);

  // History before the window still loads, one page of up to 100 events per "Older". How
  // many events the sends produced depends on how fast FakeCli took them, so the number of
  // pages does too.
  const firstRow = page.locator("main ol > li").first().locator("span").first();
  for (let first = lo; first > 1; ) {
    await page.getByRole("button", { name: "Older", exact: true }).click();
    first = Math.max(1, first - 100);
    await expect(firstRow).toHaveText(`#${first}`, { timeout: 20_000 });
  }
  expect(await shownSeqs(page)).toEqual(range(1, last));
});

/** `[from, from + 1, …, to]`. */
function range(from: number, to: number): number[] {
  return Array.from({ length: to - from + 1 }, (_, i) => from + i);
}

test("layout_390", async ({ daemon, page }) => {
  test.setTimeout(120_000);
  // Long unbroken text in every field the list and the timeline show, and a closed row.
  const cwd = join(daemon.root, `a-directory-name-without-any-break-${"x".repeat(120)}`);
  const { thread } = await daemon.spawnThread({ cwd, prompt: `unbroken-${"y".repeat(300)}` });
  await daemon.waitEvents(thread, (es) => es.some((e) => e.kind === "user_message"));
  const { thread: closed } = await daemon.spawnThread({ cwd: join(daemon.root, `closed-${"z".repeat(120)}`) });
  await daemon.chaldea(["stop", closed]);
  await daemon.chaldea(["close", closed]);
  await openLink(page, await daemon.pairUrl());
  await expect(page.locator(`li[data-thread="${closed}"]`).getByText("closed", { exact: true })).toBeVisible();
  await page.locator(`li[data-thread="${thread}"] button`).first().click();
  await expect(page.locator("main ol").getByText(/^unbroken-/).first()).toBeVisible();
  await expect(statusOf(page)).toHaveText("live");

  const overflow = await page.evaluate(() => ({
    doc: document.documentElement.scrollWidth - document.documentElement.clientWidth,
    body: document.body.scrollWidth - document.body.clientWidth,
  }));
  expect(overflow, "horizontal scroll").toEqual({ doc: 0, body: 0 });

  // Every field of the list rows and of the timeline, present.
  const row = page.locator(`li[data-thread="${thread}"]`);
  await expect(row.getByText("claude", { exact: true })).toHaveCount(1);
  await expect(row.getByText(cwd)).toHaveCount(1);
  await expect(row.getByText(/ · task /)).toHaveCount(1);
  for (const name of ["Older", "Newer", "Jump to latest"]) {
    await expect(page.getByRole("button", { name, exact: true })).toHaveCount(1);
  }
  const rows = page.locator("main ol > li");
  const n = await rows.count();
  expect(n).toBeGreaterThan(0);
  for (const part of ["p:first-child > span:first-child", "time", "span.font-semibold"]) {
    await expect(rows.locator(part), part).toHaveCount(n);
  }

  // Each of them, and each of their parts, is rendered visible (not `visibility: hidden`,
  // not transparent), lies inside the viewport, and shows its whole content: no box past
  // either edge, no block narrower than what it holds.
  expect(await clipped(page, "[data-status], li[data-thread], #timeline-heading, [role='group'], main ol > li")).toEqual([]);

  // W2-12: the composer, the inputs, the attention banner, and both dialogs fit too. An
  // input goes `unconfirmed` (two lines, `sent`, then a stop), and a resumed launch makes
  // Stop and Mark ready available.
  const { thread: busy } = await daemon.spawnThread({ cwd: join(daemon.root, `busy-${"w".repeat(120)}`) });
  await daemon.waitEvents(busy, (es) => es.some((e) => e.kind === "readiness_observed"));
  await daemon.chaldea(["send", busy, `first-line-${"v".repeat(200)}\nsecond line`]);
  await daemon.waitInputs(busy, (is) => is[0]?.state === "sent", 25_000);
  await daemon.chaldea(["stop", busy]);
  await daemon.waitInputs(busy, (is) => is[0]?.state === "unconfirmed");
  const { launch_id: resumed } = (await daemon.chaldea(["resume", busy])) as { launch_id: string };
  await daemon.waitEvents(busy, (es) => es.some((e) => e.kind === "readiness_observed" && e.launch_id === resumed));
  await selectThread(page, busy);
  await expect(page.getByRole("region", { name: "Attention" })).toBeVisible();
  await page.getByLabel("Message", { exact: true }).fill(`unbroken-draft-${"u".repeat(300)}`);
  const actions = page.getByRole("region", { name: "Thread actions" });
  await expect(actions.getByRole("button", { name: "Mark ready" })).toBeEnabled({ timeout: 20_000 });
  const parts = "section[aria-label='Attention'], section[aria-label='Thread actions'], section[aria-labelledby='inputs-heading'], form[aria-label='Composer']";
  expect(await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)).toBe(0);
  expect(await clipped(page, parts)).toEqual([]);
  for (const [button, name] of [
    ["Mark ready", "Mark the CLI ready?"],
    ["Stop", "Stop this launch?"],
  ] as const) {
    await actions.getByRole("button", { name: button }).click();
    const dialog = page.getByRole("dialog", { name });
    await expect(dialog).toBeVisible();
    expect(await clipped(page, "dialog[open]")).toEqual([]);
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(dialog).toBeHidden();
  }
});

/**
 * Every element under `selector` that is not rendered visible (no box, `visibility:
 * hidden`, transparent), lies outside the viewport, or is narrower than what it holds.
 */
function clipped(page: Page, selector: string): Promise<string[]> {
  return page.evaluate((selector) => {
    // Both spellings of the options: the current ones and the older ones WebKit may read.
    const visible = { visibilityProperty: true, opacityProperty: true, checkVisibilityCSS: true, checkOpacity: true };
    const bad: string[] = [];
    for (const root of document.querySelectorAll(selector)) {
      for (const el of [root, ...root.querySelectorAll("*")]) {
        // Options and closed dialogs have no box of their own.
        if (el.tagName === "OPTION" || el.closest("dialog:not([open])")) continue;
        const r = el.getBoundingClientRect();
        const what = `${el.tagName.toLowerCase()} "${(el.textContent ?? "").slice(0, 30)}"`;
        if (r.width === 0 || r.height === 0) bad.push(`${what}: no box`);
        else if (!el.checkVisibility(visible)) bad.push(`${what}: not visible`);
        else if (r.left < 0 || r.right > innerWidth + 0.5) bad.push(`${what}: ${r.left}..${r.right} of ${innerWidth}`);
        else if (getComputedStyle(el).display !== "inline" && el.scrollWidth > el.clientWidth + 1) {
          bad.push(`${what}: ${el.scrollWidth} wide in ${el.clientWidth}`);
        }
      }
    }
    return bad;
  }, selector);
}
