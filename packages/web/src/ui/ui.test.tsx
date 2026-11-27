// @vitest-environment jsdom
// Component checks of the list, the timeline, and the status (cheap ones; the 390 × 844
// layout is checked in a real browser).

import { cleanup, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, test } from "vitest";

import { threadId } from "../sync/model/server";
import { seeded } from "../sync/ops";
import { StatusBar } from "./StatusBar";
import { ThreadList } from "./ThreadList";
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
  test("the list shows agent, cwd, axes, closed, and the labels of unfollowed rows", () => {
    const sim = live();
    sim.server.close(threadId(3));
    sim.quiesce();
    render(<ThreadList state={sim.state} onSelect={noop} onVisible={noop} />);
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
