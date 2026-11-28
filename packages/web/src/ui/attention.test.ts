// W2-8 `attention.banner` (docs/design/w2-composer.md §2.4): entries come from followed
// threads' own copies only, never host "last known" values; they follow `status_changed`,
// snapshots, and unfollowing; `awaiting_approval` comes from the turn axis.

import { describe, expect, test } from "vitest";

import { threadId } from "../sync/model/server";
import { seeded } from "../sync/ops";
import { MODEL_CONFIG, Sim, TEST_CONFIG } from "../sync/sim";
import { attentionEntries } from "./attention";

const T1 = threadId(1);
const T2 = threadId(2);
const I1 = "0193dddd-0000-7000-8000-000000000001";

const kinds = (sim: Sim) => attentionEntries(sim.state).map((e) => `${e.thread === T1 ? "t1" : e.thread === T2 ? "t2" : "?"} ${e.kind}`);

describe("attention.banner", () => {
  test("a host snapshot's attention of an unfollowed thread is not shown", () => {
    const sim = new Sim({ ...TEST_CONFIG, followMax: 0 }, MODEL_CONFIG);
    sim.server.register(1, false);
    sim.server.register(2, false);
    sim.server.status(T2, "attention", [{ kind: "startup_stall" }]);
    // Past the replay bound: the host copy is a snapshot carrying T2's attention.
    for (let n = 3; n <= 8; n++) sim.server.register(n, false);
    sim.boot();
    sim.quiesce();
    const host = sim.state.streams.host?.copy;
    expect(host?.kind === "host" && host.threads[T2]?.info.attention).toEqual([{ kind: "startup_stall" }]);
    expect(sim.state.followed).not.toContain(`thread:${T2}`);
    expect(kinds(sim)).toEqual([]);
    sim.dispatch({ type: "select", thread: T2 });
    sim.quiesce();
    expect(kinds(sim)).toEqual(["t2 startup_stall"]);
    // Unfollowing drops it.
    sim.dispatch({ type: "select", thread: null });
    sim.quiesce();
    expect(kinds(sim)).toEqual([]);
  });

  test("clearing one of two causes keeps the other; the entry names the input in its state", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: T1 });
    sim.quiesce();
    sim.server.inputCreated(T1, I1, "x");
    sim.server.status(T1, "attention", [{ kind: "startup_stall" }, { kind: "input_gate_timeout", input_id: I1 }]);
    sim.quiesce();
    expect(kinds(sim)).toEqual(["t1 startup_stall", "t1 input_gate_timeout"]);
    expect(attentionEntries(sim.state)[1]?.input?.id).toBe(I1);
    sim.server.status(T1, "attention", [{ kind: "input_gate_timeout", input_id: I1 }]);
    sim.quiesce();
    expect(kinds(sim)).toEqual(["t1 input_gate_timeout"]);
    // The input moved on: the cause stays until its own clearing event, without an action.
    sim.server.inputState(T1, I1, "sending", { kind: "gate_opened" });
    sim.quiesce();
    expect(attentionEntries(sim.state)[0]?.input).toBeNull();
  });

  test("a snapshot replaces the entries", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: T1 });
    sim.quiesce();
    sim.server.status(T1, "attention", [{ kind: "session_start_timeout" }]);
    sim.quiesce();
    expect(kinds(sim)).toEqual(["t1 session_start_timeout"]);
    sim.server.status(T1, "attention", [{ kind: "resume_blocked" }]);
    for (let k = 0; k < 7; k++) sim.server.message(T1, 3);
    sim.restart();
    sim.quiesce();
    expect(kinds(sim)).toEqual(["t1 resume_blocked"]);
  });

  test("awaiting_approval comes from the turn axis", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: T2 });
    sim.quiesce();
    expect(kinds(sim)).toEqual([]);
    sim.server.status(T2, "turn", "awaiting_approval");
    sim.quiesce();
    expect(kinds(sim)).toEqual(["t2 awaiting_approval"]);
    sim.server.status(T2, "turn", "running");
    sim.quiesce();
    expect(kinds(sim)).toEqual([]);
  });
});
