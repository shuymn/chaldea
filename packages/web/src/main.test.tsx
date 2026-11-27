// @vitest-environment jsdom
// The page lifecycle: leaving the page stops the client and unmounts the view; a page
// restored from the back-forward cache reloads.

import { afterEach, expect, test, vi } from "vitest";

afterEach(() => {
  vi.unstubAllGlobals();
  vi.resetModules();
  vi.doUnmock("./sync/runner");
  vi.doUnmock("react-dom/client");
  vi.doUnmock("./auth/credential");
  document.body.replaceChildren();
});

function transition(type: "pagehide" | "pageshow", persisted: boolean): Event {
  const e = new Event(type);
  Object.defineProperty(e, "persisted", { value: persisted });
  return e;
}

test("pagehide stops the client and closes the store, persisted or not; a persisted pageshow reloads", async () => {
  const root = document.createElement("div");
  root.id = "root";
  document.body.append(root);
  const order: string[] = [];
  const runner = { start: vi.fn(async () => {}), stop: vi.fn(() => order.push("stop")) };
  const close = vi.fn(() => order.push("close"));
  const reload = vi.fn();
  vi.stubGlobal("location", { reload, hash: "", pathname: "/", search: "", href: "http://127.0.0.1:1/" });
  vi.doMock("./sync/runner", () => ({ browserRunner: () => runner }));
  vi.doMock("./auth/credential", () => ({
    CredentialStore: class {
      close = close;
    },
  }));
  vi.doMock("react-dom/client", () => ({
    createRoot: () => ({
      render: () => {
        root.textContent = "Thread 2";
      },
      unmount: () => {
        root.replaceChildren();
      },
    }),
  }));
  await import("./main");

  window.dispatchEvent(transition("pagehide", true));
  expect(runner.stop).toHaveBeenCalledTimes(1);
  // The store's connection is closed after the runner stopped.
  expect(order).toEqual(["stop", "close"]);
  expect(root.textContent).toBe("");
  window.dispatchEvent(transition("pageshow", true));
  expect(reload).toHaveBeenCalledTimes(1);
  window.dispatchEvent(transition("pageshow", false));
  expect(reload).toHaveBeenCalledTimes(1);
  window.dispatchEvent(transition("pagehide", false));
  expect(runner.stop).toHaveBeenCalledTimes(2);
});
