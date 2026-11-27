// W1-7 `sync.window`: one contiguous window at or below the cursor, within both caps,
// every event reachable with Older and Newer (docs/design/w1-pwa.md §2.4; amendments 2, 7).

import fc from "fast-check";
import { describe, expect, test } from "vitest";

import { windowProblems } from "./check";
import { threadId } from "./model/server";
import { seeded } from "./ops";
import { MODEL_CONFIG, Sim, TEST_CONFIG } from "./sim";
import { type Config, threadStream } from "./state";

const T1 = threadStream(threadId(1));
const T2 = threadStream(threadId(2));
const ID1 = threadId(1);

function busy(events: number, cfg: Partial<Config> = {}, text = "x", size = 3): Sim {
  const sim = new Sim({ ...TEST_CONFIG, ...cfg }, MODEL_CONFIG);
  sim.server.register(1, false);
  sim.server.register(2, false);
  for (let k = 0; k < events; k++) sim.server.message(ID1, size, text);
  sim.server.message(threadId(2), 3);
  sim.boot();
  return sim;
}

const press = (sim: Sim, t: "older" | "newer" | "latest") => {
  sim.dispatch({ type: t });
  sim.quiesce();
};

type Step = { t: "older" | "newer" | "latest" | "write"; size: number; jp: boolean };
const stepArb: fc.Arbitrary<Step> = fc.record({
  t: fc.constantFrom("older", "older", "newer", "latest", "write"),
  size: fc.integer({ min: 1, max: 2500 }),
  jp: fc.boolean(),
});

