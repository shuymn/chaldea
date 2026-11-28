// The commands against the daemon (docs/design/w2-composer.md §2.2, §2.4, §4): W2-5
// `send_outcome_unknown`, `send_not_done_retry`, `double_submit`, `retry_session`; W2-7 and
// W2-8 `unconfirmed_resolve`, `gate_timeout_cancel`; W2-10 `ready_opens_gate`,
// `spawn_stop_resume`, `spawn_failed`, `stop_dialog_pins`, `command_401`, and the
// unconfirmed-death hint.
//
// FakeCli `echo` emits `SessionStart`, then one `UserPromptSubmit` per input line, and never
// `Stop`: after one prompt the turn stays running and the gate closed (the closed-gate fixture).

import { mkdtempSync } from "node:fs";
import { createServer } from "node:http";
import { join } from "node:path";

import type { Page, Route } from "@playwright/test";

import {
  commandLine,
  commandPosts,
  composer,
  type Daemon,
  expect,
  openGate,
  openLink,
  openThread,
  selectThread,
  sendButton,
  socketTap,
  statusOf,
  test,
} from "./harness";

const SPENT = "This link is spent or expired";

/** The closed-gate fixture: a first prompt acked, the turn running, the gate closed. */
async function closedGate(daemon: Daemon): Promise<string> {
  const { thread } = await daemon.spawnThread({ prompt: "first turn" });
  await daemon.waitInputs(thread, (is) => is[0]?.state === "acked");
  return thread;
}

const actions = (page: Page) => page.getByRole("region", { name: "Thread actions" });
const banner = (page: Page) => page.getByRole("region", { name: "Attention" });
const inputRow = (page: Page, id: string) => page.locator(`section[aria-labelledby="inputs-heading"] li[data-input="${id}"]`);

/** Sends `text` from the composer and waits for the reply. */
async function send(page: Page, thread: string, text: string): Promise<void> {
  await composer(page).fill(text);
  await sendButton(page).click();
  await expect(commandLine(page, `send:${thread}`)).toHaveAttribute("data-phase", "settled");
}

/** Holds every request to `path` until the test answers it. */
async function hold(page: Page, path: string): Promise<{ next: () => Promise<Route> }> {
  const waiting: ((r: Route) => void)[] = [];
  const queued: Route[] = [];
  await page.route(`**${path}`, (route) => {
    const w = waiting.shift();
    if (w) w(route);
    else queued.push(route);
  });
  return {
    next: () => {
      const q = queued.shift();
      return q ? Promise.resolve(q) : new Promise<Route>((ok) => waiting.push(ok));
    },
  };
}

test("ready_opens_gate", async ({ daemon, page }) => {
  test.setTimeout(120_000);
  const thread = await closedGate(daemon);
  await openThread(page, daemon, thread);
  await send(page, thread, "second turn");
  const [, second] = await daemon.waitInputs(thread, (is) => is[1]?.state === "queued");
  await expect(inputRow(page, second!.input_id)).toHaveAttribute("data-state", "queued");
  const readies = commandPosts(page, "ready");

  await actions(page).getByRole("button", { name: "Mark ready" }).click();
  const dialog = page.getByRole("dialog", { name: "Mark the CLI ready?" });
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText("normal composer, open and empty, with no popup");
  await expect(dialog).toContainText("Queued prompts may then be written");
  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(dialog).toBeHidden();
  await page.waitForTimeout(300);
  expect(readies).toEqual([]);

  await actions(page).getByRole("button", { name: "Mark ready" }).click();
  await dialog.getByRole("button", { name: "Mark ready" }).click();
  await expect(commandLine(page, `ready:${thread}`)).toHaveAttribute("data-phase", "settled");
  expect(readies).toHaveLength(1);
  const es = await daemon.waitEvents(thread, (xs) =>
    xs.some((e) => e.kind === "readiness_observed" && e.payload.evidence === "owner_asserted"),
  );
  expect(es.some((e) => e.kind === "readiness_observed" && e.payload.by === "human")).toBe(true);
  await daemon.waitInputs(thread, (is) => is[1]?.state === "acked");
  await expect(inputRow(page, second!.input_id)).toHaveAttribute("data-state", "acked");
});

