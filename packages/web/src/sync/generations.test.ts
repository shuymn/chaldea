// W1-6 `sync.generations`: a response to a request that is no longer current changes no
// epoch, copy, or other state (formal-specs §7.4 F2; mvp §2.4 "Replay").

import fc from "fast-check";
import { describe, expect, test } from "vitest";

import { threadId } from "./model/server";
import { opArb, run, seeded, start, startArb } from "./ops";
import { type Sim, TEST_CONFIG } from "./sim";
import { HOST, type State, threadStream } from "./state";

const T1 = threadStream(threadId(1));
const T2 = threadStream(threadId(2));

/** The state without the clock, which every action advances. */
const frozen = (s: State) => JSON.stringify({ ...s, now: 0 });

/** Delivers `id`'s answer and asserts it was discarded with no state change. */
function deliverStale(sim: Sim, id: number): void {
  expect(sim.state.inflight[id]).toBeUndefined();
  const before = frozen(sim.state);
  sim.deliverId(id);
  expect(frozen(sim.state)).toBe(before);
}

function live(cfg = TEST_CONFIG): Sim {
  const sim = seeded(cfg);
  sim.dispatch({ type: "select", thread: threadId(1) });
  sim.dispatch({ type: "visible", threads: [threadId(1), threadId(2)] });
  sim.quiesce();
  return sim;
}

