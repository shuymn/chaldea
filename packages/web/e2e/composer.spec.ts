// The composer against the daemon (docs/design/w2-composer.md §2.1, §4): W2-4
// `ime_sequences`, W2-6 `send_intact`, `send_decomposed`, `send_rejected_keeps_draft`, `draft_edit_during_send`,
// `oversize_no_post`, and W2-11 `offline_locks`, `catch_up_after_hello` (with
// `stop_during_paging`).

import { devices, type Page, type Route } from "@playwright/test";

import { COMPOSITION_WINDOW_MS } from "../src/ui/keys";
import {
  caughtUp,
  commandLine,
  commandPosts,
  composer,
  countSubmits,
  expect,
  openGate,
  openLink,
  openThread,
  SILENCE_MS,
  selectThread,
  sendButton,
  shownAtLeast,
  socketTap,
  statusOf,
  test,
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

test("send_decomposed", async ({ daemon, page }) => {
  // A decomposed が (U+304B U+3099) is sent and stored as typed, never normalized to NFC.
  // One send, no later ones, and no wait for `acked`: the echo CLI reports the prompt without
  // Claude's NFC, so the daemon leaves it `sent` (a known FakeCli gap).
  const thread = await openGate(daemon);
  await openThread(page, daemon, thread);
  const posts = commandPosts(page, "send");
  const text = `日本語の確認です。${String.fromCharCode(0x304b, 0x3099)}`;
  await composer(page).fill(text);
  await expect(composer(page)).toHaveValue(text);
  await sendButton(page).click();
  await expect(commandLine(page, sendSlot(thread))).toHaveAttribute("data-phase", "settled");
  expect(posts).toHaveLength(1);
  const hex = (s: string) => Buffer.from(s, "utf8").toString("hex");
  expect(hex(JSON.parse(posts[0]!).args.text)).toBe(hex(text));
  const [input] = await daemon.waitInputs(thread, (is) => is.length === 1);
  expect(hex(input!.text)).toBe(hex(text));
  expect(hex(input!.text).endsWith("e3818be38299")).toBe(true);
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
  // The refusal (aria-invalid) is set in the same reducer step that would have posted.
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
  // The reducer refused them with a notice, posting nothing.
  await expect(page.locator("[data-last-command]")).toContainText("Send: not sent — the page is not live.");
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

/**
 * The synthesized sequences, each a set of composing signals and its confirming Enter:
 * - `inside`: the Enter inside the composition (isComposing, keyCode 229, the flag);
 * - `webkit229`: WebKit's order, compositionend first, then an Enter with keyCode 229 past
 *   the 50 ms window (keyCode 229 alone);
 * - `window`: compositionend, then at once an Enter with keyCode 13 (the window alone);
 * - `isComposing`: no composition events, an Enter with `isComposing` and keyCode 13
 *   (`isComposing` alone);
 * - `flag`: compositionstart, then an Enter with neither `isComposing` nor 229, then
 *   compositionend (the composition flag alone).
 */
type How = "inside" | "webkit229" | "window" | "isComposing" | "flag";

/** Dispatches a synthesized sequence and its confirming Enter; whether the Enter was not prevented. */
async function syntheticComposition(page: Page, text: string, how: How) {
  return composer(page).evaluate(
    async (el, { text, how }) => {
      const ta = el as HTMLTextAreaElement;
      const key = (keyCode: number, isComposing: boolean) => {
        const e = new KeyboardEvent("keydown", { key: "Enter", code: "Enter", bubbles: true, cancelable: true, isComposing });
        // The init dictionary leaves keyCode 0.
        Object.defineProperty(e, "keyCode", { value: keyCode });
        return ta.dispatchEvent(e);
      };
      // The committed text, as the IME leaves it.
      const commit = () => {
        const set = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
        set?.call(ta, text);
        ta.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertCompositionText", data: text }));
      };
      const start = () => ta.dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true, data: "" }));
      const end = () => ta.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true, data: text }));
      switch (how) {
        case "inside": {
          start();
          commit();
          const r = key(229, true);
          end();
          return r;
        }
        case "webkit229":
          start();
          commit();
          end();
          // Past the 50 ms window, so keyCode 229 alone holds the Enter back.
          await new Promise((ok) => setTimeout(ok, 80));
          return key(229, false);
        case "window":
          start();
          commit();
          end();
          return key(13, false);
        case "isComposing":
          commit();
          return key(13, true);
        case "flag": {
          start();
          commit();
          const r = key(13, false);
          // Ends the composition; the fresh Enter comes after the window.
          end();
          return r;
        }
      }
    },
    { text, how },
  );
}

