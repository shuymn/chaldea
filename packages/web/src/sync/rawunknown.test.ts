// W2-9 `raw_unknown.count` (docs/design/w2-composer.md §2.3): a copy's count is its
// snapshot's `raw_unknown_count` plus the accepted forward envelopes of kind `raw_unknown`
// after it, typed or not; snapshot `recent_events` and backward pages never count.

import { describe, expect, test } from "vitest";

import { threadId } from "./model/server";
import { seeded } from "./ops";
import type { Sim } from "./sim";
import { HOST, threadStream } from "./state";

const T1 = threadStream(threadId(1));
const count = (sim: Sim, key: string) => sim.state.streams[key]?.copy?.rawUnknown;

/** Records the `mode` of every successful read delivered to `sim` from now on, per stream. */
function modes(sim: Sim): string[] {
  const seen: string[] = [];
  const dispatch = sim.dispatch.bind(sim);
  sim.dispatch = (a) => {
    if (a.type === "response" && a.result.kind === "ok") seen.push(`${a.result.body.stream_id} ${a.result.body.mode}`);
    return dispatch(a);
  };
  return seen;
}

describe("raw_unknown.count", () => {
  test("replay from 0 counts every raw_unknown envelope, typed and generic, on host and thread", () => {
    const sim = seeded();
    sim.server.rawUnknown(T1, true);
    sim.server.rawUnknown(T1, false);
    sim.server.rawUnknown(HOST, false);
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    expect(count(sim, T1)).toBe(2);
    expect(count(sim, HOST)).toBe(1);
    // Live envelopes after the copy: counted once each.
    sim.server.rawUnknown(T1, false);
    sim.server.rawUnknown(HOST, true);
    sim.quiesce();
    expect(count(sim, T1)).toBe(3);
    expect(count(sim, HOST)).toBe(2);
  });

  test("a snapshot's count is the base; its recent events do not count again; later envelopes add", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    // Past the replay bound (6 events): the next forward read is a snapshot, whose
    // `recent_events` holds the last raw_unknown.
    for (let k = 0; k < 4; k++) sim.server.rawUnknown(T1, k % 2 === 0);
    for (let k = 0; k < 4; k++) sim.server.message(threadId(1), 5);
    sim.server.rawUnknown(T1, false);
    const seen = modes(sim);
    sim.restart();
    sim.quiesce();
    expect(seen).toContain(`${T1} snapshot`);
    expect(count(sim, T1)).toBe(5);
    expect(count(sim, T1)).toBe(sim.server.rawUnknownCount(T1));
    sim.server.rawUnknown(T1, true);
    sim.quiesce();
    expect(count(sim, T1)).toBe(6);
  });

  test("a snapshot replaces the count, not adds to it", () => {
    const sim = seeded();
    sim.server.rawUnknown(HOST, true);
    sim.quiesce();
    expect(count(sim, HOST)).toBe(1);
    // A host snapshot (past the replay bound) after more raw_unknown events.
    for (let k = 0; k < 7; k++) sim.server.rawUnknown(HOST, k % 2 === 0);
    const seen = modes(sim);
    sim.restart();
    sim.quiesce();
    expect(seen).toContain("host snapshot");
    expect(count(sim, HOST)).toBe(8);
  });

  test("an overlapping history page is not counted", () => {
    const sim = seeded();
    for (let k = 0; k < 3; k++) sim.server.rawUnknown(T1, false);
    for (let k = 0; k < 6; k++) sim.server.message(threadId(1), 5);
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    const want = sim.server.rawUnknownCount(T1);
    expect(count(sim, T1)).toBe(want);
    // Page back through the raw_unknown events, and jump to the tail again.
    for (let k = 0; k < 10; k++) {
      sim.dispatch({ type: "older" });
      sim.quiesce();
    }
    expect(sim.state.window?.lo).toBe(1);
    sim.dispatch({ type: "latest" });
    sim.quiesce();
    expect(count(sim, T1)).toBe(want);
  });

  test("an epoch switch takes the new database's count", () => {
    const sim = seeded();
    for (let k = 0; k < 3; k++) sim.server.rawUnknown(T1, true);
    sim.server.rawUnknown(HOST, true);
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    expect([count(sim, T1), count(sim, HOST)]).toEqual([3, 1]);
    sim.restart();
    sim.server.swap([1]);
    sim.server.rawUnknown(T1, false);
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    expect(sim.state.epoch).toBe(sim.server.db.epoch);
    expect([count(sim, T1), count(sim, HOST)]).toEqual([1, 0]);
  });
});