describe("sync.generations", () => {
  test("any response to a request no longer current changes nothing", () => {
    fc.assert(
      fc.property(startArb, fc.array(opArb(), { maxLength: 80, size: "max" }), (from, ops) => {
        const sim = start(from);
        const dispatch = sim.dispatch.bind(sim);
        let stale = 0;
        sim.dispatch = (a) => {
          if (a.type === "response" && !sim.state.inflight[a.id]) {
            stale += 1;
            const before = frozen(sim.state);
            const effects = dispatch(a as never);
            expect(frozen(sim.state)).toBe(before);
            return effects;
          }
          return dispatch(a);
        };
        for (const op of ops) run(sim, op);
        sim.quiesce();
        return stale >= 0;
      }),
      { numRuns: 300 },
    );
  });

  test("first establishment: an older host page cannot switch the epoch back", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.connect();
    const host = sim.flight(HOST, "forward");
    const thread = sim.flight(T1, "forward");
    if (host === undefined || thread === undefined) throw new Error("no reads");
    sim.serve(host); // epoch 1
    sim.server.swap([1]);
    sim.serve(thread); // epoch 2
    sim.deliverId(thread);
    expect(sim.state.epoch).toBe("epoch-2");
    deliverStale(sim, host);
    expect(sim.state.epoch).toBe("epoch-2");
  });

  test("epoch switch: a page of the old epoch after the switch is discarded", () => {
    const sim = live();
    sim.server.message(threadId(1), 3);
    sim.server.message(threadId(2), 3);
    sim.quiesce();
    sim.server.message(threadId(2), 3);
    sim.server.message(threadId(1), 3);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const old = sim.flight(T2, "forward");
    const t1 = sim.flight(T1, "forward");
    if (old === undefined || t1 === undefined) throw new Error("no reads");
    sim.serve(old); // epoch 1 page
    sim.server.swap([1, 2]);
    sim.serve(t1); // epoch_changed snapshot
    sim.deliverId(t1);
    expect(sim.state.epoch).toBe("epoch-2");
    deliverStale(sim, old);
  });

  test("socket open: a response issued before open, delivered after it, is discarded", () => {
    const sim = seeded();
    while (sim.serveTicket());
    // The socket is being constructed; a read issued now is older once it opens.
    const host = sim.flight(HOST, "forward");
    if (host === undefined) throw new Error("no host read");
    sim.serve(host);
    sim.open();
    deliverStale(sim, host);
    expect(sim.state.epoch).toBeNull();
    // The open made every followed stream due again.
    expect(sim.flight(HOST, "forward")).not.toBe(host);
    expect(sim.flight(HOST, "forward")).toBeDefined();
  });

  test("reconnect: answers to requests of the previous socket are discarded", () => {
    const sim = live();
    sim.server.message(threadId(1), 3);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const fwd = sim.flight(T1, "forward");
    if (fwd === undefined) throw new Error("no read");
    sim.serve(fwd);
    sim.restart();
    sim.advance(1000);
    sim.connect();
    deliverStale(sim, fwd);
  });

  test("snapshot replace: a backward page issued before it is discarded", () => {
    const sim = live();
    sim.dispatch({ type: "older" });
    const bwd = sim.flight(T1, "backward");
    if (bwd === undefined) throw new Error("no backward read");
    sim.serve(bwd);
    // A burst past the replay bound: the next forward read is a same-epoch snapshot.
    for (let k = 0; k < 9; k++) sim.server.message(threadId(1), 2);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const fwd = sim.flight(T1, "forward");
    if (fwd === undefined) throw new Error("no read");
    sim.serve(fwd);
    sim.deliverId(fwd);
    deliverStale(sim, bwd);
  });

  test("timeout and supersession: a late answer after the timeout is discarded", () => {
    const sim = live();
    sim.server.message(threadId(1), 3);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const first = sim.flight(T1, "forward");
    if (first === undefined) throw new Error("no read");
    sim.serve(first);
    sim.dispatch({ type: "response", id: first, result: { kind: "timeout" } });
    sim.advance(1000);
    const second = sim.flight(T1, "forward");
    expect(second).toBeDefined();
    expect(second).not.toBe(first);
    deliverStale(sim, first);
  });

  test("latest supersedes a pending history read", () => {
    const sim = live();
    sim.dispatch({ type: "older" });
    const older = sim.flight(T1, "backward");
    if (older === undefined) throw new Error("no backward read");
    sim.serve(older);
    sim.dispatch({ type: "latest" });
    deliverStale(sim, older);
  });

  test("unfollow: an answer for a stream no longer followed is discarded", () => {
    const sim = live({ ...TEST_CONFIG, followMax: 1 });
    sim.server.message(threadId(2), 3);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const fwd = sim.flight(T2, "forward");
    if (fwd === undefined) throw new Error("no read");
    sim.serve(fwd);
    // Thread 3 scrolls in; thread 2 is hidden and gives up its slot.
    sim.dispatch({ type: "visible", threads: [threadId(3)] });
    expect(sim.state.followed).not.toContain(T2);
    deliverStale(sim, fwd);
    expect(sim.state.streams[T2]).toBeUndefined();
  });

  test("a stale 404 removes nothing", () => {
    const sim = live();
    sim.server.message(threadId(2), 3);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const fwd = sim.flight(T2, "forward");
    if (fwd === undefined) throw new Error("no read");
    sim.restart();
    sim.server.swap([1]);
    const f = sim.flights.get(fwd);
    if (f) f.lost = false;
    sim.serve(fwd); // 404: thread 2 is not in the new database
    sim.advance(1000);
    sim.connect();
    deliverStale(sim, fwd);
    expect(sim.state.gone).toEqual({});
  });

  test("an old forward snapshot after a backward page of the next epoch keeps the resync due", () => {
    const sim = live();
    for (let k = 0; k < 9; k++) sim.server.message(threadId(1), 2);
    sim.dispatch({ type: "older" });
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const fwd = sim.flight(T1, "forward");
    const bwd = sim.flight(T1, "backward");
    if (fwd === undefined || bwd === undefined) throw new Error("no reads");
    sim.serve(fwd); // a same-epoch snapshot of epoch 1
    sim.server.swap([1, 2]);
    sim.serve(bwd); // a page of epoch 2
    sim.deliverId(bwd);
    sim.deliverId(fwd);
    expect(sim.state.epoch).toBe("epoch-1");
    // The resync still issues a forward read after the snapshot.
    const next = sim.flight(T1, "forward");
    expect(next).toBeDefined();
    expect(next).not.toBe(fwd);
    if (next === undefined) return;
    sim.serve(next);
    sim.deliverId(next);
    expect(sim.state.epoch).toBe("epoch-2");
  });
});
