// W1-5 `sync.converges`: when the network quiesces after any losses, delays, restarts,
// and epoch changes, the client holds the server's epoch, cursors, owned projections,
// and exact window (docs/design/w1-pwa.md §4).

import fc from "fast-check";
import { describe, expect, test } from "vitest";

import { problems } from "./check";
import { threadId } from "./model/server";
import { type Op, opArb, run, seeded, start, startArb } from "./ops";

export const NUM_RUNS = 1000;

describe("sync.converges", () => {
  test("random schedules, half from the seeded race, converge after quiesce", () => {
    fc.assert(
      fc.property(startArb, fc.array(opArb(), { maxLength: 80, size: "max" }), (from, ops: Op[]) => {
        const sim = start(from);
        for (const op of ops) run(sim, op);
        sim.quiesce();
        expect(problems(sim)).toEqual([]);
      }),
      { numRuns: NUM_RUNS },
    );
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