test("gate_timeout_cancel", async ({ daemon, page }, info) => {
  test.skip(!["chromium-desktop", "webkit-390"].includes(info.project.name), "60 s gate timeout: two projects only");
  test.setTimeout(150_000);
  const thread = await closedGate(daemon);
  await openThread(page, daemon, thread);
  await send(page, thread, "waits for readiness");
  const [, second] = await daemon.waitInputs(thread, (is) => is[1]?.state === "queued");
  const entry = banner(page).locator(`[data-cause="input_gate_timeout"]`);
  await expect(entry).toBeVisible({ timeout: 90_000 });
  await expect(entry.getByRole("button", { name: "Mark ready" })).toBeVisible();
  await entry.getByRole("button", { name: "Cancel" }).click();
  await daemon.waitInputs(thread, (is) => is[1]?.state === "cancelled");
  await expect(banner(page)).toHaveCount(0);
  await expect(inputRow(page, second!.input_id)).toHaveAttribute("data-state", "cancelled");
  await expect(inputRow(page, second!.input_id).getByRole("button")).toHaveCount(0);
});

/** An input that went `unconfirmed`: two lines (whose per-line hooks match no input), `sent`, then a CLI stop. */
async function unconfirmed(daemon: Daemon, page: Page, thread: string): Promise<string> {
  await send(page, thread, "line one\nline two");
  const [i] = await daemon.waitInputs(thread, (is) => is[0]?.state === "sent", 25_000);
  await daemon.chaldea(["stop", thread]);
  await daemon.waitInputs(thread, (is) => is[0]?.state === "unconfirmed");
  return i!.input_id;
}

test("unconfirmed_resolve", async ({ daemon, page }) => {
  test.setTimeout(120_000);
  const thread = await openGate(daemon);
  await openThread(page, daemon, thread);
  const id = await unconfirmed(daemon, page, thread);
  const entry = banner(page).locator(`[data-cause="input_unconfirmed"]`);
  await expect(entry).toBeVisible();
  await expect(entry.getByRole("button")).toHaveText(["It arrived", "Dismiss"]);
  await expect(inputRow(page, id)).toHaveAttribute("data-state", "unconfirmed");
  await expect(inputRow(page, id).getByRole("button", { name: "Cancel" })).toHaveCount(0);
  await entry.getByRole("button", { name: "Dismiss" }).click();
  await daemon.waitInputs(thread, (is) => is[0]?.state === "dismissed");
  await expect(banner(page)).toHaveCount(0);
  await expect(inputRow(page, id)).toHaveAttribute("data-state", "dismissed");
  await expect(inputRow(page, id)).toContainText("by human");
});

test("send_outcome_unknown", async ({ daemon, page }) => {
  const thread = await openGate(daemon);
  await page.route("**/api/commands/send", async (route) => {
    // Committed, its reply lost.
    await route.fetch();
    await route.abort();
  });
  await openThread(page, daemon, thread);
  const posts = commandPosts(page, "send");
  await composer(page).fill("did it arrive?");
  await sendButton(page).click();
  const line = commandLine(page, `send:${thread}`);
  await expect(line).toHaveAttribute("data-phase", "check_thread");
  await expect(line).toContainText("Check the thread");
  await expect(line.getByRole("button", { name: "Retry" })).toHaveCount(0);
  await expect(sendButton(page)).toBeDisabled();
  await page.waitForTimeout(1500);
  await expect(line.getByRole("button", { name: "Retry" })).toHaveCount(0);
  expect(posts).toHaveLength(1);
  expect(await daemon.waitInputs(thread, (is) => is.length === 1)).toHaveLength(1);
  await line.getByRole("button", { name: "Forget" }).click();
  await expect(line).toContainText("Sending again is a new command");
  await expect(composer(page)).toHaveValue("did it arrive?");
  await expect(sendButton(page)).toBeEnabled();
});

