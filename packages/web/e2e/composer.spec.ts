// The composer against the daemon (docs/design/w2-composer.md §2.1, §4): W2-4
// `ime_sequences`, W2-6 `send_intact`, `send_rejected_keeps_draft`, `draft_edit_during_send`,
// `oversize_no_post`, and W2-11 `offline_locks`, `catch_up_after_hello` (with
// `stop_during_paging`).

import { devices, type Page, type Route } from "@playwright/test";

import {
  caughtUp,
  commandLine,
  commandPosts,
  composer,
  expect,
  openGate,
  openLink,
  openThread,
  selectThread,
  sendButton,
  socketTap,
  statusOf,
  test,
  untilShown,
} from "./harness";

const sendSlot = (thread: string) => `send:${thread}`;

test("send_intact", async ({ daemon, page }) => {
  test.setTimeout(120_000);
  const thread = await openGate(daemon);
  await openThread(page, daemon, thread);
  const text = "日本語のテスト 🎌 が";
  await composer(page).fill(text);
  expect(await composer(page).inputValue()).toBe(text);
  await sendButton(page).click();
  await expect(commandLine(page, sendSlot(thread))).toHaveAttribute("data-phase", "settled");
  // The draft cleared on `queued`; the stored text is the value, byte for byte.
  await expect(composer(page)).toHaveValue("");
  const [first] = await daemon.waitInputs(thread, (is) => is[0]?.state === "acked");
  expect(first!.text).toBe(text);
  expect(first!.origin).toBe("pwa");
  await expect(page.locator(`[data-input="${first!.input_id}"]`).first()).toHaveAttribute("data-state", "acked");

  // A 200 KiB prompt, stored as a blob and read back through /api/blobs.
  const big = `${"日本語の長い入力".repeat(8_600)}🎌`;
  expect(new TextEncoder().encode(big).length).toBeGreaterThan(200 * 1024);
  await composer(page).fill(big);
  await expect(sendButton(page)).toBeEnabled();
  await sendButton(page).click();
  await expect(commandLine(page, sendSlot(thread))).toHaveAttribute("data-phase", "settled", { timeout: 20_000 });
  const inputs = await daemon.waitInputs(thread, (is) => is.length === 2);
  expect(inputs[1]!.text === big, "the stored 200 KiB text differs from the textarea's").toBe(true);
});

test("send_rejected_keeps_draft", async ({ daemon, page }) => {
  const thread = await openGate(daemon);
  await openThread(page, daemon, thread);
  await composer(page).fill("!ls");
  await sendButton(page).click();
  const line = commandLine(page, sendSlot(thread));
  await expect(line).toHaveAttribute("data-phase", "settled");
  await expect(line).toContainText("rejected: the CLI would not receive it as written");
  await expect(composer(page)).toHaveValue("!ls");
  const [i] = await daemon.waitInputs(thread, (is) => is[0]?.state === "rejected");
  expect(i!.reason).toBe("rejected");
});

test("draft_edit_during_send", async ({ daemon, page }) => {
  const thread = await openGate(daemon);
  const { thread: other } = await daemon.spawnThread();
  let release!: () => void;
  const released = new Promise<void>((ok) => {
    release = ok;
  });
  await page.route("**/api/commands/send", async (route) => {
    // Committed, its reply held until the stream showed the input acked.
    const res = await route.fetch();
    await released;
    await route.fulfill({ response: res });
  });
  await openThread(page, daemon, thread);
  await composer(page).fill("first prompt");
  await sendButton(page).click();
  await expect(commandLine(page, sendSlot(thread))).toHaveAttribute("data-phase", "inflight");
  await composer(page).fill("a new draft 新しい");
  await page.locator(`li[data-thread="${other}"] button`).first().click();
  await expect(composer(page)).toHaveValue("");
  await selectThread(page, thread);
  await expect(composer(page)).toHaveValue("a new draft 新しい");
  const [i] = await daemon.waitInputs(thread, (is) => is[0]?.state === "acked");
  await expect(page.locator(`[data-input="${i!.input_id}"]`).first()).toHaveAttribute("data-state", "acked");
  release();
  const line = commandLine(page, sendSlot(thread));
  await expect(line).toHaveAttribute("data-phase", "settled");
  await expect(line).toContainText("queued");
  await expect(composer(page)).toHaveValue("a new draft 新しい");
  // The late `queued` changes no streamed state.
  await expect(page.locator(`[data-input="${i!.input_id}"]`).first()).toHaveAttribute("data-state", "acked");
});

