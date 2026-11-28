// W2-7 `inputs.project` (docs/design/w2-composer.md §2.3): the thread copy's inputs and
// launch, from its own stream: snapshots give non-terminal inputs without a reason or
// actor; transitions add them; terminal transitions this copy observed are kept, 20 at most.

import { describe, expect, test } from "vitest";

import { threadId } from "./model/server";
import { seeded } from "./ops";
import type { Sim } from "./sim";
import { threadCopy } from "./state";

const T = threadId(1);
const id = (n: number) => `0193dddd-0000-7000-8000-${n.toString(16).padStart(12, "0")}`;
const L1 = "0193cccc-0000-7000-8000-000000000001";
const L2 = "0193cccc-0000-7000-8000-000000000002";

function watching(): Sim {
  const sim = seeded();
  sim.dispatch({ type: "select", thread: T });
  sim.quiesce();
  return sim;
}

/** Forces the next forward read of the thread to be a snapshot (past the replay bound). */
function viaSnapshot(sim: Sim): void {
  for (let k = 0; k < 7; k++) sim.server.message(T, 3);
  sim.restart();
  sim.quiesce();
}

describe("inputs.project", () => {
  test("transitions carry reason and actor; terminal ones move to the done list", () => {
    const sim = watching();
    sim.server.inputCreated(T, id(1), "最初");
    sim.server.inputCreated(T, id(2), "二番目");
    sim.quiesce();
    let c = threadCopy(sim.state, T);
    expect(c?.inputs.map((i) => [i.id, i.state, i.reason, i.by, i.preview])).toEqual([
      [id(1), "queued", null, null, "最初"],
      [id(2), "queued", null, null, "二番目"],
    ]);
    sim.server.inputState(T, id(1), "sending", { kind: "gate_opened" });
    sim.server.inputState(T, id(1), "sent", { kind: "write_completed" });
    sim.server.inputState(T, id(1), "unconfirmed", { kind: "ack_window_expired" });
    sim.server.inputState(T, id(2), "cancelled", { kind: "cancelled" }, "human");
    sim.quiesce();
    c = threadCopy(sim.state, T);
    expect(c?.inputs.map((i) => [i.id, i.state, i.reason?.kind])).toEqual([[id(1), "unconfirmed", "ack_window_expired"]]);
    expect(c?.done.map((i) => [i.id, i.state, i.reason?.kind, i.by])).toEqual([[id(2), "cancelled", "cancelled", "human"]]);
    sim.server.inputState(T, id(1), "dismissed", { kind: "resolved" }, "human");
    sim.quiesce();
    c = threadCopy(sim.state, T);
    expect(c?.inputs).toEqual([]);
    expect(c?.done.map((i) => i.id)).toEqual([id(2), id(1)]);
  });

  test("the done list keeps the newest 20", () => {
    const sim = watching();
    for (let n = 1; n <= 23; n++) {
      sim.server.inputCreated(T, id(n), `p${n}`);
      sim.server.inputState(T, id(n), "rejected", { kind: "rejected", reject: "lossy_text" });
      sim.quiesce();
    }
    const c = threadCopy(sim.state, T);
    expect(c?.done).toHaveLength(20);
    expect(c?.done[0]?.id).toBe(id(4));
    expect(c?.done.at(-1)?.id).toBe(id(23));
    expect(c?.done.at(-1)?.reason).toEqual({ kind: "rejected", reject: "lossy_text" });
  });

  test("a snapshot gives the non-terminal inputs without reason or actor, and no done list", () => {
    const sim = watching();
    sim.server.inputCreated(T, id(1), "a");
    sim.server.inputCreated(T, id(2), "b");
    sim.server.inputState(T, id(1), "sending", { kind: "gate_opened" });
    sim.server.inputState(T, id(1), "sent", { kind: "write_completed" });
    sim.server.inputState(T, id(1), "unconfirmed", { kind: "ack_window_expired" });
    sim.server.inputState(T, id(2), "cancelled", { kind: "cancelled" }, "human");
    sim.quiesce();
    expect(threadCopy(sim.state, T)?.done).toHaveLength(1);
    viaSnapshot(sim);
    const c = threadCopy(sim.state, T);
    expect(c?.inputs.map((i) => [i.id, i.state, i.reason, i.by, i.createdBy])).toEqual([
      [id(1), "unconfirmed", null, null, "human"],
    ]);
    expect(c?.done).toEqual([]);
  });

  test("the launch: reserved, exited, death confirmed, from replay and from a snapshot", () => {
    const sim = watching();
    expect(threadCopy(sim.state, T)?.launch).toBeNull();
    sim.server.launchReserved(T, L1);
    sim.quiesce();
    expect(threadCopy(sim.state, T)?.launch).toEqual({ id: L1, exited: false, exitReason: null, deathConfirmed: false });
    sim.server.launchExited(T, "daemon_restart", false);
    sim.quiesce();
    expect(threadCopy(sim.state, T)?.launch).toEqual({
      id: L1,
      exited: true,
      exitReason: "daemon_restart",
      deathConfirmed: false,
    });
    viaSnapshot(sim);
    expect(threadCopy(sim.state, T)?.launch).toEqual({
      id: L1,
      exited: true,
      exitReason: "daemon_restart",
      deathConfirmed: false,
    });
    sim.server.deathConfirmed(T);
    sim.quiesce();
    expect(threadCopy(sim.state, T)?.launch?.deathConfirmed).toBe(true);
    sim.server.launchReserved(T, L2);
    sim.quiesce();
    expect(threadCopy(sim.state, T)?.launch).toEqual({ id: L2, exited: false, exitReason: null, deathConfirmed: false });
  });
});