test("send_not_done_retry", async ({ daemon, page }) => {
  const thread = await openGate(daemon);
  let first = true;
  await page.route("**/api/commands/send", async (route) => {
    if (first) {
      first = false;
      await route.fulfill({ status: 503, contentType: "application/json", body: '{"code":"busy","retry_after":1}' });
    } else {
      await route.continue();
    }
  });
  await openThread(page, daemon, thread);
  const posts = commandPosts(page, "send");
  await composer(page).fill("retried 再送");
  await sendButton(page).click();
  const line = commandLine(page, `send:${thread}`);
  await expect(line).toHaveAttribute("data-phase", "not_done");
  await expect(line).toContainText("the daemon is busy");
  const retry = line.getByRole("button", { name: "Retry" });
  await expect(retry).toBeVisible({ timeout: 5_000 });
  await retry.click();
  await expect(line).toHaveAttribute("data-phase", "settled");
  expect(posts).toHaveLength(2);
  expect(posts[1] === posts[0], "the Retry's body differs from the first attempt's").toBe(true);
  expect(JSON.parse(posts[1]!).command_id).toBe(JSON.parse(posts[0]!).command_id);
  await page.waitForTimeout(500);
  expect(await daemon.inputs(thread)).toHaveLength(1);
});

test("double_submit", async ({ daemon, page }) => {
  test.setTimeout(150_000);
  const thread = await openGate(daemon);
  await openThread(page, daemon, thread);
  const sends = commandPosts(page, "send");
  const spawns = commandPosts(page, "spawn");

  await composer(page).fill("once");
  await sendButton(page).dblclick();
  await expect(commandLine(page, `send:${thread}`)).toHaveAttribute("data-phase", "settled");
  expect(sends).toHaveLength(1);
  expect(await daemon.inputs(thread)).toHaveLength(1);
  // The thread's stream settles (the input acked) before the Retry part needs a current copy.
  const [once] = await daemon.waitInputs(thread, (is) => is[0]?.state === "acked");
  await expect(inputRow(page, once!.input_id)).toHaveAttribute("data-state", "acked");

  const cwd = mkdtempSync(join(daemon.root, "cwd-form-"));
  await page.getByLabel("Working directory (absolute path)").fill(cwd);
  await page.getByRole("button", { name: "Start thread" }).dblclick();
  await expect(commandLine(page, "spawn")).toHaveAttribute("data-phase", "settled", { timeout: 45_000 });
  expect(spawns).toHaveLength(1);
  const host = await daemon.stream("host");
  expect(host.filter((e) => e.kind === "thread_registered" && e.payload.cwd === cwd)).toHaveLength(1);

  // Retry, double-clicked: one more POST.
  await selectThread(page, thread);
  let first = true;
  await page.route("**/api/commands/send", async (route) => {
    if (first) {
      first = false;
      await route.fulfill({ status: 503, contentType: "application/json", body: '{"code":"busy","retry_after":1}' });
    } else {
      await route.continue();
    }
  });
  await composer(page).fill("retry once");
  await expect(sendButton(page)).toBeEnabled();
  await expect(page.getByText("Commands wait until")).toHaveCount(0);
  await sendButton(page).click();
  await expect(commandLine(page, `send:${thread}`)).toHaveAttribute("data-phase", "not_done");
  const retry = commandLine(page, `send:${thread}`).getByRole("button", { name: "Retry" });
  await expect(retry).toBeVisible({ timeout: 5_000 });
  await retry.dblclick();
  await expect(commandLine(page, `send:${thread}`)).toHaveAttribute("data-phase", "settled");
  expect(sends).toHaveLength(3);
  expect(await daemon.inputs(thread)).toHaveLength(2);
  await page.unroute("**/api/commands/send");

  // An unconfirmed input: It arrived held, then Dismiss sends nothing.
  const fresh = await openGate(daemon);
  await selectThread(page, fresh);
  const id = await unconfirmed(daemon, page, fresh);
  const resolves = commandPosts(page, "input_resolve");
  const held = await hold(page, "/api/commands/input_resolve");
  const row = inputRow(page, id);
  await row.getByRole("button", { name: "It arrived" }).click();
  const route = await held.next();
  await expect(row.getByRole("button", { name: "Dismiss" })).toBeDisabled();
  await row.getByRole("button", { name: "Dismiss" }).dispatchEvent("click");
  await banner(page).locator(`[data-cause="input_unconfirmed"]`).getByRole("button", { name: "Dismiss" }).dispatchEvent("click");
  await page.waitForTimeout(300);
  expect(resolves).toHaveLength(1);
  await route.continue();
  await daemon.waitInputs(fresh, (is) => is[0]?.state === "acked");
  await expect(row).toHaveAttribute("data-state", "acked");
  expect(resolves).toHaveLength(1);
});

