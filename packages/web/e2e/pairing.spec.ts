// Pairing, requests, and tabs (docs/design/w1-pwa.md §2.2; W1-10). Assertions on secrets
// compare inside the test and report only whether they matched, never the value.

import { readdirSync, rmSync } from "node:fs";
import { join } from "node:path";

import type { Request, Route } from "@playwright/test";

import { expect, openLink, statusOf, test } from "./harness";

const SPENT = "This link is spent or expired";

test("pair_once", async ({ daemon, browser, page }) => {
  await daemon.spawnThread();
  const url = await daemon.pairUrl();
  let fragmentAtExchange: boolean | null = null;
  await page.route("**/api/pair", async (route) => {
    // The exchange is held here until the page's own URL has been read.
    fragmentAtExchange = await page.evaluate(() => location.hash.startsWith("#pair"));
    await route.continue();
  });
  await openLink(page, url);
  await expect(page.locator("li[data-thread]")).toHaveCount(1);
  await expect(statusOf(page)).toHaveText("live");
  expect(fragmentAtExchange, "the fragment was still in the URL when /api/pair was sent").toBe(false);
  expect(await page.evaluate(() => location.hash), "fragment left after pairing").toBe("");

  // A reload of the paired page reads the stored credential.
  await page.reload();
  await expect(page.locator("li[data-thread]")).toHaveCount(1);
  await expect(statusOf(page)).toHaveText("live");

  // The spent link in a fresh profile asks for a fresh link.
  const fresh = await browser.newContext();
  try {
    const p = await fresh.newPage();
    await openLink(p, url);
    await expect(p.getByText(SPENT)).toBeVisible();
    expect(await p.evaluate(() => location.hash)).toBe("");
  } finally {
    await fresh.close();
  }
});

test("request_policy", async ({ daemon, page }) => {
  const { thread } = await daemon.spawnThread({ prompt: "policy check" });
  // Headers as sent: a finished request's full headers (WebKit gives them only once the
  // response arrived), a failed one's provisional headers.
  const sent: { method: string; url: string; headers: Promise<Record<string, string>> }[] = [];
  const record = (r: Request, h: Promise<Record<string, string>>) =>
    sent.push({ method: r.method(), url: r.url(), headers: h });
  page.on("requestfinished", (r) => record(r, r.allHeaders()));
  page.on("requestfailed", (r) => record(r, Promise.resolve(r.headers())));
  const sockets: string[] = [];
  page.on("websocket", (ws) => sockets.push(ws.url()));

  await openLink(page, await daemon.pairUrl());
  await page.locator(`li[data-thread="${thread}"] button`).first().click();
  await expect(page.locator("main ol").getByText("policy check").first()).toBeVisible();
  await expect(statusOf(page)).toHaveText("live");

  const tokens = new Set<string>();
  let posts = 0;
  for (const r of sent) {
    const u = new URL(r.url);
    expect(u.origin, `a request left the page's origin (${u.pathname})`).toBe(daemon.origin);
    const h = await r.headers;
    expect(h.referer, `Referer on ${r.method} ${u.pathname}`).toBeUndefined();
    if (r.method === "POST") {
      posts += 1;
      expect(h.origin, `Origin on POST ${u.pathname}`).toBe(daemon.origin);
    }
    if (u.pathname === "/api/pair" || !u.pathname.startsWith("/api/")) {
      expect(h.authorization === undefined, `Authorization on ${u.pathname}`).toBe(true);
    } else {
      const m = /^Bearer (\S+)$/.exec(h.authorization ?? "");
      expect(m !== null, `no Bearer token on ${r.method} ${u.pathname}`).toBe(true);
      if (m?.[1]) tokens.add(m[1]);
    }
  }
  // The pair exchange, the ticket, and every read happened.
  expect(posts).toBeGreaterThanOrEqual(2);
  expect(tokens.size).toBe(1);
  const [token] = [...tokens];
  for (const r of sent) {
    expect(r.url.includes(token!), "the token is in a URL").toBe(false);
    for (const [k, v] of Object.entries(await r.headers)) {
      if (k !== "authorization") expect(v.includes(token!), `the token is in ${k}`).toBe(false);
    }
  }
  expect(sockets.length).toBeGreaterThanOrEqual(1);
  for (const s of sockets) {
    const u = new URL(s);
    expect(`${u.protocol}//${u.host}${u.pathname}`).toBe(`ws://127.0.0.1:${daemon.port}/api/ws`);
    expect([...u.searchParams.keys()]).toEqual(["ticket"]);
    expect(s.includes(token!), "the token is in the socket URL").toBe(false);
  }
});

