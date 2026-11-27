// W1-5 `sync.converges`: when the network quiesces after any losses, delays, restarts,
// and epoch changes, the client holds the server's epoch, cursors, owned projections,
// and exact window (docs/design/w1-pwa.md §4).

import fc from "fast-check";
import { describe, expect, test } from "vitest";

import { HISTORY_FAILED, problems } from "./check";
import { MODEL_CONFIG, Sim, TEST_CONFIG } from "./sim";
import { status } from "./state";
import { threadId } from "./model/server";
import { type Op, opArb, run, seeded, start, startArb } from "./ops";

export const NUM_RUNS = 1000;

/**
 * Quiesces and checks convergence. A history read that failed shows a Retry and is not
 * retried automatically: that outcome is reported as such, and everything else, the
 * window it retains included, must already hold; then the user retries and the page must
 * converge exactly.
 */
export function settle(sim: Sim): void {
  sim.quiesce();
  const first = problems(sim);
  if (first.some((p) => p.startsWith(HISTORY_FAILED))) {
    expect(sim.state.window?.pending).not.toBeNull();
    expect(first.filter((p) => !p.startsWith(HISTORY_FAILED))).toEqual([]);
    sim.dispatch({ type: "retry" });
    sim.quiesce();
  }
  expect(problems(sim)).toEqual([]);
}

describe("sync.converges", () => {
  test("random schedules, half from the seeded race, converge after quiesce", () => {
    fc.assert(
      fc.property(startArb, fc.array(opArb(), { maxLength: 80, size: "max" }), (from, ops: Op[]) => {
        const sim = start(from);
        for (const op of ops) run(sim, op);
        settle(sim);
      }),
      { numRuns: NUM_RUNS },
    );
  });

  test("regression: a history timeout then deselection leaves the page live (seed 2640285)", () => {
    // The counterexample of review round 2, replayed by its ops and by its seed and path.
    const ops: Op[] = [{ t: "older" }, { t: "loseReq", i: 0 }, { t: "timeout", i: 0 }, { t: "select", n: null }];
    const sim = start("warm");
    for (const op of ops) run(sim, op);
    settle(sim);
    expect(status(sim.state)).toBe("live");
    fc.assert(
      fc.property(startArb, fc.array(opArb(), { maxLength: 80, size: "max" }), (from, generated: Op[]) => {
        const s = start(from);
        for (const op of generated) run(s, op);
        settle(s);
      }),
      { seed: 2640285, path: "8062:3:1:2:8:8:12:15:15:15", endOnFailure: true },
    );
  });

  test("a tail page split by the read limit converges on what was served", () => {
    const sim = new Sim(TEST_CONFIG, MODEL_CONFIG);
    sim.server.register(1, false);
    for (let k = 0; k < 4; k++) sim.server.message(threadId(1), 2500, "日本語🎌");
    sim.boot();
    sim.quiesce();
    sim.dispatch({ type: "select", thread: threadId(1) });
    settle(sim);
    const limits = sim.log.flatMap((e) => (e.type === "fetch" && e.req.dir === "backward" ? [e.req.limit] : []));
    expect(limits).toEqual([3, 1]);
    expect(sim.state.window?.events.map((e) => e.seq)).toEqual([4]);
  });

  test("a lost initial tail read is not converged: it shows a Retry, then loads the tail", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: threadId(2) });
    sim.connect();
    const key = `thread:${threadId(2)}`;
    for (let k = 0; k < 10 && sim.flight(key, "backward") === undefined; k++) {
      for (const id of [...sim.flights.keys()]) sim.serve(id);
      while (sim.deliver());
    }
    const tail = sim.flight(key, "backward") ?? -1;
    expect(tail).toBeGreaterThan(0);
    sim.loseRequest(tail);
    sim.timeout(tail);
    sim.quiesce();
    expect(sim.state.window?.events).toEqual([]);
    expect(problems(sim).some((p) => p.startsWith(HISTORY_FAILED))).toBe(true);
    sim.dispatch({ type: "retry" });
    sim.quiesce();
    expect(sim.state.window?.events.map((e) => e.seq)).toEqual([1]);
    expect(problems(sim)).toEqual([]);
  });

  test("a commit seen only through a periodic heads refresh is pulled", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    // The commit's notification is lost; only the refresh carries it.
    sim.server.message(threadId(1), 5);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    sim.frames.length = 0;
    sim.quiesce();
    expect(problems(sim)).toEqual([]);
  });

  test("a snapshot fallback after an epoch change replaces a non-empty copy", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.dispatch({ type: "visible", threads: [threadId(2)] });
    sim.quiesce();
    sim.restart();
    sim.server.swap([1, 2]);
    for (let k = 0; k < 9; k++) sim.server.message(threadId(1), 3);
    sim.quiesce();
    expect(sim.state.epoch).toBe(sim.server.db.epoch);
    expect(problems(sim)).toEqual([]);
  });
});