test("oversize_no_post", async ({ daemon, page }) => {
  const thread = await openGate(daemon);
  await openThread(page, daemon, thread);
  const posts = commandPosts(page, "send");
  const big = "あ".repeat(90_000);
  await composer(page).fill(big);
  await sendButton(page).click();
  await expect(composer(page)).toHaveAttribute("aria-invalid", "true");
  const described = await composer(page).evaluate((el) =>
    (el.getAttribute("aria-describedby") ?? "")
      .split(" ")
      .map((id) => document.getElementById(id)?.textContent ?? "")
      .join(" "),
  );
  expect(described).toMatch(/over 256 KiB/);
  await page.waitForTimeout(500);
  expect(posts).toEqual([]);
  expect((await composer(page).inputValue()) === big).toBe(true);
});

test("offline_locks", async ({ daemon, page }) => {
  const thread = await openGate(daemon);
  await openThread(page, daemon, thread);
  const posts: string[] = [];
  page.on("request", (r) => {
    if (new URL(r.url()).pathname.startsWith("/api/commands/")) posts.push(r.url());
  });
  await composer(page).fill("kept while offline");
  await daemon.stop();
  await expect(statusOf(page)).toHaveText("offline", { timeout: 20_000 });
  await expect(sendButton(page)).toBeDisabled();
  await expect(page.getByRole("button", { name: "Start thread" })).toBeDisabled();
  await composer(page).press("Control+Enter");
  await composer(page).press("Enter");
  await page.waitForTimeout(500);
  expect(posts).toEqual([]);
  expect(await composer(page).inputValue()).toMatch(/^kept while offline/);
});

/** Holds the thread's forward reads while `on`; `release` lets the held ones go. */
async function holdForward(page: Page, thread: string) {
  const held: Route[] = [];
  const s = { on: false };
  await page.route(
    (u) =>
      u.pathname === "/api/events" && u.searchParams.get("stream_id") === `thread:${thread}` && u.searchParams.has("after_seq"),
    async (route) => {
      if (s.on) held.push(route);
      else await route.continue();
    },
  );
  return {
    hold: () => {
      s.on = true;
    },
    held,
    release: async () => {
      s.on = false;
      for (const r of held.splice(0)) await r.continue().catch(() => {});
    },
  };
}

test("catch_up_after_hello", async ({ daemon, page }) => {
  test.setTimeout(120_000);
  const thread = await openGate(daemon);
  const tap = await socketTap(page);
  const reads = await holdForward(page, thread);
  await openThread(page, daemon, thread);
  await composer(page).fill("waits for the copy");
  await expect(sendButton(page)).toBeEnabled();

  // A reopen makes the thread's forward read due; while it is held the copy is not current.
  reads.hold();
  tap.drop();
  await expect.poll(() => reads.held.length, { timeout: 20_000 }).toBeGreaterThan(0);
  await expect(statusOf(page)).toHaveText("live");
  await expect(sendButton(page)).toBeDisabled();
  await expect(page.getByText("Commands wait until this thread is caught up.")).toBeVisible();
  // Stop uses the latest valid copy.
  await expect(page.getByRole("region", { name: "Thread actions" }).getByRole("button", { name: "Stop" })).toBeEnabled();
  await reads.release();
  await expect(sendButton(page)).toBeEnabled({ timeout: 20_000 });

  // stop_during_paging: the thread's forward pages still loading, Stop goes through.
  const stops = commandPosts(page, "stop");
  reads.hold();
  tap.drop();
  await expect.poll(() => reads.held.length, { timeout: 20_000 }).toBeGreaterThan(0);
  await expect(sendButton(page)).toBeDisabled();
  await page.getByRole("region", { name: "Thread actions" }).getByRole("button", { name: "Stop" }).click();
  await page.getByRole("dialog", { name: "Stop this launch?" }).getByRole("button", { name: "Stop" }).click();
  await expect(commandLine(page, `launch:${thread}`)).toContainText("stopped, death confirmed", { timeout: 45_000 });
  expect(stops).toHaveLength(1);
  await reads.release();
  await daemon.waitEvents(thread, (es) => es.some((e) => e.kind === "launch_exited"));
});

// ---- IME (W2-4; CL-22). Real IMEs are W2-1, the owner's check. ----------------------------

/** Dispatches a synthesized composition and its confirming Enter, in `order`. */
async function syntheticComposition(page: Page, text: string, how: "inside" | "webkit229" | "window") {
  return composer(page).evaluate(
    async (el, { text, how }) => {
      const ta = el as HTMLTextAreaElement;
      const key = (keyCode: number, isComposing: boolean) => {
        const e = new KeyboardEvent("keydown", { key: "Enter", code: "Enter", bubbles: true, cancelable: true, isComposing });
        // The init dictionary leaves keyCode 0.
        Object.defineProperty(e, "keyCode", { value: keyCode });
        return ta.dispatchEvent(e);
      };
      ta.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true, data: "" }));
      // The committed text, as the IME leaves it.
      const set = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
      set?.call(ta, text);
      ta.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertCompositionText", data: text }));
      let notPrevented: boolean;
      if (how === "inside") {
        notPrevented = key(229, true);
        ta.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true, data: text }));
      } else {
        // WebKit's order: compositionend before the confirming keydown.
        ta.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true, data: text }));
        if (how === "webkit229") {
          // Past the 50 ms window, so keyCode 229 alone holds the Enter back.
          await new Promise((ok) => setTimeout(ok, 80));
          notPrevented = key(229, false);
        } else {
          notPrevented = key(13, false);
        }
      }
      return notPrevented;
    },
    { text, how },
  );
}