describe("sync.window", () => {
  test("after any Older, Newer, Jump to latest, and writes the window is exact and within the caps", () => {
    fc.assert(
      fc.property(fc.integer({ min: 0, max: 30 }), fc.array(stepArb, { maxLength: 25 }), (events, steps) => {
        const sim = busy(events);
        sim.dispatch({ type: "select", thread: ID1 });
        sim.quiesce();
        for (const s of steps) {
          if (s.t === "write") {
            sim.server.message(ID1, s.size, s.jp ? "日本語🎌" : "x");
            sim.quiesce();
          } else press(sim, s.t);
          expect(windowProblems(sim, ID1)).toEqual([]);
        }
        // Everything stays reachable: Older to seq 1, then Newer back to the live tail.
        let guard = 0;
        while ((sim.state.window?.lo ?? 1) > 1 && guard++ < 200) press(sim, "older");
        expect(sim.state.window?.lo).toBe(1);
        guard = 0;
        while (!sim.state.window?.attached && guard++ < 200) press(sim, "newer");
        expect(sim.state.window?.attached).toBe(true);
        expect(sim.state.window?.hi).toBe(sim.server.head(T1));
        expect(windowProblems(sim, ID1)).toEqual([]);
      }),
      { numRuns: 150 },
    );
  });

  test("the event cap: live events drop from the oldest end", () => {
    const sim = busy(4);
    sim.dispatch({ type: "select", thread: ID1 });
    sim.quiesce();
    // Two at a time, below the replay bound, so they arrive as live pages.
    for (let k = 0; k < 5; k++) {
      sim.server.message(ID1, 3);
      sim.server.message(ID1, 3);
      sim.quiesce();
    }
    const w = sim.state.window;
    expect(w?.events.length).toBe(TEST_CONFIG.windowMaxEvents);
    expect(w?.hi).toBe(sim.server.head(T1));
    expect(w?.attached).toBe(true);
    expect(windowProblems(sim, ID1)).toEqual([]);
  });

  test("the byte cap, counted in UTF-8 bytes: Japanese text fills it before the event cap", () => {
    // 1,200 code points of Japanese are about 3,900 bytes but 1,500 UTF-16 units.
    const sim = busy(0, { windowMaxEvents: 100, windowMaxBytes: 16 * 1024 });
    sim.dispatch({ type: "select", thread: ID1 });
    sim.quiesce();
    for (let k = 0; k < 8; k++) sim.server.message(ID1, 1200, "日本語🎌");
    sim.quiesce();
    const w = sim.state.window;
    expect(w?.events.length).toBeLessThan(8);
    expect(w?.bytes).toBeLessThanOrEqual(16 * 1024);
    const strLen = (w?.events ?? []).reduce((n, e) => n + JSON.stringify(e).length, 0);
    expect(strLen).toBeLessThan(w?.bytes ?? 0);
    expect(windowProblems(sim, ID1)).toEqual([]);
  });

  test("Older at capacity detaches; Newer and Jump to latest restore the tail", () => {
    const sim = busy(20);
    sim.dispatch({ type: "select", thread: ID1 });
    sim.quiesce();
    press(sim, "older");
    expect(sim.state.window?.attached).toBe(true);
    press(sim, "older");
    expect(sim.state.window?.attached).toBe(false);
    const hi = sim.state.window?.hi ?? 0;
    // Detached: a live event moves only the cursor.
    sim.server.message(ID1, 3);
    sim.quiesce();
    expect(sim.state.window?.hi).toBe(hi);
    press(sim, "latest");
    expect(sim.state.window?.attached).toBe(true);
    expect(sim.state.window?.hi).toBe(sim.server.head(T1));
    press(sim, "older");
    press(sim, "older");
    while (!sim.state.window?.attached) press(sim, "newer");
    expect(sim.state.window?.hi).toBe(sim.server.head(T1));
    expect(windowProblems(sim, ID1)).toEqual([]);
  });

  test("Newer never holds a seq above the cursor while the server has more", () => {
    const sim = busy(20);
    sim.dispatch({ type: "select", thread: ID1 });
    sim.quiesce();
    press(sim, "older");
    press(sim, "older");
    sim.withholdHeads = true;
    for (let k = 0; k < 5; k++) sim.server.message(ID1, 3);
    const cursor = sim.state.streams[T1]?.copy?.cursor ?? 0;
    sim.dispatch({ type: "newer" });
    const id = sim.flight(T1, "backward") ?? -1;
    sim.serve(id);
    sim.deliverId(id);
    expect(sim.state.window?.hi).toBeLessThanOrEqual(cursor);
    expect(Math.max(...(sim.state.window?.events ?? []).map((e) => e.seq))).toBeLessThanOrEqual(cursor);
  });

  test("selecting a thread with a copy issues its tail read at once; without one, none before the first forward response", () => {
    const sim = busy(10);
    sim.dispatch({ type: "visible", threads: [ID1] });
    sim.quiesce();
    const cursor = sim.state.streams[T1]?.copy?.cursor ?? -1;
    sim.dispatch({ type: "select", thread: ID1 });
    const tail = sim.flights.get(sim.flight(T1, "backward") ?? -1)?.req;
    expect(tail?.beforeSeq).toBe(cursor + 1);
    sim.quiesce();
    expect(sim.state.window?.events.length).toBeGreaterThan(0);

    sim.dispatch({ type: "select", thread: threadId(2) });
    expect(sim.flight(T2, "backward")).toBeUndefined();
    expect(sim.state.window).toBeNull();
    const fwd = sim.flight(T2, "forward") ?? -1;
    sim.serve(fwd);
    expect(sim.flight(T2, "backward")).toBeUndefined();
    sim.deliverId(fwd);
    expect(sim.flights.get(sim.flight(T2, "backward") ?? -1)?.req.beforeSeq).toBe(sim.server.head(T2) + 1);
  });

  test("a forward delivery between the tail request and its response is merged before attaching", () => {
    const sim = busy(10);
    sim.dispatch({ type: "visible", threads: [ID1] });
    sim.quiesce();
    sim.dispatch({ type: "select", thread: ID1 });
    const tail = sim.flight(T1, "backward") ?? -1;
    sim.serve(tail);
    sim.server.message(ID1, 3);
    sim.server.message(ID1, 3);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const fwd = sim.flight(T1, "forward") ?? -1;
    sim.serve(fwd);
    sim.deliverId(fwd);
    sim.deliverId(tail);
    const w = sim.state.window;
    expect(w?.attached).toBe(true);
    // The tail page (8–10) is kept under the live events (11, 12).
    expect(w?.events.map((e) => e.seq)).toEqual([8, 9, 10, 11, 12]);
    expect(windowProblems(sim, ID1)).toEqual([]);
  });

  test("selecting the already selected row keeps its window, attached or detached", () => {
    const sim = busy(20);
    sim.dispatch({ type: "select", thread: ID1 });
    sim.quiesce();
    const attached = sim.state.window;
    sim.dispatch({ type: "select", thread: ID1 });
    expect(sim.state.window).toEqual(attached);
    expect(sim.flight(T1, "backward")).toBeUndefined();
    press(sim, "older");
    press(sim, "older");
    const detached = sim.state.window;
    expect(detached?.attached).toBe(false);
    sim.dispatch({ type: "select", thread: ID1 });
    expect(sim.state.window).toEqual(detached);
  });

  test("fill four slots, then switch thread", () => {
    const sim = busy(10, { followMax: 3 });
    sim.server.register(3, false);
    sim.dispatch({ type: "select", thread: ID1 });
    sim.dispatch({ type: "visible", threads: [threadId(2), threadId(3)] });
    sim.quiesce();
    for (const n of [1, 2, 3]) sim.server.message(threadId(n), 3);
    sim.dispatch({ type: "older" });
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    expect(Object.keys(sim.state.inflight).length).toBe(4);
    sim.dispatch({ type: "select", thread: threadId(2) });
    expect(sim.state.streams[T1]?.bwd).toBeNull();
    sim.quiesce();
    expect(windowProblems(sim, threadId(2))).toEqual([]);
  });

  test("CursorAhead while detached discards the window and reloads the tail", () => {
    const sim = busy(20);
    sim.dispatch({ type: "select", thread: ID1 });
    sim.quiesce();
    press(sim, "older");
    press(sim, "older");
    expect(sim.state.window?.attached).toBe(false);
    // A restored database with the same epoch and a shorter stream: only CursorAhead tells.
    const cursor = sim.state.streams[T1]?.copy?.cursor ?? 0;
    sim.server.message(ID1, 3);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const fwd = sim.flight(T1, "forward") ?? -1;
    const a = sim.server.answer({ stream: T1, afterSeq: cursor + 50, beforeSeq: null, limit: 4, epoch: sim.state.epoch });
    if (a.status !== 200) throw new Error("no snapshot");
    expect(a.body.mode === "snapshot" && a.body.reason).toBe("cursor_ahead");
    sim.fail(fwd, { kind: "ok", body: a.body });
    sim.deliverId(fwd);
    const w = sim.state.window;
    expect(w?.attached).toBe(true);
    expect(w?.events).toEqual([]);
    expect(w?.pending).toEqual({ purpose: "tail" });
    sim.quiesce();
    expect(windowProblems(sim, ID1)).toEqual([]);
  });

  test("an epoch switch drops the window, and a new one loads", () => {
    const sim = busy(10);
    sim.dispatch({ type: "select", thread: ID1 });
    sim.quiesce();
    sim.restart();
    sim.server.swap([1, 2]);
    sim.server.message(ID1, 3);
    sim.quiesce();
    expect(sim.state.window?.events.map((e) => e.seq)).toEqual([1]);
    expect(windowProblems(sim, ID1)).toEqual([]);
  });
});
