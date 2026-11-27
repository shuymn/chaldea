// W1-6 `sync.f1`, `sync.vanished`, and `sync.reconnect` (docs/design/w1-pwa.md §2.3;
// formal-specs §7.4 F1 and "PR 18").

import fc from "fast-check";
import { describe, expect, test } from "vitest";

import { type Axes, threadId } from "./model/server";
import { seeded } from "./ops";
import { MODEL_CONFIG, Sim, TEST_CONFIG } from "./sim";
import { HOST, listRows, rowView, threadStream } from "./state";

const T1 = threadStream(threadId(1));
const T2 = threadStream(threadId(2));
const T3 = threadStream(threadId(3));

const row = (sim: Sim, n: number) => {
  const r = listRows(sim.state).find((x) => x.info.thread_id === threadId(n));
  return r ? rowView(sim.state, r) : undefined;
};

const PROCESS = ["starting", "running", "stopping", "exited"] as const;
const TURN = ["unknown", "idle", "running", "awaiting_approval"] as const;
const TASK = ["none", "active", "blocked", "completed", "failed", "cancelled"] as const;

describe("sync.f1", () => {
  test("a followed row shows the thread stream's axes, not the host snapshot's", () => {
    fc.assert(
      fc.property(
        fc.constantFrom(...PROCESS),
        fc.constantFrom(...TURN),
        fc.constantFrom(...TASK),
        fc.boolean(),
        (process, turn, task, attention) => {
          // A host snapshot (the host log is past the replay bound) read before the thread changed.
          const sim = new Sim(TEST_CONFIG, MODEL_CONFIG);
          for (let n = 1; n <= 3; n++) sim.server.register(n, n === 1);
          for (let k = 0; k < 8; k++) sim.server.close(threadId(3));
          sim.server.register(5, false);
          sim.server.register(6, false);
          sim.server.register(7, false);
          sim.server.register(8, false);
          sim.boot();
          sim.quiesce();
          expect(sim.state.streams[HOST]?.copy?.kind).toBe("host");
          const hostInfo = row(sim, 1)?.info;
          const want: Axes = { process, turn, task, attention: attention ? [{ kind: "startup_stall" }] : [] };
          sim.server.status(threadId(1), "process", want.process);
          sim.server.status(threadId(1), "turn", want.turn);
          sim.server.status(threadId(1), "task", want.task);
          sim.server.status(threadId(1), "attention", want.attention);
          // Unfollowed: the host's values, labelled.
          const before = row(sim, 1);
          expect(before?.label).not.toBeNull();
          expect(before?.process).toBe(hostInfo?.process_state);
          sim.dispatch({ type: "select", thread: threadId(1) });
          sim.quiesce();
          const after = row(sim, 1);
          expect(after?.label).toBeNull();
          expect([after?.process, after?.turn, after?.task]).toEqual([process, turn, task]);
          expect(after?.attention).toEqual(want.attention);
          // The host copy keeps its own read.
          const host = sim.state.streams[HOST]?.copy;
          expect(host?.kind === "host" && host.threads[threadId(1)]?.info.turn_state).toBe(hostInfo?.turn_state);
        },
      ),
      { numRuns: 40 },
    );
  });

  test("the initial task state comes from registration on the host stream", () => {
    const sim = seeded();
    sim.server.register(5, true);
    sim.server.message(threadId(5), 3);
    sim.dispatch({ type: "select", thread: threadId(5) });
    sim.quiesce();
    // Thread 5 was registered with a task, and its stream (a replay) never states one.
    expect(sim.state.streams[threadStream(threadId(5))]?.copy).toMatchObject({ kind: "thread", task: null });
    expect(row(sim, 5)?.label).toBeNull();
    expect(row(sim, 5)?.task).toBe("active");
    expect(row(sim, 2)?.task).toBe("none");
  });

  test("closed comes from the host, whatever the thread snapshot says", () => {
    const sim = seeded();
    for (let k = 0; k < 9; k++) sim.server.message(threadId(1), 2);
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    sim.server.close(threadId(1));
    sim.quiesce();
    expect(row(sim, 1)?.info.closed).toBe(true);
  });

  test("an unfollowed row says last known after a host snapshot, unavailable after registration only", () => {
    const sim = seeded();
    sim.quiesce();
    // The host log is short: a replay, so thread 2's row has registration defaults only.
    expect(row(sim, 2)?.label).toBe("unavailable");
    sim.restart();
    sim.server.swap([1, 2, 3]);
    sim.quiesce();
    // The epoch switch came as a host snapshot.
    expect(row(sim, 2)?.label).toBe("last known");
    expect(row(sim, 2)?.turn).toBe("unknown");
  });
});