test("spawn_stop_resume", async ({ daemon, page }) => {
  test.setTimeout(150_000);
  await openLink(page, await daemon.pairUrl());
  await expect(statusOf(page)).toHaveText("live");
  const cwd = mkdtempSync(join(daemon.root, "cwd-form-"));
  await page.getByLabel("Working directory (absolute path)").fill(cwd);
  await page.getByLabel("Task (optional)").fill("from the form");
  await page.getByRole("button", { name: "Start thread" }).click();
  await expect(commandLine(page, "spawn")).toContainText("outcome: started", { timeout: 45_000 });
  const reg = (await daemon.stream("host")).find((e) => e.kind === "thread_registered" && e.payload.cwd === cwd);
  const thread = reg!.payload.thread_id as string;
  expect(reg!.payload.task).toBe("from the form");
  // The selection moved to the new thread.
  await expect(page.locator(`li[data-thread="${thread}"] button`).first()).toHaveAttribute("aria-current", "true");
  await daemon.waitEvents(thread, (es) => es.some((e) => e.kind === "readiness_observed"));

  await expect(actions(page).getByRole("button", { name: "Stop" })).toBeEnabled({ timeout: 20_000 });
  await actions(page).getByRole("button", { name: "Stop" }).click();
  const dialog = page.getByRole("dialog", { name: "Stop this launch?" });
  await dialog.getByRole("button", { name: "Stop" }).click();
  await expect(commandLine(page, `launch:${thread}`)).toContainText("stopped, death confirmed", { timeout: 45_000 });
  const es = await daemon.waitEvents(thread, (xs) => xs.some((e) => e.kind === "launch_exited"));
  const exited = es.find((e) => e.kind === "launch_exited")!;
  expect([exited.payload.reason, exited.payload.death_confirmed]).toEqual(["stopped", true]);

  const resume = actions(page).getByRole("button", { name: "Resume" });
  await expect(resume).toBeEnabled({ timeout: 20_000 });
  await resume.click();
  await expect(commandLine(page, `launch:${thread}`)).toContainText("Resume: outcome: started", { timeout: 45_000 });
  await expect(commandLine(page, `launch:${thread}`)).toBeVisible();
  await daemon.waitEvents(thread, (xs) => xs.filter((e) => e.kind === "launch_started").length === 2);
  // The counts shown are the daemon's (W2-9).
  await expect(actions(page)).toContainText(`raw_unknown count: ${await daemon.rawUnknown(`thread:${thread}`)}`);
  await expect(page.locator("[data-raw-unknown=host]")).toHaveText(`host raw_unknown count: ${await daemon.rawUnknown("host")}`);
});

test.describe("without a claude on PATH", () => {
  test.use({ claude: "missing" });
  test("spawn_failed", async ({ daemon, page }) => {
    await openLink(page, await daemon.pairUrl());
    await expect(statusOf(page)).toHaveText("live");
    await page.getByLabel("Working directory (absolute path)").fill(mkdtempSync(join(daemon.root, "cwd-")));
    await page.getByRole("button", { name: "Start thread" }).click();
    await expect(commandLine(page, "spawn")).toContainText("outcome: spawn_failed", { timeout: 45_000 });
  });
});

test("stop_dialog_pins", async ({ daemon, page }) => {
  test.setTimeout(120_000);
  const thread = await openGate(daemon);
  await openThread(page, daemon, thread);
  const stops = commandPosts(page, "stop");
  await actions(page).getByRole("button", { name: "Stop" }).click();
  const dialog = page.getByRole("dialog", { name: "Stop this launch?" });
  await expect(dialog).toBeVisible();
  // Launch A ends and B starts from the CLI: the dialog pinned A, so it closes.
  await daemon.chaldea(["stop", thread]);
  await daemon.chaldea(["resume", thread]);
  await expect(dialog).toBeHidden({ timeout: 20_000 });
  await page.waitForTimeout(500);
  expect(stops).toEqual([]);
});

