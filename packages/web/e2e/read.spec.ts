// The read path against the daemon: generic rows (W1-8), the restart (W1-3, W1-12), and the
// 390 × 844 layout (W1-4).

import { mkdirSync } from "node:fs";
import { join } from "node:path";

import { expect, openLink, shownSeqs, statusOf, test, untilShown } from "./harness";

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

  await openLink(page, await daemon.pairUrl());
  await page.locator(`li[data-thread="${thread}"] button`).first().click();
  await untilShown(page, head);
  await expect(statusOf(page)).toHaveText("live");
  const shown = await shownSeqs(page);
  expect(shown.length).toBeGreaterThan(0);
  expect(Math.min(...shown)).toBeGreaterThan(1);
  const synced = await page.getByText(/^last synced /).textContent();

  // A stop seen from a quiescent page: the socket closes, the ticket POST fails in
  // transport, and the page shows offline with its content and last-synced time.
  await daemon.stop();
  await expect(statusOf(page)).toHaveText("offline", { timeout: 20_000 });
  await expect(page.getByRole("alert")).toBeVisible();
  await expect(page.getByText(/^last synced /)).toHaveText(synced!);
  expect(await shownSeqs(page)).toEqual(shown);

  // The page cannot reach the daemon again until the witness is committed.
  await page.route("**/api/**", (route) => route.abort());
  await daemon.start();
  const { launch_id: resumed } = (await daemon.chaldea(["resume", thread])) as { launch_id: string };
  const after = await daemon.waitEvents(thread, (es) =>
    es.some((e) => e.kind === "launch_started" && e.launch_id === resumed),
  );
  const witness = after.find((e) => e.kind === "launch_started" && e.launch_id === resumed)!.seq;
  expect(witness).toBeGreaterThan(head);
  await expect(statusOf(page)).toHaveText("offline");
  await page.unrouteAll({ behavior: "wait" });

  // Catch-up: the witness appears, every row shown before appears exactly once, in order.
  await expect(page.locator("main ol > li").filter({ has: page.getByText(`#${witness}`, { exact: true }) })).toHaveCount(1, {
    timeout: 45_000,
  });
  await expect(statusOf(page)).toHaveText("live", { timeout: 45_000 });
  await expect(page.getByText(/^last synced /)).not.toHaveText(synced!);
  const now = await shownSeqs(page);
  for (const s of shown) expect(now.filter((x) => x === s), `#${s}`).toHaveLength(1);
  for (let i = 1; i < now.length; i++) expect(now[i]!, "rows in seq order").toBeGreaterThan(now[i - 1]!);

  // History before the window still loads.
  await page.getByRole("button", { name: "Older", exact: true }).click();
  await expect(page.locator("main ol > li").first().locator("span").first()).toHaveText("#1", { timeout: 20_000 });
});

test("layout_390", async ({ daemon, page }) => {
  // Long unbroken text in every field the list and the timeline show.
  const cwd = join(daemon.root, `a-directory-name-without-any-break-${"x".repeat(120)}`);
  mkdirSync(cwd);
  const { thread } = await daemon.spawnThread({ cwd, prompt: `unbroken-${"y".repeat(300)}` });
  await daemon.waitEvents(thread, (es) => es.some((e) => e.kind === "user_message"));
  await openLink(page, await daemon.pairUrl());
  await page.locator(`li[data-thread="${thread}"] button`).first().click();
  await expect(page.locator("main ol").getByText(/^unbroken-/).first()).toBeVisible();
  await expect(statusOf(page)).toHaveText("live");

  const width = page.viewportSize()!.width;
  const overflow = await page.evaluate(() => ({
    doc: document.documentElement.scrollWidth - document.documentElement.clientWidth,
    body: document.body.scrollWidth - document.body.clientWidth,
  }));
  expect(overflow, "horizontal scroll").toEqual({ doc: 0, body: 0 });

  const row = page.locator(`li[data-thread="${thread}"]`);
  const fields = [
    page.locator("[data-status]"),
    page.getByText(/^last synced /),
    row.getByText("claude", { exact: true }),
    row.getByText(cwd),
    row.getByText(/ · task /),
    page.getByRole("heading", { name: "Timeline" }).or(page.locator("#timeline-heading")),
    page.getByRole("button", { name: "Older", exact: true }),
    page.getByRole("button", { name: "Newer", exact: true }),
    page.getByRole("button", { name: "Jump to latest" }),
    page.locator("main ol > li").first(),
    page.locator("main ol").getByText(/^unbroken-/).first(),
  ];
  for (const f of fields) {
    await f.scrollIntoViewIfNeeded();
    await expect(f).toBeVisible();
    const box = await f.boundingBox();
    expect(box, "a field without a box").not.toBeNull();
    expect(box!.x, "a field starts left of the viewport").toBeGreaterThanOrEqual(0);
    expect(box!.x + box!.width, "a field ends right of the viewport").toBeLessThanOrEqual(width);
  }
});