test("ime_sequences", async ({ daemon, page, browserName, browser }) => {
  test.setTimeout(180_000);
  await openLink(page, await daemon.pairUrl());
  await expect(statusOf(page)).toHaveText("live");
  const posts = commandPosts(page, "send");
  const submits = await countSubmits(page);
  /** POSTs and submits so far: each sequence adds exactly one of each. */
  let sent = 0;
  /**
   * A fresh thread per sequence. The echo CLI never ends a turn, so a thread's gate closes
   * after its first prompt and a second input would stay queued: here every input is the
   * thread's first, so it reaches `acked` and the thread's events stop. Precomposed text, so
   * the echo CLI's prompt matches (its NFC gap is `send_decomposed`'s).
   */
  const sequence = async (name: string, text: string, guarded: () => Promise<boolean | void>) => {
    const thread = await openGate(daemon);
    // The startup writes end with the idle turn; then the page holds them and is ready.
    const es = await daemon.waitEvents(thread, (xs) =>
      xs.some((e) => e.kind === "status_changed" && e.payload.field === "turn_state" && e.payload.to === "idle"),
    );
    await selectThread(page, thread);
    await shownAtLeast(page, es.at(-1)!.seq);
    await caughtUp(page, thread);
    expect(posts, `${name}: POSTs before the guarded key`).toHaveLength(sent);
    expect(await submits(), `${name}: submits before the guarded key`).toBe(sent);

    const notPrevented = await guarded();
    if (notPrevented !== undefined) expect(notPrevented, `${name}: the composing Enter was prevented`).toBe(true);
    await expect(composer(page)).toHaveValue(text);
    // Silence: the window in which the composer still gives an Enter to the IME (keys.ts),
    // plus `SILENCE_MS`.
    await page.waitForTimeout(COMPOSITION_WINDOW_MS + SILENCE_MS);
    expect(await submits(), `${name}: the guarded key submitted the form`).toBe(sent);
    expect(posts, `${name}: the guarded key made a POST`).toHaveLength(sent);

    // One fresh Enter, the counts rechecked right before it.
    await caughtUp(page, thread);
    expect(posts).toHaveLength(sent);
    expect(await submits()).toBe(sent);
    await composer(page).press("Enter");
    sent += 1;
    await expect.poll(() => posts.length, { message: `${name}: the fresh Enter made no POST` }).toBe(sent);
    expect(await submits()).toBe(sent);
    expect(JSON.parse(posts[sent - 1]!).args.text).toBe(text);
    await expect(commandLine(page, sendSlot(thread))).toHaveAttribute("data-phase", "settled");
    const [input] = await daemon.waitInputs(thread, (is) => is[0]?.state === "acked");
    expect(input!.text === text, `${name}: the stored text differs`).toBe(true);
  };

  if (browserName === "chromium") {
    // A real composition through CDP: the confirming Enter arrives as keyCode 229.
    await sequence("cdp", "日本語", async () => {
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
    });
  }
  for (const how of ["inside", "webkit229", "window", "isComposing", "flag"] as const) {
    const text = `変換 ${how} が`;
    await sequence(how, text, () => syntheticComposition(page, text, how));
  }

  if (browserName !== "chromium") return;
  // A coarse primary pointer: plain Enter is a newline; Ctrl+Enter sends.
  const thread = await openGate(daemon);
  const mobile = await browser.newContext({ ...devices["Pixel 7"] });
  try {
    const p = await mobile.newPage();
    expect(await p.evaluate(() => matchMedia("(pointer: coarse)").matches)).toBe(true);
    const sent = commandPosts(p, "send");
    await openLink(p, await daemon.pairUrl());
    await selectThread(p, thread);
    const submitted = await countSubmits(p);
    await composer(p).fill("一行目");
    await composer(p).press("End");
    await composer(p).press("Enter");
    // The keydown ran to its default action (the newline), so a submit would already have
    // been counted: the form's submit count is the condition, not a wait.
    await expect(composer(p)).toHaveValue("一行目\n");
    expect(await submitted()).toBe(0);
    expect(sent).toEqual([]);
    await composer(p).press("Control+Enter");
    await expect.poll(() => sent.length).toBe(1);
    expect(await submitted()).toBe(1);
  } finally {
    await mobile.close();
  }
});