test("death_unconfirmed_replies", async ({ daemon, page }) => {
  // No CI trigger exists for an exited launch with death unconfirmed on a real daemon: the
  // replies that report one are fulfilled here. Each shows on its own line; the hint itself
  // follows the current launch, which the stream shows confirmed dead, so it is not shown
  // (commands.map covers the hint from a copy with death unconfirmed).
  test.setTimeout(120_000);
  const thread = await openGate(daemon);
  await page.route("**/api/commands/stop", async (route) => {
    const res = await route.fetch();
    const body = (await res.json()) as Record<string, unknown>;
    await route.fulfill({ response: res, json: { ...body, death_confirmed: false, confirmed_within_deadline: false } });
  });
  await page.route("**/api/commands/resume", (route) =>
    route.fulfill({ status: 409, contentType: "application/json", body: '{"code":"death_unconfirmed"}' }),
  );
  await openThread(page, daemon, thread);
  await actions(page).getByRole("button", { name: "Stop" }).click();
  await page.getByRole("dialog", { name: "Stop this launch?" }).getByRole("button", { name: "Stop" }).click();
  await expect(commandLine(page, `launch:${thread}`)).toContainText("death not confirmed", { timeout: 45_000 });
  await daemon.waitEvents(thread, (es) => es.some((e) => e.kind === "launch_exited" && e.payload.death_confirmed === true));
  const resume = actions(page).getByRole("button", { name: "Resume" });
  await expect(resume).toBeEnabled({ timeout: 20_000 });
  await expect(actions(page).locator(".hint")).toHaveCount(0);
  await resume.click();
  await expect(commandLine(page, `launch:${thread}`)).toContainText("not confirmed dead");
  await expect(actions(page).locator(".hint")).toHaveCount(0);
});

/**
 * A server that answers every request with `status` and its headers, sends the start of a
 * body promising more, then resets the connection: a reply whose body fails after the headers.
 */
async function cuttingServer(status: number, origin: string): Promise<{ url: string; close: () => Promise<void> }> {
  const server = createServer((req, res) => {
    req.resume();
    res.writeHead(status, {
      "content-type": "application/json",
      "content-length": "4096",
      "access-control-allow-origin": origin,
      "cache-control": "no-store",
    });
    res.write('{"code":"in_pro');
    setTimeout(() => res.socket?.destroy(), 100);
  });
  await new Promise<void>((ok) => server.listen(0, "127.0.0.1", ok));
  const a = server.address();
  const port = typeof a === "object" && a ? a.port : 0;
  return {
    url: `http://127.0.0.1:${port}/cut`,
    close: () =>
      new Promise<void>((ok) => {
        server.closeAllConnections();
        server.close(() => ok());
      }),
  };
}

test("reply_body_cut", async ({ daemon, page }) => {
  // The headers arrive and the body fails: the outcome is unknown, whatever the status.
  const thread = await openGate(daemon);
  const cut409 = await cuttingServer(409, daemon.origin);
  const cut408 = await cuttingServer(408, daemon.origin);
  try {
    await page.route("**/api/commands/spawn", (route) => route.continue({ url: cut409.url }));
    await page.route("**/api/commands/send", (route) => route.continue({ url: cut408.url }));
    await openThread(page, daemon, thread);
    const spawns = commandPosts(page, "spawn");
    const sends = commandPosts(page, "send");
    await page.getByLabel("Working directory (absolute path)").fill(mkdtempSync(join(daemon.root, "cwd-")));
    await page.getByRole("button", { name: "Start thread" }).click();
    await expect(commandLine(page, "spawn")).toHaveAttribute("data-phase", "check_thread", { timeout: 20_000 });
    await expect(page.getByRole("button", { name: "Start thread" })).toBeDisabled();
    await composer(page).fill("cut reply");
    await sendButton(page).click();
    const line = commandLine(page, `send:${thread}`);
    await expect(line).toHaveAttribute("data-phase", "check_thread", { timeout: 20_000 });
    await page.waitForTimeout(1500);
    await expect(line.getByRole("button", { name: "Retry" })).toHaveCount(0);
    await expect(sendButton(page)).toBeDisabled();
    expect([spawns.length, sends.length]).toEqual([1, 1]);
  } finally {
    await cut409.close();
    await cut408.close();
  }
});