test("ime_sequences", async ({ daemon, page, browserName, browser }) => {
  test.setTimeout(120_000);
  const thread = await openGate(daemon);
  await openThread(page, daemon, thread);
  const posts = commandPosts(page, "send");
  const line = commandLine(page, sendSlot(thread));
  /** A fresh Enter after the composition sends once. */
  const freshEnter = async (n: number) => {
    await page.waitForTimeout(100);
    expect(posts, "a POST after the confirming Enter").toHaveLength(n - 1);
    // The earlier inputs are done writing events, and the page shows the thread's last one:
    // the copy stays current for this Enter. An input is done once queued behind the closed
    // gate, or written with the echo CLI's prompt recorded (acked; a decomposed text, which
    // the echo CLI does not normalize, stays `sent` until the 30 s window).
    const settled = await daemon.waitEvents(thread, (es) => {
      const inputs = es.filter((e) => e.kind === "input_created").length;
      const written = new Set(
        es.filter((e) => e.kind === "input_state_changed" && e.payload.to === "sent").map((e) => e.payload.input_id),
      ).size;
      const pending = es.some((e) => e.kind === "input_state_changed" && e.payload.to === "sending" && !es.some(
        (x) => x.kind === "input_state_changed" && x.payload.input_id === e.payload.input_id && x.payload.from === "sending",
      ));
      return inputs === n - 1 && !pending && es.filter((e) => e.kind === "user_message").length >= written;
    });
    await untilShown(page, settled.at(-1)!.seq);
    await caughtUp(page);
    await composer(page).press("Enter");
    await expect.poll(() => posts.length).toBe(n);
    await expect(line).toHaveAttribute("data-phase", "settled");
  };

  let n = 0;
  if (browserName === "chromium") {
    // A real composition through CDP: the confirming Enter arrives as keyCode 229.
    const cdp = await page.context().newCDPSession(page);
    await composer(page).focus();
    await cdp.send("Input.imeSetComposition", { text: "にほんご", selectionStart: 4, selectionEnd: 4 });
    await cdp.send("Input.dispatchKeyEvent", {
      type: "rawKeyDown",
      key: "Enter",
      code: "Enter",
      windowsVirtualKeyCode: 229,
      nativeVirtualKeyCode: 229,
    });
    await cdp.send("Input.insertText", { text: "日本語" });
    await cdp.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
    await expect(composer(page)).toHaveValue("日本語");
    await freshEnter(++n);
    expect(JSON.parse(posts[n - 1]!).args.text).toBe("日本語");
  }
  for (const how of ["inside", "webkit229", "window"] as const) {
    // A decomposed が (U+304B U+3099): kept as typed, never normalized to NFC.
    const text = `変換 ${how} が`;
    expect(await syntheticComposition(page, text, how), `${how}: the composing Enter was prevented`).toBe(true);
    await expect(composer(page)).toHaveValue(text);
    await freshEnter(++n);
    expect(JSON.parse(posts[n - 1]!).args.text).toBe(text);
  }
  const stored = (await daemon.waitInputs(thread, (is) => is.length === n)).map((i) => i.text);
  for (const how of ["inside", "webkit229", "window"] as const) {
    expect(stored).toContain(`変換 ${how} が`);
  }
  expect(stored.some((t) => t.includes("が"))).toBe(false);

  if (browserName !== "chromium") return;
  // A coarse primary pointer: plain Enter is a newline; Ctrl+Enter sends.
  const mobile = await browser.newContext({ ...devices["Pixel 7"] });
  try {
    const p = await mobile.newPage();
    expect(await p.evaluate(() => matchMedia("(pointer: coarse)").matches)).toBe(true);
    const sent = commandPosts(p, "send");
    await openLink(p, await daemon.pairUrl());
    await selectThread(p, thread);
    await composer(p).fill("一行目");
    await composer(p).press("End");
    await composer(p).press("Enter");
    await expect(composer(p)).toHaveValue("一行目\n");
    await p.waitForTimeout(300);
    expect(sent).toEqual([]);
    await composer(p).press("Control+Enter");
    await expect.poll(() => sent.length).toBe(1);
  } finally {
    await mobile.close();
  }
});
