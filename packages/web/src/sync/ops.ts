// Random schedules for the sync properties (test only): writes, network steps with
// losses and reordering, restarts, database swaps, and user actions.

import type { InputReason } from "@chaldea/contracts/generated/InputReason";
import type { InputState } from "@chaldea/contracts/generated/InputState";
import fc from "fast-check";

import type { Axes } from "./model/server";
import { threadId } from "./model/server";
import { MODEL_CONFIG, Sim, TEST_CONFIG } from "./sim";
import { type Config, listRows } from "./state";

export const THREADS = [1, 2, 3, 4] as const;

export type Op =
  | { t: "register"; n: number; task: boolean }
  | { t: "status"; n: number; v: number }
  | { t: "message"; n: number; size: number; jp: boolean }
  | { t: "burst"; n: number; k: number; size: number; jp: boolean }
  | { t: "unknown"; n: number }
  /** A `raw_unknown` event on a thread (`n`) or the host (`n` 0), typed or generic. */
  | { t: "raw"; n: number; typed: boolean }
  /** The launch's next step: reserved, exited (death confirmed or not), death confirmed. */
  | { t: "launch"; n: number; v: number }
  /** An input created, or the next transition of one. */
  | { t: "input"; n: number; v: number }
  | { t: "close"; n: number }
  | { t: "serve"; i: number }
  | { t: "deliver"; i: number }
  | { t: "loseReq"; i: number }
  | { t: "loseResp"; i: number }
  | { t: "timeout"; i: number }
  | { t: "fail"; i: number; how: 408 | 503 | "transport" }
  | { t: "ticket"; i: number }
  | { t: "open"; i: number }
  | { t: "sock"; i: number }
  | { t: "frame"; i: number }
  | { t: "dropFrame"; i: number }
  | { t: "refresh" }
  | { t: "restart" }
  | { t: "swap"; keep: boolean[] }
  | { t: "down" }
  | { t: "advance"; ms: number }
  | { t: "select"; n: number | null }
  | { t: "visible"; mask: boolean[] }
  | { t: "older" }
  | { t: "newer" }
  | { t: "latest" };

// Thread 1 is the busy one, so windows fill and pages split.
const n = fc.oneof({ weight: 3, arbitrary: fc.constant(1) }, { weight: 3, arbitrary: fc.constantFrom(...THREADS) });
const i = fc.nat(7);

export interface OpWeights {
  readonly writes: number;
  readonly network: number;
  readonly faults: number;
  readonly user: number;
}

export const opArb = (w: OpWeights = { writes: 3, network: 6, faults: 1, user: 3 }): fc.Arbitrary<Op> =>
  fc.oneof(
    { weight: w.writes, arbitrary: fc.record({ t: fc.constant("register" as const), n, task: fc.boolean() }) },
    { weight: w.writes * 2, arbitrary: fc.record({ t: fc.constant("status" as const), n, v: fc.nat(11) }) },
    {
      weight: w.writes * 2,
      arbitrary: fc.record({
        t: fc.constant("message" as const),
        n,
        size: fc.oneof(fc.integer({ min: 1, max: 60 }), fc.integer({ min: 500, max: 2500 })),
        jp: fc.boolean(),
      }),
    },
    {
      weight: w.writes,
      arbitrary: fc.record({
        t: fc.constant("burst" as const),
        n,
        k: fc.integer({ min: 2, max: 12 }),
        size: fc.integer({ min: 1, max: 2500 }),
        jp: fc.boolean(),
      }),
    },
    { weight: 1, arbitrary: fc.record({ t: fc.constant("unknown" as const), n }) },
    { weight: 1, arbitrary: fc.record({ t: fc.constant("close" as const), n }) },
    { weight: w.network * 3, arbitrary: fc.record({ t: fc.constant("serve" as const), i }) },
    { weight: w.network * 3, arbitrary: fc.record({ t: fc.constant("deliver" as const), i }) },
    { weight: w.faults, arbitrary: fc.record({ t: fc.constant("loseReq" as const), i }) },
    { weight: w.faults, arbitrary: fc.record({ t: fc.constant("loseResp" as const), i }) },
    { weight: w.network, arbitrary: fc.record({ t: fc.constant("timeout" as const), i }) },
    {
      weight: w.faults,
      arbitrary: fc.record({ t: fc.constant("fail" as const), i, how: fc.constantFrom(408 as const, 503 as const, "transport" as const) }),
    },
    { weight: w.network, arbitrary: fc.record({ t: fc.constant("ticket" as const), i }) },
    { weight: w.network, arbitrary: fc.record({ t: fc.constant("open" as const), i }) },
    { weight: w.network * 2, arbitrary: fc.record({ t: fc.constant("sock" as const), i }) },
    { weight: w.network * 2, arbitrary: fc.record({ t: fc.constant("frame" as const), i }) },
    { weight: w.faults, arbitrary: fc.record({ t: fc.constant("dropFrame" as const), i }) },
    { weight: 1, arbitrary: fc.record({ t: fc.constant("refresh" as const) }) },
    { weight: w.faults, arbitrary: fc.record({ t: fc.constant("restart" as const) }) },
    {
      weight: w.faults,
      arbitrary: fc.record({ t: fc.constant("swap" as const), keep: fc.array(fc.boolean(), { minLength: 4, maxLength: 4 }) }),
    },
    { weight: w.faults, arbitrary: fc.record({ t: fc.constant("down" as const) }) },
    { weight: w.network, arbitrary: fc.record({ t: fc.constant("advance" as const), ms: fc.constantFrom(500, 1000, 5000, 31000) }) },
    { weight: w.user, arbitrary: fc.record({ t: fc.constant("select" as const), n: fc.option(n, { nil: null }) }) },
    {
      weight: w.user,
      arbitrary: fc.record({ t: fc.constant("visible" as const), mask: fc.array(fc.boolean(), { minLength: 4, maxLength: 4 }) }),
    },
    { weight: w.user * 3, arbitrary: fc.record({ t: fc.constant("older" as const) }) },
    { weight: w.user, arbitrary: fc.record({ t: fc.constant("newer" as const) }) },
    { weight: 1, arbitrary: fc.record({ t: fc.constant("latest" as const) }) },
  );