test("single_flight_held", async ({ daemon, page }) => {
  // The first POST is held before it is forwarded, with no reopen: the keyboard, the button,
  // and the launch controls send nothing more while the slot is held.
  test.setTimeout(120_000);
  const thread = await openGate(daemon);
  const sendHold = await hold(page, "/api/commands/send");
  const stopHold = await hold(page, "/api/commands/stop");
  await openThread(page, daemon, thread);
  const sends = commandPosts(page, "send");
  const stops = commandPosts(page, "stop");
  await composer(page).fill("once only");
  await composer(page).press("Control+Enter");
  const first = await sendHold.next();
  await composer(page).press("Control+Enter");
  await composer(page).press("Enter");
  await sendButton(page).dispatchEvent("click");
  await page.waitForTimeout(500);
  expect(sends).toHaveLength(1);
  await first.continue();
  await expect(commandLine(page, `send:${thread}`)).toHaveAttribute("data-phase", "settled");
  expect(sends).toHaveLength(1);
  expect(await daemon.inputs(thread)).toHaveLength(1);

  await actions(page).getByRole("button", { name: "Stop" }).click();
  await page.getByRole("dialog", { name: "Stop this launch?" }).getByRole("button", { name: "Stop" }).click();
  const stop = await stopHold.next();
  await expect(actions(page).getByRole("button", { name: "Stop" })).toBeDisabled();
  await actions(page).getByRole("button", { name: "Stop" }).dispatchEvent("click");
  await page.waitForTimeout(300);
  await expect(page.getByRole("dialog", { name: "Stop this launch?" })).toBeHidden();
  expect(stops).toHaveLength(1);
  await stop.continue();
  await expect(commandLine(page, `launch:${thread}`)).toContainText("stopped", { timeout: 45_000 });
  expect(stops).toHaveLength(1);
});

test("late_result_after_404", async ({ daemon, page }) => {
  // A Send held, its thread 404s (selection and composer go), then `queued` arrives: the
  // result is shown in the status bar and announced.
  const thread = await openGate(daemon);
  const tap = await socketTap(page);
  const held = await hold(page, "/api/commands/send");
  let gone = false;
  await page.route(
    (u) => u.pathname === "/api/events" && u.searchParams.get("stream_id") === `thread:${thread}`,
    async (route) => {
      if (gone) await route.fulfill({ status: 404, contentType: "application/json", body: '{"code":"not_found"}' });
      else await route.continue();
    },
  );
  await openThread(page, daemon, thread);
  await composer(page).fill("late result");
  await sendButton(page).click();
  const route = await held.next();
  const res = await route.fetch();
  gone = true;
  tap.drop();
  await expect(composer(page)).toHaveCount(0, { timeout: 20_000 });
  await route.fulfill({ response: res });
  const last = page.locator("[data-last-command]");
  await expect(last).toContainText("Send: queued.");
  await expect(last).toContainText("claude ");
  await expect(page.getByRole("status")).toHaveText(await last.innerText());
});

test("dialog_keyboard", async ({ daemon, page, browserName }) => {
  // WebKit moves focus to buttons with Alt+Tab (the macOS default).
  const tab = browserName === "webkit" ? "Alt+Tab" : "Tab";
  const thread = await openGate(daemon);
  await openThread(page, daemon, thread);
  const trigger = actions(page).getByRole("button", { name: "Stop" });
  await expect(trigger).toBeEnabled();
  await page.locator(`li[data-thread="${thread}"] button`).first().focus();
  for (let i = 0; i < 40 && !(await trigger.evaluate((el) => el === document.activeElement)); i++) {
    await page.keyboard.press(tab);
  }
  await expect(trigger).toBeFocused();
  await page.keyboard.press("Enter");
  const dialog = page.getByRole("dialog", { name: "Stop this launch?" });
  await expect(dialog).toBeVisible();
  expect(await dialog.evaluate((d) => d.contains(document.activeElement))).toBe(true);
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  await expect(trigger).toBeFocused();
});

