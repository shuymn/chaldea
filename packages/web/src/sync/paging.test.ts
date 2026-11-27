// W1-6 `sync.paging` and `sync.heads` (formal-specs §7.4 "PR 18": continue while
// `has_more`, page backward only while older events exist; heads merged by max for
// followed ids, omissions ignored, a delivered response does not clear an announced
// head). Run with socket heads withheld, so only the rule under test can make progress.

import fc from "fast-check";
import { describe, expect, test } from "vitest";

import { threadId } from "./model/server";
import { seeded } from "./ops";
import { MODEL_CONFIG, Sim, TEST_CONFIG } from "./sim";
import { HOST, threadStream } from "./state";

const T1 = threadStream(threadId(1));
const T2 = threadStream(threadId(2));
const NO_SNAPSHOTS = { ...MODEL_CONFIG, replayMaxEvents: 10_000, replayMaxBytes: 1 << 30 };

describe("sync.paging", () => {
  test("forward pages continue while has_more, without any socket heads", () => {
    fc.assert(
      fc.property(fc.integer({ min: 0, max: 40 }), fc.integer({ min: 1, max: 6 }), (events, limit) => {
        const sim = new Sim({ ...TEST_CONFIG, forwardLimit: limit }, NO_SNAPSHOTS);
        sim.withholdHeads = true;
        sim.server.register(1, false);
        for (let k = 0; k < events; k++) sim.server.message(threadId(1), 3);
        sim.boot();
        sim.dispatch({ type: "select", thread: threadId(1) });
        sim.quiesce(60, 0);
        expect(sim.state.streams[T1]?.copy?.cursor).toBe(events);
        expect(sim.state.streams[HOST]?.copy?.cursor).toBe(1);
        // Every forward read but the last of each stream said has_more.
        const reads = sim.log.filter((e) => e.type === "fetch" && e.req.stream === T1 && e.req.dir === "forward");
        expect(reads.length).toBeGreaterThanOrEqual(Math.max(1, Math.ceil(events / limit)));
      }),
      { numRuns: 60 },
    );
  });

  test("from a snapshot, history pages back from as_of_seq + 1 to seq 1 and stops", () => {
    fc.assert(
      fc.property(fc.integer({ min: 7, max: 30 }), (events) => {
        const sim = new Sim({ ...TEST_CONFIG, windowMaxEvents: 100 }, MODEL_CONFIG);
        sim.withholdHeads = true;
        sim.server.register(1, false);
        for (let k = 0; k < events; k++) sim.server.message(threadId(1), 3);
        sim.boot();
        sim.dispatch({ type: "select", thread: threadId(1) });
        sim.quiesce(60, 0);
        // More than the replay bound: the copy is a snapshot at the head.
        const first = sim.log.find((e) => e.type === "fetch" && e.req.stream === T1);
        expect(first?.type === "fetch" && first.req.dir).toBe("forward");
        const tail = sim.log.find((e) => e.type === "fetch" && e.req.stream === T1 && e.req.dir === "backward");
        expect(tail?.type === "fetch" && tail.req.beforeSeq).toBe(events + 1);
        let guard = 0;
        while ((sim.state.window?.lo ?? 1) > 1 && guard++ < 100) {
          sim.dispatch({ type: "older" });
          sim.quiesce(60, 0);
        }
        expect(sim.state.window?.lo).toBe(1);
        expect(sim.state.window?.hi).toBe(events);
        const backward = sim.log.filter((e) => e.type === "fetch" && e.req.dir === "backward").length;
        expect(backward).toBe(Math.ceil(events / TEST_CONFIG.backwardLimit));
        // Nothing older: Older asks for nothing more.
        sim.dispatch({ type: "older" });
        expect(sim.flight(T1, "backward")).toBeUndefined();
      }),
      { numRuns: 40 },
    );
  });
});

describe("sync.heads", () => {
  function live(): Sim {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce(60, 0);
    sim.withholdHeads = true;
    return sim;
  }
  const socketId = (sim: Sim) => sim.state.socket?.id ?? -1;

  test("an old heads frame omitting a newly followed id neither unfollows nor lowers", () => {
    fc.assert(
      fc.property(fc.integer({ min: 1, max: 5 }), (extra) => {
        const sim = live();
        const head = sim.server.head(T1);
        for (let k = 0; k < extra; k++) sim.server.message(threadId(1), 3);
        const id = socketId(sim);
        sim.dispatch({
          type: "socketMessage",
          socket: id,
          msg: { type: "heads", heads: [{ stream_id: T1, head_seq: head + extra }] },
        });
        // Thread 2 becomes followed; the old frame, selected before that subscribe merged,
        // names only the old set and an older head.
        sim.dispatch({ type: "visible", threads: [threadId(2)] });
        sim.dispatch({
          type: "socketMessage",
          socket: id,
          msg: { type: "heads", heads: [{ stream_id: T1, head_seq: head }] },
        });
        expect(sim.state.followed).toContain(T2);
        expect(sim.state.streams[T1]?.announced).toBe(head + extra);
        sim.quiesce(60, 0);
        expect(sim.state.streams[T1]?.copy?.cursor).toBe(head + extra);
      }),
      { numRuns: 30 },
    );
  });

  test("a head announced during a fetch that ends below it stays due", () => {
    const sim = live();
    const head = sim.server.head(T1);
    sim.server.message(threadId(1), 3);
    sim.server.message(threadId(1), 3);
    const id = socketId(sim);
    sim.dispatch({ type: "socketMessage", socket: id, msg: { type: "stream_advanced", stream_id: T1, head_seq: head + 2 } });
    const fwd = sim.flight(T1, "forward");
    if (fwd === undefined) throw new Error("no read");
    sim.serve(fwd); // ends at head + 2
    for (let k = 0; k < 3; k++) sim.server.message(threadId(1), 3);
    sim.dispatch({ type: "socketMessage", socket: id, msg: { type: "stream_advanced", stream_id: T1, head_seq: head + 5 } });
    sim.deliverId(fwd);
    expect(sim.state.streams[T1]?.copy?.cursor).toBe(head + 2);
    expect(sim.flight(T1, "forward")).toBeDefined();
    sim.quiesce(60, 0);
    expect(sim.state.streams[T1]?.copy?.cursor).toBe(head + 5);
  });

  test("heads of streams not followed are ignored", () => {
    const sim = live();
    sim.dispatch({
      type: "socketMessage",
      socket: socketId(sim),
      msg: { type: "heads", heads: [{ stream_id: threadStream(threadId(3)), head_seq: 9 }] },
    });
    expect(sim.state.streams[threadStream(threadId(3))]).toBeUndefined();
  });
});