/**
 * The writes W2's projections read (`raw_unknown`, launches, inputs), mixed into `opArb`'s
 * schedules. Kept apart so `opArb`'s recorded seeds and paths still replay.
 */
export const w2OpArb = (): fc.Arbitrary<Op> =>
  fc.oneof(
    { weight: 6, arbitrary: opArb() },
    {
      weight: 1,
      arbitrary: fc.record({ t: fc.constant("raw" as const), n: fc.oneof(n, fc.constant(0)), typed: fc.boolean() }),
    },
    { weight: 1, arbitrary: fc.record({ t: fc.constant("launch" as const), n, v: fc.nat(5) }) },
    { weight: 1, arbitrary: fc.record({ t: fc.constant("input" as const), n, v: fc.nat(11) }) },
  );

const PROCESS = ["starting", "running", "stopping", "exited"] as const;
const TURN = ["unknown", "idle", "running", "awaiting_approval"] as const;
const TASK = ["none", "active", "blocked", "completed", "failed", "cancelled"] as const;

function statusWrite(sim: Sim, id: string, v: number): void {
  const pick = <T>(xs: readonly T[]) => xs[v % xs.length] as T;
  const field = (["process", "turn", "task", "attention"] as const)[v % 4] ?? "process";
  const to: Axes[keyof Axes] =
    field === "process"
      ? pick(PROCESS)
      : field === "turn"
        ? pick(TURN)
        : field === "task"
          ? pick(TASK)
          : v % 3 === 0
            ? []
            : [{ kind: "startup_stall" }];
  sim.server.status(id, field, to as never);
}

let launchN = 1;
let inputN = 1;

function launchWrite(sim: Sim, id: string, v: number): void {
  const l = sim.server.thread(id)?.launch ?? null;
  if (l === null || (l.exit_reason !== null && l.death_confirmed)) {
    sim.server.launchReserved(id, `0193ffff-0000-7000-8000-${(launchN++).toString(16).padStart(12, "0")}`);
  } else if (l.exit_reason === null) {
    sim.server.launchExited(id, (["exit", "stopped", "daemon_restart"] as const)[v % 3] ?? "exit", v % 2 === 0);
  } else {
    sim.server.deathConfirmed(id);
  }
}

const NEXT: readonly (readonly [InputState, InputReason])[] = [
  ["sending", { kind: "gate_opened" }],
  ["sent", { kind: "write_completed" }],
  ["acked", { kind: "ack_attributed" }],
  ["unconfirmed", { kind: "ack_window_expired" }],
  ["cancelled", { kind: "cancelled" }],
  ["dismissed", { kind: "resolved" }],
];

function inputWrite(sim: Sim, id: string, v: number): void {
  const t = sim.server.thread(id);
  if (!t) return;
  const open = t.inputs[v % Math.max(1, t.inputs.length)];
  if (open === undefined || v % 4 === 0) {
    sim.server.inputCreated(id, `0193eeee-0000-7000-8000-${(inputN++).toString(16).padStart(12, "0")}`, `入力 ${v}`);
    return;
  }
  const [to, reason] = NEXT[v % NEXT.length] ?? NEXT[0]!;
  sim.server.inputState(id, open.input_id, to, reason, to === "dismissed" || to === "cancelled" ? "human" : null);
}

const at = <T>(xs: readonly T[], i: number): T | undefined => (xs.length === 0 ? undefined : xs[i % xs.length]);