test("command_401", async ({ daemon, context }) => {
  test.setTimeout(120_000);
  const thread = await openGate(daemon);

  // (b) An old generation's 401 arrives after a re-pair in another tab: the replacement survives.
  const a = await context.newPage();
  const auth: string[] = [];
  a.on("request", (r) => {
    const h = r.headers().authorization;
    if (h) auth.push(h);
  });
  await openThread(a, daemon, thread);
  const tokenA = auth.at(-1)!;
  const held = await hold(a, "/api/commands/send");
  await composer(a).fill("held with the old pairing");
  await sendButton(a).click();
  const route = await held.next();
  const b = await context.newPage();
  await openLink(b, await daemon.pairUrl());
  await expect(statusOf(b)).toHaveText("live");
  await expect.poll(() => auth.at(-1) !== tokenA, { message: "tab A never adopted the new pairing", timeout: 15_000 }).toBe(true);
  const tokenB = auth.at(-1)!;
  await route.fulfill({ status: 401, contentType: "application/json", body: '{"code":"unauthorized"}' });
  await expect(commandLine(a, `send:${thread}`)).toContainText("the pairing was refused");
  await a.waitForTimeout(1_000);
  await expect(a.getByText(SPENT)).toHaveCount(0);
  await expect(statusOf(a)).toHaveText("live");
  await expect(composer(a)).toHaveValue("held with the old pairing");
  await expect(sendButton(a)).toBeEnabled();
  await a.reload();
  await expect(statusOf(a)).toHaveText("live");
  expect(auth.at(-1) === tokenB, "the replacement credential did not survive").toBe(true);
  await a.unroute("**/api/commands/send");

  // (a) A 401 for the current generation clears it, and the page asks for a fresh link.
  await a.route("**/api/commands/send", (r) =>
    r.fulfill({ status: 401, contentType: "application/json", body: '{"code":"unauthorized"}' }),
  );
  await selectThread(a, thread);
  await composer(a).fill("refused");
  await expect(composer(a)).toHaveValue("refused");
  await sendButton(a).click();
  await expect(a.getByText(SPENT)).toBeVisible({ timeout: 15_000 });
  await expect(b.getByText(SPENT)).toBeVisible({ timeout: 15_000 });
});

test("retry_session", async ({ daemon, page }) => {
  test.setTimeout(150_000);
  const thread = await openGate(daemon);
  const tap = await socketTap(page);
  const held = await hold(page, "/api/commands/send");
  await openThread(page, daemon, thread);
  const posts = commandPosts(page, "send");
  const line = commandLine(page, `send:${thread}`);
  const reopen = async () => {
    const before = tap.opened();
    tap.drop();
    await expect.poll(() => tap.opened(), { timeout: 20_000 }).toBeGreaterThan(before);
    await expect(statusOf(page)).toHaveText("live", { timeout: 20_000 });
  };

  // (a) Not done (408), then a reopen: Retry goes, the slot is released.
  await composer(page).fill("a: not done");
  await sendButton(page).click();
  await (await held.next()).fulfill({ status: 408, contentType: "application/json", body: '{"code":"request_timeout"}' });
  await expect(line.getByRole("button", { name: "Retry" })).toBeVisible();
  await reopen();
  await expect(line.getByRole("button", { name: "Retry" })).toHaveCount(0);
  await expect(line).toHaveAttribute("data-phase", "released");
  await expect(sendButton(page)).toBeEnabled();
  expect(posts).toHaveLength(1);

  // (b) In flight across a reopen, then the reply is lost: check the thread, no Retry.
  await composer(page).fill("b: lost");
  await sendButton(page).click();
  const b = await held.next();
  await reopen();
  await b.abort();
  await expect(line).toHaveAttribute("data-phase", "check_thread");
  await expect(line.getByRole("button", { name: "Retry" })).toHaveCount(0);
  expect(posts).toHaveLength(2);
  await line.getByRole("button", { name: "Forget" }).click();

  // (c) A `queued` reply held across a reopen: a newer Send waits for the slot (single
  // flight); the late reply shows its result and does not clear the untouched draft.
  await composer(page).fill("c: late queued");
  await sendButton(page).click();
  const c = await held.next();
  const res = await c.fetch();
  await reopen();
  await expect(sendButton(page)).toBeDisabled();
  await composer(page).press("Control+Enter");
  await page.waitForTimeout(300);
  expect(posts).toHaveLength(3);
  await c.fulfill({ response: res });
  await expect(line).toHaveAttribute("data-phase", "settled");
  await expect(line).toContainText("queued");
  await expect(composer(page)).toHaveValue("c: late queued");
  await expect(sendButton(page)).toBeEnabled();
});
