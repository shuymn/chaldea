// @vitest-environment jsdom
// The page lifecycle (review round 2): into the back-forward cache the client suspends
// and keeps its view; back from it, it resumes without a reload; a discarded page is
// torn down.

import { afterEach, expect, test, vi } from "vitest";

afterEach(() => {
  vi.unstubAllGlobals();
  vi.resetModules();
  vi.doUnmock("./sync/runner");
  vi.doUnmock("react-dom/client");
  document.body.replaceChildren();
});

function transition(type: "pagehide" | "pageshow", persisted: boolean): Event {
  const e = new Event(type);
  Object.defineProperty(e, "persisted", { value: persisted });
  return e;
}

test("a persisted pagehide keeps the view; pageshow resumes without a reload", async () => {
  const root = document.createElement("div");
  root.id = "root";
  document.body.append(root);
  const runner = { start: vi.fn(async () => {}), stop: vi.fn(), suspend: vi.fn(), resume: vi.fn() };
  const reload = vi.fn();
  vi.stubGlobal("location", { reload, hash: "", pathname: "/", search: "", href: "http://127.0.0.1:1/" });
  vi.doMock("./sync/runner", () => ({ browserRunner: () => runner }));
  vi.doMock("react-dom/client", () => ({
    createRoot: () => ({
      render: () => {
        root.textContent = "Thread 2, history 8..12";
      },
      unmount: () => {
        root.replaceChildren();
      },
    }),
  }));
  await import("./main");

  window.dispatchEvent(transition("pagehide", true));
  window.dispatchEvent(transition("pageshow", true));
  expect({
    text: root.textContent,
    reloads: reload.mock.calls.length,
    suspends: runner.suspend.mock.calls.length,
    resumes: runner.resume.mock.calls.length,
    stops: runner.stop.mock.calls.length,
  }).toEqual({ text: "Thread 2, history 8..12", reloads: 0, suspends: 1, resumes: 1, stops: 0 });

  window.dispatchEvent(transition("pagehide", false));
  expect(runner.stop).toHaveBeenCalledTimes(1);
  expect(root.textContent).toBe("");
});
