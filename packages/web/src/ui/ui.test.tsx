// @vitest-environment jsdom
// Component checks of the list, the timeline, and the status (cheap ones; the 390 × 844
// layout is checked in a real browser).

import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, test, vi } from "vitest";

import type { CredentialStore } from "../auth/credential";
import { threadId } from "../sync/model/server";
import { seeded } from "../sync/ops";
import { Runner } from "../sync/runner";
import { threadStream } from "../sync/state";
import { App } from "./App";
import { StatusBar } from "./StatusBar";
import { SETTLE_MS, ThreadList } from "./ThreadList";
import { Timeline } from "./Timeline";

afterEach(cleanup);

const noop = () => {};

function live() {
  const sim = seeded();
  sim.server.unknown(threadId(2));
  sim.server.message(threadId(2), 12, "日本語🎌\n");
  sim.dispatch({ type: "select", thread: threadId(2) });
  sim.quiesce();
  return sim;
}

describe("ui", () => {
  test("unreadable browser storage shows a recoverable state, not connecting", async () => {
    let fail = true;
    const store = {
      listen: () => () => {},
      read: async () => {
        if (fail) throw new Error("storage unavailable");
        return null;
      },
    } as unknown as CredentialStore;
    const runner = new Runner({
      fetch: () => new Promise<Response>(() => {}),
      openSocket: () => {
        throw new Error("unused");
      },
      store,
      now: () => 0,
      socketUrl: (p) => p,
    });
    render(<App runner={runner} />);
    await act(() => runner.start());
    expect(screen.getByText(/Browser storage is unavailable/)).toBeTruthy();
    fail = false;
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Retry" }));
      await runner.retryStorage();
    });
    expect(screen.queryByText(/Browser storage is unavailable/)).toBeNull();
    expect(screen.getByText(/This link is spent or expired/)).toBeTruthy();
    runner.stop();
  });

  test("the list shows agent, cwd, axes, closed, and the labels of unfollowed rows", () => {
    const sim = live();
    sim.server.close(threadId(3));
    sim.quiesce();
    render(<ThreadList state={sim.state} onSelect={noop} onVisible={noop} onRetry={noop} />);
    const nav = screen.getByRole("navigation", { name: "Threads" });
    const items = within(nav).getAllByRole("listitem");
    expect(items).toHaveLength(3);
    const followed = items.find((li) => li.dataset.thread === threadId(2));
    expect(followed?.textContent).toContain("claude");
    expect(followed?.textContent).toContain("/work/p2");
    expect(followed?.textContent).toContain("running");
    expect(followed?.textContent).not.toContain("unavailable");
    const other = items.find((li) => li.dataset.thread === threadId(1));
    expect(other?.textContent).toContain("unavailable");
    const closed = items.find((li) => li.dataset.thread === threadId(3));
    expect(closed?.textContent).toContain("closed");
    expect(within(nav).getByRole("button", { current: true })).toBeTruthy();
  });

  test("a background stream error is visible on its row, with a retry", () => {
    const sim = live();
    const id = threadId(1);
    const key = threadStream(id);
    sim.dispatch({ type: "visible", threads: [id] });
    sim.quiesce();
    sim.server.message(id, 3);
    sim.connect();
    const request = sim.flight(key, "forward") ?? -1;
    sim.fail(request, { kind: "schema" });
    sim.deliverId(request);
    expect(sim.state.selected).toBe(threadId(2));
    expect(sim.state.streams[key]?.error?.kind).toBe("schema");
    let retried = 0;
    render(<ThreadList state={sim.state} onSelect={noop} onVisible={noop} onRetry={() => retried++} />);
    const row = screen.getAllByRole("listitem").find((li) => li.dataset.thread === id);
    if (!row) throw new Error("no row");
    expect(within(row).queryByText(/the daemon.s answer did not match/)).not.toBeNull();
    fireEvent.click(within(row).getByRole("button", { name: "Retry" }));
    expect(retried).toBe(1);
  });

  test("a background stream whose reads fail in transport is marked retrying on its row", () => {
    const sim = live();
    const id = threadId(1);
    sim.dispatch({ type: "visible", threads: [id] });
    sim.quiesce();
    sim.server.message(id, 3);
    sim.connect();
    const request = sim.flight(threadStream(id), "forward") ?? -1;
    sim.fail(request, { kind: "transport" });
    sim.deliverId(request);
    render(<ThreadList state={sim.state} onSelect={noop} onVisible={noop} onRetry={noop} />);
    const row = screen.getAllByRole("listitem").find((li) => li.dataset.thread === id);
    expect(row?.textContent).toContain("retrying");
  });

  test("visible rows are reported only once scrolling settles", () => {
    vi.useFakeTimers();
    type Observe = (entries: { target: Element; isIntersecting: boolean }[]) => void;
    const io: { observe: Observe | null } = { observe: null };
    vi.stubGlobal(
      "IntersectionObserver",
      class {
        constructor(cb: Observe) {
          io.observe = cb;
        }
        observe() {}
        disconnect() {}
      },
    );
    try {
      const sim = live();
      const reports: string[][] = [];
      render(<ThreadList state={sim.state} onSelect={noop} onVisible={(t) => reports.push(t)} onRetry={noop} />);
      const rows = screen.getAllByRole("listitem");
      io.observe?.(rows.map((target) => ({ target, isIntersecting: true })));
      // The page keeps scrolling without crossing another row boundary.
      for (let k = 0; k < 6; k++) {
        vi.advanceTimersByTime(100);
        fireEvent.scroll(window);
      }
      expect(reports).toEqual([]);
      vi.advanceTimersByTime(SETTLE_MS);
      expect(reports).toHaveLength(1);
      expect(reports[0]?.sort()).toEqual(rows.map((r) => r.dataset.thread).sort());
    } finally {
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  });

  test("the timeline is an ordered list of plain text, unknown events labelled, with window controls", () => {
    const sim = live();
    render(<Timeline state={sim.state} onOlder={noop} onNewer={noop} onLatest={noop} onRetry={noop} />);
    const list = screen.getByRole("list");
    expect(list.tagName).toBe("OL");
    expect(list.getAttribute("aria-live")).toBeNull();
    const items = within(list).getAllByRole("listitem");
    expect(items.map((li) => li.querySelector("span")?.textContent)).toEqual(
      sim.state.window?.events.map((e) => `#${e.seq}`),
    );
    expect(screen.getByText("unknown event")).toBeTruthy();
    const body = screen.getByText((t) => t.startsWith("日本語🎌"));
    expect(body.getAttribute("dir")).toBe("auto");
    expect(body.className).toContain("whitespace-pre-wrap");
    for (const name of ["Older", "Newer", "Jump to latest"]) {
      expect(screen.getByRole("button", { name })).toBeTruthy();
    }
  });

  test("status: one status region, and an alert while offline", () => {
    const sim = live();
    const { rerender } = render(<StatusBar state={sim.state} />);
    expect(screen.getByText("live")).toBeTruthy();
    expect(screen.getAllByRole("status")).toHaveLength(1);
    expect(screen.queryByRole("alert")).toBeNull();
    sim.restart();
    sim.up = false;
    sim.advance(1000);
    sim.serveTicket();
    rerender(<StatusBar state={sim.state} />);
    expect(screen.getByText("offline")).toBeTruthy();
    expect(screen.getByRole("alert").textContent).toContain("Offline");
    expect(screen.getByText(/^last synced /)).toBeTruthy();
  });
});
