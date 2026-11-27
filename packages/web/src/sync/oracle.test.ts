// The window oracle itself (review round 2): it rejects a window that lost a page it was
// served, or that claims to be detached at the head, and accepts a page halved by the
// read limit.

import { describe, expect, test } from "vitest";

import { windowProblems } from "./check";
import { threadId } from "./model/server";
import { MODEL_CONFIG, Sim, TEST_CONFIG } from "./sim";

const ID = threadId(1);
const bytes = (es: readonly unknown[]) => es.reduce<number>((n, e) => n + new TextEncoder().encode(JSON.stringify(e)).length, 0);

function load(size: number, text = "x"): Sim {
  const sim = new Sim(TEST_CONFIG, MODEL_CONFIG);
  sim.server.register(1, false);
  for (let k = 0; k < 4; k++) sim.server.message(ID, size, text);
  sim.boot();
  sim.quiesce();
  sim.dispatch({ type: "select", thread: ID });
  sim.quiesce();
  return sim;
}

describe("window oracle", () => {
  test("rejects a window that dropped the page Older loaded", () => {
    const sim = load(3);
    sim.dispatch({ type: "older" });
    sim.quiesce();
    const good = sim.state.window;
    if (!good) throw new Error("no window");
    expect(good.events.map((e) => e.seq)).toEqual([1, 2, 3, 4]);
    expect(windowProblems(sim, ID)).toEqual([]);
    const events = good.events.slice(1);
    sim.state = { ...sim.state, window: { ...good, lo: 2, events, bytes: bytes(events) } };
    expect(windowProblems(sim, ID).join()).toContain("expected [1,2,3,4]");
  });

  test("rejects a window detached at the head", () => {
    const sim = load(3);
    const good = sim.state.window;
    if (!good) throw new Error("no window");
    sim.state = { ...sim.state, window: { ...good, attached: false } };
    expect(windowProblems(sim, ID).join()).toContain("attached false, expected true");
  });

  test("accepts a tail page the read limit halved", () => {
    const sim = load(2500, "日本語🎌");
    const limits = sim.log.flatMap((e) => (e.type === "fetch" && e.req.dir === "backward" ? [e.req.limit] : []));
    expect(limits).toEqual([3, 1]);
    expect(sim.state.window?.events.map((e) => e.seq)).toEqual([4]);
    expect(windowProblems(sim, ID)).toEqual([]);
  });

  test("checks the window a failed read leaves, before any Retry", () => {
    const sim = load(3);
    sim.dispatch({ type: "older" });
    const id = sim.flight(`thread:${ID}`, "backward") ?? -1;
    sim.fail(id, { kind: "status", status: 503, retryAfter: null });
    sim.deliverId(id);
    const w = sim.state.window;
    if (!w) throw new Error("no window");
    expect(windowProblems(sim, ID).filter((p) => !p.startsWith("history read failed"))).toEqual([]);
    const events = w.events.slice(1);
    sim.state = { ...sim.state, window: { ...w, lo: w.lo + 1, events, bytes: bytes(events) } };
    expect(windowProblems(sim, ID).some((p) => p.startsWith("window ["))).toBe(true);
  });
});
