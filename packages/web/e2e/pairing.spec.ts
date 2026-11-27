// Pairing, requests, and tabs (docs/design/w1-pwa.md §2.2; W1-10). Assertions on secrets
// compare inside the test and report only whether they matched, never the value.

import { readdirSync, rmSync } from "node:fs";
import { join } from "node:path";

import type { BrowserContext, Page, Request, Route } from "@playwright/test";

import { type Daemon, expect, openLink, statusOf, test } from "./harness";

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

/** A tab whose Authorization headers are recorded in request order. */
async function tab(context: BrowserContext): Promise<{ page: Page; auth: string[] }> {
  const page = await context.newPage();
  const auth: string[] = [];
  page.on("request", (r) => {
    const h = r.headers().authorization;
    if (h) auth.push(h);
  });
  return { page, auth };
}

/** Pairs `t` from a fresh link; the Authorization it then uses. */
async function pair(daemon: Daemon, t: { page: Page; auth: string[] }): Promise<string> {
  await openLink(t.page, await daemon.pairUrl());
  await expect(statusOf(t.page)).toHaveText("live");
  const h = t.auth.at(-1);
  if (h === undefined) throw new Error("the tab made no authorized request");
  return h;
}

test("two_tabs", async ({ daemon, context }) => {
  // Tab A receives the other tabs' credential notices only when the test releases them, so
  // its own 401 meets a newer stored generation first.
  const a = await tab(context);
  await a.page.addInitScript(() => {
    const Native = window.BroadcastChannel;
    const queue: (() => void)[] = [];
    let hold = true;
    (window as unknown as { __releaseNotices: () => void }).__releaseNotices = () => {
      hold = false;
      for (const f of queue.splice(0)) f();
    };
    window.BroadcastChannel = class extends Native {
      override set onmessage(h: ((this: BroadcastChannel, e: MessageEvent) => unknown) | null) {
        super.onmessage =
          h === null
            ? null
            : (e: MessageEvent) => {
                if (hold) queue.push(() => h.call(this, e));
                else h.call(this, e);
              };
      }
      override get onmessage() {
        return super.onmessage;
      }
    };
  });
  const tokenA = await pair(daemon, a);

  // Hold A's next host read, made with generation A.
  let held: Route | null = null;
  let heldSeen!: () => void;
  const heldNow = new Promise<void>((ok) => {
    heldSeen = ok;
  });
  await a.page.route(
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

  // Tab B pairs again: generation B is committed, and A has not heard of it.
  const b = await tab(context);
  const tokenB = await pair(daemon, b);
  expect(tokenB !== tokenA, "tab B holds a new token").toBe(true);
  expect(a.auth.includes(tokenB), "tab A heard of B before its 401").toBe(false);

  // The delayed 401 for generation A: its compare-and-delete finds B, deletes nothing,
  // publishes nothing, and A adopts B without a prompt.
  await (held as Route | null)?.fulfill({
    status: 401,
    contentType: "application/json",
    body: '{"code":"unauthorized"}',
  });
  await expect
    .poll(() => a.auth.includes(tokenB), { message: "tab A never adopted generation B", timeout: 15_000 })
    .toBe(true);
  await a.page.evaluate(() => (window as unknown as { __releaseNotices: () => void }).__releaseNotices());
  await a.page.waitForTimeout(1_000);
  for (const t of [a, b]) {
    await expect(t.page.getByText(SPENT)).toHaveCount(0);
    await expect(statusOf(t.page)).toHaveText("live");
  }

  // Tab C pairs again: A and B reread on its notice and adopt C, retiring B's work.
  const c = await tab(context);
  const tokenC = await pair(daemon, c);
  for (const [name, t] of [
    ["A", a],
    ["B", b],
  ] as const) {
    await expect
      .poll(() => t.auth.includes(tokenC), { message: `tab ${name} never adopted generation C`, timeout: 15_000 })
      .toBe(true);
    await expect(statusOf(t.page)).toHaveText("live");
  }

  // C's credential is stored: a reload stays paired and uses it.
  await b.page.reload();
  await expect(statusOf(b.page)).toHaveText("live");
  await expect(b.page.getByText(SPENT)).toHaveCount(0);
  expect(b.auth.at(-1) === tokenC, "tab B reloaded with a token other than C's").toBe(true);
});