export function run(sim: Sim, op: Op): void {
  switch (op.t) {
    case "register":
      return sim.server.register(op.n, op.task);
    case "status":
      return statusWrite(sim, threadId(op.n), op.v);
    case "message":
      return sim.server.message(threadId(op.n), op.size, op.jp ? "日本語🎌" : "x");
    case "burst":
      for (let k = 0; k < op.k; k++) sim.server.message(threadId(op.n), op.size, op.jp ? "日本語🎌" : "x");
      return;
    case "unknown":
      return sim.server.unknown(threadId(op.n));
    case "raw":
      return sim.server.rawUnknown(op.n === 0 ? "host" : `thread:${threadId(op.n)}`, op.typed);
    case "launch":
      return launchWrite(sim, threadId(op.n), op.v);
    case "input":
      return inputWrite(sim, threadId(op.n), op.v);
    case "close":
      return sim.server.close(threadId(op.n));
    case "serve": {
      const id = at([...sim.flights.keys()], op.i);
      if (id !== undefined) sim.serve(id);
      return;
    }
    case "deliver":
      if (sim.responses.length > 0) sim.deliver(op.i % sim.responses.length);
      return;
    case "loseReq": {
      const id = at([...sim.flights.keys()], op.i);
      if (id !== undefined) sim.loseRequest(id);
      return;
    }
    case "loseResp":
      if (sim.responses.length > 0) sim.loseResponse(op.i % sim.responses.length);
      return;
    case "timeout": {
      const id = at([...sim.flights.entries()].filter(([, f]) => f.lost).map(([k]) => k), op.i);
      if (id !== undefined) sim.timeout(id);
      return;
    }
    case "fail": {
      const id = at([...sim.flights.keys()], op.i);
      if (id === undefined) return;
      sim.fail(
        id,
        op.how === "transport" ? { kind: "transport" } : { kind: "status", status: op.how, retryAfter: op.how === 503 ? 2 : null },
      );
      return;
    }
    case "ticket":
      if (sim.tickets.length > 0) sim.serveTicket(op.i % sim.tickets.length);
      return;
    case "open":
      if (sim.opens.length > 0) sim.open(op.i % sim.opens.length);
      return;
    case "sock": {
      const id = at([...sim.conns.keys()], op.i);
      if (id !== undefined) sim.socketStep(id);
      return;
    }
    case "frame":
      if (sim.frames.length > 0) sim.deliverFrame(op.i % sim.frames.length);
      return;
    case "dropFrame":
      if (sim.frames.length > 0) sim.dropFrame(op.i % sim.frames.length);
      return;
    case "refresh":
      return sim.refresh();
    case "restart":
      return sim.restart();
    case "swap":
      sim.restart();
      sim.server.swap(THREADS.filter((_, k) => op.keep[k]));
      return;
    case "down":
      sim.restart();
      sim.up = false;
      return;
    case "advance":
      return sim.advance(op.ms);
    case "select": {
      // The user selects a row of the list.
      const id = op.n === null ? null : threadId(op.n);
      if (id === null || listRows(sim.state).some((r) => r.info.thread_id === id)) {
        sim.dispatch({ type: "select", thread: id });
      }
      return;
    }
    case "visible":
      sim.dispatch({
        type: "visible",
        threads: THREADS.filter((_, k) => op.mask[k]).map(threadId),
      });
      return;
    case "older":
    case "newer":
    case "latest":
      sim.dispatch({ type: op.t });
      return;
  }
}

/** A server with threads 1–3 and a few events, and a booted client. */
export function seeded(cfg: Config = TEST_CONFIG): Sim {
  const sim = new Sim(cfg, MODEL_CONFIG);
  sim.server.register(1, true);
  sim.server.register(2, false);
  sim.server.register(3, false);
  for (let k = 0; k < 14; k++) sim.server.message(threadId(1), 10 + k);
  sim.server.status(threadId(2), "turn", "running");
  sim.boot();
  return sim;
}

/** Where a random schedule starts: cold, warm (thread 1 displayed, quiesced), or the seeded race. */
export type Start = "cold" | "warm" | "race";
export const startArb: fc.Arbitrary<Start> = fc.constantFrom("cold", "warm", "race", "race");

export function start(kind: Start, cfg: Config = TEST_CONFIG): Sim {
  if (kind === "race") return racing(cfg);
  const sim = seeded(cfg);
  if (kind === "warm") {
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
  }
  return sim;
}

/**
 * The seeded race of formal-specs §7.1 (`racingState`): the client holds epoch 1's host
 * copy and the displayed thread's copy; a forward read of epoch 1 and a backward page of
 * the next epoch are in transit, the database having been replaced between them.
 */
export function racing(cfg: Config = TEST_CONFIG): Sim {
  const sim = seeded(cfg);
  for (let k = 0; k < 8; k++) sim.server.message(threadId(1), 20);
  sim.dispatch({ type: "select", thread: threadId(1) });
  sim.quiesce();
  sim.dispatch({ type: "older" });
  sim.server.message(threadId(1), 30);
  // A forward read, from a socket announcement, and the backward read are in flight.
  sim.refresh();
  for (const id of [...sim.conns.keys()]) while (sim.socketStep(id));
  while (sim.deliverFrame());
  const fwd = [...sim.flights.values()].find((f) => f.req.dir === "forward");
  const bwd = [...sim.flights.values()].find((f) => f.req.dir === "backward");
  if (fwd) sim.serve(fwd.req.id);
  sim.restart();
  sim.server.swap([1, 2]);
  sim.server.message(threadId(1), 40);
  if (bwd) {
    bwd.lost = false;
    sim.serve(bwd.req.id);
  }
  return sim;
}
