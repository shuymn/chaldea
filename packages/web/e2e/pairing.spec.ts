// Pairing, requests, and tabs (docs/design/w1-pwa.md §2.2; W1-10). Assertions on secrets
// compare inside the test and report only whether they matched, never the value.

import { readdirSync, rmSync } from "node:fs";
import { join } from "node:path";

import type { BrowserContext, Page, Request, Route } from "@playwright/test";

import { type Daemon, expect, hasFragment, openLink, statusOf, test } from "./harness";

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
  expect(await hasFragment(page), "fragment left after pairing").toBe(false);

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
    expect(await hasFragment(p), "fragment left on the spent link").toBe(false);
  } finally {
    await fresh.close();
  }
});

test("request_policy", async ({ daemon, page }) => {
  const { thread } = await daemon.spawnThread({ prompt: "policy check" });
  // Headers as sent: a finished request's full headers (WebKit gives them only once the
  // response arrived). A failed one (the page aborts a request it no longer needs) has only
  // the browser's provisional headers, where Chromium lists a suppressed referrer as an
  // empty `referer` for every request, assets included; a finished request's wire headers
  // never carry it.
  const sent: {
    method: string;
    url: string;
    provisional: boolean;
    headers: Promise<Record<string, string>>;
  }[] = [];
  const record = (r: Request, provisional: boolean, h: Promise<Record<string, string>>) =>
    sent.push({ method: r.method(), url: r.url(), provisional, headers: h });
  page.on("requestfinished", (r) => record(r, false, r.allHeaders()));
  page.on("requestfailed", (r) => record(r, true, Promise.resolve(r.headers())));
  const sockets: string[] = [];
  page.on("websocket", (ws) => sockets.push(ws.url()));
  /** The purpose of every ticket POST. */
  const tickets: unknown[] = [];
  page.on("request", (r) => {
    if (new URL(r.url()).pathname === "/api/ws-ticket") tickets.push((r.postDataJSON() as { purpose?: unknown }).purpose);
  });

  await openLink(page, await daemon.pairUrl());
  await page.locator(`li[data-thread="${thread}"] button`).first().click();
  await expect(page.locator("main ol").getByText("policy check").first()).toBeVisible();
  await expect(statusOf(page)).toHaveText("live");
  // Every command the page has, through its own controls (W2-12).
  test.setTimeout(150_000);
  const line = (slot: string) => page.locator(`[data-slot="${slot}"]`).first();
  const actions = page.getByRole("region", { name: "Thread actions" });
  const sendText = async (text: string) => {
    await page.getByLabel("Message", { exact: true }).fill(text);
    await page.getByRole("form", { name: "Composer" }).getByRole("button", { name: "Send", exact: true }).click();
    await expect(line(`send:${thread}`)).toHaveAttribute("data-phase", "settled");
  };
  const row = (id: string) => page.locator(`section[aria-labelledby="inputs-heading"] li[data-input="${id}"]`);
  await daemon.waitInputs(thread, (is) => is[0]?.state === "acked");
  // send, then input_cancel on the queued input (the gate is closed after the first turn).
  await sendText("a policy send");
  const [, queued] = await daemon.waitInputs(thread, (is) => is[1]?.state === "queued");
  await row(queued!.input_id).getByRole("button", { name: "Cancel" }).click();
  await daemon.waitInputs(thread, (is) => is[1]?.state === "cancelled");
  // ready, stop, resume.
  await actions.getByRole("button", { name: "Mark ready" }).click();
  await page.getByRole("dialog", { name: "Mark the CLI ready?" }).getByRole("button", { name: "Mark ready" }).click();
  await expect(line(`ready:${thread}`)).toHaveAttribute("data-phase", "settled");
  await actions.getByRole("button", { name: "Stop" }).click();
  await page.getByRole("dialog", { name: "Stop this launch?" }).getByRole("button", { name: "Stop" }).click();
  await expect(line(`launch:${thread}`)).toContainText("stopped", { timeout: 45_000 });
  await expect(actions.getByRole("button", { name: "Resume" })).toBeEnabled({ timeout: 20_000 });
  await actions.getByRole("button", { name: "Resume" }).click();
  await expect(line(`launch:${thread}`)).toContainText("Resume: outcome: started", { timeout: 45_000 });
  await daemon.waitEvents(thread, (es) => {
    const second = es.filter((e) => e.kind === "launch_started")[1]?.launch_id;
    return second != null && es.some((e) => e.kind === "readiness_observed" && e.launch_id === second);
  });
  // input_resolve on an unconfirmed input (two lines, `sent`, then a stop).
  await sendText("line one\nline two");
  const sentOne = await daemon.waitInputs(thread, (is) => is[2]?.state === "sent", 25_000);
  await daemon.chaldea(["stop", thread]);
  await daemon.waitInputs(thread, (is) => is[2]?.state === "unconfirmed");
  await row(sentOne[2]!.input_id).getByRole("button", { name: "Dismiss" }).click();
  await daemon.waitInputs(thread, (is) => is[2]?.state === "dismissed");
  // spawn from the form.
  await page.getByLabel("Working directory (absolute path)").fill(join(daemon.root));
  await page.getByRole("button", { name: "Start thread" }).click();
  await expect(line("spawn")).toHaveAttribute("data-phase", "settled", { timeout: 45_000 });
  const commandPaths = new Set(sent.map((r) => new URL(r.url).pathname).filter((p) => p.startsWith("/api/commands/")));
  expect([...commandPaths].sort()).toEqual(
    ["input_cancel", "input_resolve", "ready", "resume", "send", "spawn", "stop"].map((c) => `/api/commands/${c}`),
  );

  // Every mutation is one the page is allowed; nothing reads the screen, attaches, or writes keys.
  const MUTATIONS = new Set([
    "/api/pair",
    "/api/ws-ticket",
    ...["send", "ready", "input_resolve", "input_cancel", "spawn", "stop", "resume"].map((c) => `/api/commands/${c}`),
  ]);
  for (const r of sent) {
    const path = new URL(r.url).pathname;
    expect(path.endsWith("/screen") || path.startsWith("/api/attach") || path === "/api/commands/key", path).toBe(false);
    if (r.method !== "GET") expect(MUTATIONS.has(path), `${r.method} ${path}`).toBe(true);
    if (r.method === "GET" && path.startsWith("/api/")) expect(path, `GET ${path}`).toBe("/api/events");
  }
  expect(tickets.length).toBeGreaterThan(0);
  expect(tickets.every((t) => t === "events"), "a ticket of another purpose").toBe(true);

  const tokens = new Set<string>();
  let posts = 0;
  for (const r of sent) {
    const u = new URL(r.url);
    expect(u.origin, `a request left the page's origin (${u.pathname})`).toBe(daemon.origin);
    const h = await r.headers;
    if (r.provisional) expect(h.referer ?? "", `Referer on aborted ${r.method} ${u.pathname}`).toBe("");
    else expect(h.referer, `Referer on ${r.method} ${u.pathname}`).toBeUndefined();
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
  // The record is gone from IndexedDB, not only from the page's memory.
  expect(await storedCredential(page), "a credential is still stored").toBe(false);

  // A fresh link repairs the pairing, and it survives a reload.
  await daemon.spawnThread();
  await openLink(page, await daemon.pairUrl());
  await expect(statusOf(page)).toHaveText("live");
  await expect(page.locator("li[data-thread]")).toHaveCount(1);
  expect(await storedCredential(page), "no credential stored after pairing").toBe(true);
  await page.reload();
  await expect(statusOf(page)).toHaveText("live");
  await expect(page.getByText(SPENT)).toHaveCount(0);
  await expect(page.locator("li[data-thread]")).toHaveCount(1);
});

/** Whether the page's origin holds a credential record (`chaldea/credential`, key `current`). */
function storedCredential(page: Page): Promise<boolean> {
  return page.evaluate(
    () =>
      new Promise<boolean>((ok, err) => {
        const open = indexedDB.open("chaldea", 1);
        open.onupgradeneeded = () => open.result.createObjectStore("credential");
        open.onerror = () => err(new Error("IndexedDB did not open"));
        open.onsuccess = () => {
          const db = open.result;
          const get = db.transaction("credential", "readonly").objectStore("credential").get("current");
          get.onsuccess = () => {
            db.close();
            ok(get.result !== undefined);
          };
          get.onerror = () => {
            db.close();
            err(new Error("IndexedDB read failed"));
          };
        };
      }),
  );
}

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

/**
 * Holds `page`'s host reads made with a given Authorization: `next(auth)` resolves with the
 * first such request's route, held until the test answers it; every other read goes on.
 */
async function holdHostReads(page: Page): Promise<{ next: (auth: string) => Promise<Route> }> {
  const waiting: { auth: string; take: (r: Route) => void }[] = [];
  await page.route(
    (u) => u.pathname === "/api/events" && u.searchParams.get("stream_id") === "host",
    async (route) => {
      const i = waiting.findIndex((w) => w.auth === route.request().headers().authorization);
      if (i >= 0) waiting.splice(i, 1)[0]!.take(route);
      else await route.continue();
    },
  );
  return { next: (auth) => new Promise<Route>((take) => waiting.push({ auth, take })) };
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
  const hold = await holdHostReads(a.page);
  const heldA = hold.next(tokenA);
  await daemon.spawnThread();
  const routeA = await heldA;

  // Tab B pairs again: generation B is committed, and A has not heard of it.
  const b = await tab(context);
  const tokenB = await pair(daemon, b);
  expect(tokenB !== tokenA, "tab B holds a new token").toBe(true);
  expect(a.auth.includes(tokenB), "tab A heard of B before its 401").toBe(false);

  // The delayed 401 for generation A: its compare-and-delete finds B, deletes nothing,
  // publishes nothing, and A adopts B without a prompt.
  await routeA.fulfill({
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

  // Tab C pairs again: A and B reread on its notice and adopt C.
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