describe("sync.vanished", () => {
  test("a 404 removes the copy, selection, and row; the stale host copy does not re-add it; an epoch switch clears it", () => {
    fc.assert(
      fc.property(fc.boolean(), (selectGone) => {
        const sim = seeded();
        sim.dispatch({ type: "select", thread: threadId(selectGone ? 2 : 1) });
        sim.dispatch({ type: "visible", threads: [threadId(1), threadId(2)] });
        sim.quiesce();
        sim.restart();
        sim.server.swap([1, 3]);
        sim.advance(1000);
        sim.connect();
        // The thread read goes first, to the new database, with the old epoch.
        sim.serveOne(T2, "forward");
        const id = sim.responses[sim.responses.length - 1]?.id ?? -1;
        sim.deliverId(id);
        expect(sim.state.epoch).toBe("epoch-1");
        expect(sim.state.gone[threadId(2)]).toBe(true);
        expect(listRows(sim.state).map((r) => r.info.thread_id)).not.toContain(threadId(2));
        expect(sim.state.streams[T2]).toBeUndefined();
        expect(sim.state.selected).toBe(selectGone ? null : threadId(1));
        // The host copy of epoch 1 still lists it; visibility does not re-follow it.
        sim.dispatch({ type: "visible", threads: [threadId(1), threadId(2)] });
        expect(sim.state.followed).not.toContain(T2);
        sim.quiesce();
        expect(sim.state.epoch).toBe("epoch-2");
        expect(sim.state.gone).toEqual({});
        expect(listRows(sim.state).map((r) => r.info.thread_id).sort()).toEqual([threadId(1), threadId(3)].sort());
      }),
      { numRuns: 4 },
    );
  });

  test("a host 404 is an error, not an empty list", () => {
    const sim = seeded();
    sim.connect();
    const id = sim.flight(HOST, "forward") ?? -1;
    sim.fail(id, { kind: "status", status: 404, retryAfter: null });
    sim.deliverId(id);
    expect(sim.state.streams[HOST]?.error?.message).toBe("the daemon reported no host stream");
  });
});

describe("sync.reconnect", () => {
  test("after a reconnect the next reads refetch every followed stream and reissue pending history", () => {
    fc.assert(
      fc.property(fc.constantFrom("older", "tail", "none"), (pending) => {
        const sim = seeded();
        sim.dispatch({ type: "select", thread: threadId(1) });
        sim.dispatch({ type: "visible", threads: [threadId(2)] });
        sim.quiesce();
        if (pending === "older") sim.dispatch({ type: "older" });
        if (pending === "tail") sim.dispatch({ type: "latest" });
        sim.restart();
        sim.advance(1000);
        while (sim.serveTicket());
        const mark = sim.log.length;
        sim.open();
        const reads = sim.log.slice(mark).flatMap((e) => (e.type === "fetch" ? [e.req] : []));
        const forward = reads.filter((r) => r.dir === "forward").map((r) => r.stream).sort();
        expect(forward).toEqual([HOST, T1, T2].sort());
        const backward = reads.filter((r) => r.dir === "backward");
        expect(backward.map((r) => r.purpose)).toEqual(pending === "none" ? [] : [pending]);
        for (const r of forward) expect(sim.state.streams[r]?.announced).toBeNull();
        sim.quiesce();
        expect(sim.state.window?.pending).toBeNull();
        expect(sim.state.streams[T3]).toBeUndefined();
      }),
      { numRuns: 6 },
    );
  });
});