test("new_database_repairs", async ({ daemon, page }) => {
  await daemon.spawnThread();
  await openLink(page, await daemon.pairUrl());
  await expect(statusOf(page)).toHaveText("live");
  await daemon.stop();
  for (const f of readdirSync(daemon.state)) {
    if (f.startsWith("chaldea.db")) rmSync(join(daemon.state, f));
  }
  await daemon.start();
  // The new database knows no token: the page's next request gets 401, which clears the
  // credential and asks for a fresh link.
  await expect(page.getByText(SPENT)).toBeVisible({ timeout: 45_000 });
});

test("two_tabs", async ({ daemon, context }) => {
  const tabA = await context.newPage();
  // A's Authorization headers, in the order its requests were made.
  const authA: string[] = [];
  tabA.on("request", (r) => {
    const h = r.headers().authorization;
    if (h) authA.push(h);
  });
  await openLink(tabA, await daemon.pairUrl());
  await expect(statusOf(tabA)).toHaveText("live");
  const tokenA = authA.at(-1);
  expect(tokenA !== undefined, "tab A made no authorized request").toBe(true);

  // Hold A's next host read, made with generation A.
  let held: Route | null = null;
  let heldSeen!: () => void;
  const heldNow = new Promise<void>((ok) => {
    heldSeen = ok;
  });
  await tabA.route(
    (u) => u.pathname === "/api/events" && u.searchParams.get("stream_id") === "host",
    async (route) => {
      if (held === null && route.request().headers().authorization === tokenA) {
        held = route;
        heldSeen();
      } else {
        await route.continue();
      }
    },
  );
  await daemon.spawnThread();
  await heldNow;

  // Tab B pairs again: generation B is committed.
  const tabB = await context.newPage();
  const authB: string[] = [];
  tabB.on("request", (r) => {
    const h = r.headers().authorization;
    if (h) authB.push(h);
  });
  await openLink(tabB, await daemon.pairUrl());
  await expect(statusOf(tabB)).toHaveText("live");
  const tokenB = authB.at(-1);
  expect(tokenB !== undefined && tokenB !== tokenA, "tab B holds a new token").toBe(true);

  // A rereads the store and adopts B: its new requests carry B's token.
  await expect
    .poll(() => authA.includes(tokenB!), { message: "tab A never used generation B", timeout: 15_000 })
    .toBe(true);
  await expect(statusOf(tabA)).toHaveText("live");

  // The delayed 401 for generation A: it deletes nothing and prompts nowhere.
  const route = held as Route | null;
  await route?.fulfill({ status: 401, contentType: "application/json", body: '{"code":"unauthorized"}' }).catch(() => {});
  await tabA.waitForTimeout(1_000);
  for (const t of [tabA, tabB]) {
    await expect(t.getByText(SPENT)).toHaveCount(0);
    await expect(statusOf(t)).toHaveText("live");
  }
  // B's credential is still stored: a reload of either tab stays paired.
  await tabB.reload();
  await expect(statusOf(tabB)).toHaveText("live");
  await expect(tabB.getByText(SPENT)).toHaveCount(0);
  const reloaded: string[] = [];
  tabA.on("request", (r) => {
    const h = r.headers().authorization;
    if (h) reloaded.push(h);
  });
  await tabA.unrouteAll({ behavior: "ignoreErrors" });
  await tabA.reload();
  await expect(statusOf(tabA)).toHaveText("live");
  expect(reloaded.every((h) => h === tokenB), "tab A reloaded with a token other than B's").toBe(true);
});
