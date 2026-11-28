// @vitest-environment jsdom
// W2-10 Retry targets (docs/design/w2-composer.md §2.2): a not-done command's Retry is
// shown and dispatched through one gate on its frozen target. When the launch it named
// is superseded, or its input moved on, within the same socket session, no Retry is shown
// and none is sent.

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, test } from "vitest";

import { canRetry, context, slots, supersededSlots, threadLabel } from "../commands/controls";
import {
  type CmdAction,
  type CmdEffect,
  type CmdState,
  initialCmdState,
  reduceCommands,
} from "../commands/reducer";
import type { CommandRunner } from "../commands/runner";
import { threadId } from "../sync/model/server";
import { seeded } from "../sync/ops";
import type { Sim } from "../sync/sim";
import { AttentionBanner } from "./AttentionBanner";
import { CommandLine } from "./CommandLine";
import { bindOps } from "./ops";

afterEach(cleanup);

const T = threadId(2);
const L1 = "0193cccc-0000-7000-8000-000000000001";
const L2 = "0193cccc-0000-7000-8000-000000000002";
const I1 = "0193dddd-0000-7000-8000-000000000001";
const BUSY = { kind: "not_done", status: 503, code: "busy", retryAfter: 0 } as const;

type Input = CmdAction extends infer A ? (A extends unknown ? Omit<A, "now"> : never) : never;

/** The command reducer against a live sim, with a CommandRunner-shaped facade for `bindOps`. */
function harness(sim: Sim) {
  let s: CmdState = initialCmdState(0);
  const posts: CmdEffect[] = [];
  const run = (a: Input) => {
    const [next, effects] = reduceCommands(s, { ...a, now: 0 } as CmdAction);
    s = next;
    posts.push(...effects.filter((e) => e.type === "post"));
  };
  run({ type: "context", ctx: context(sim.state) });
  const runner = {
    getState: () => s,
    dispatch: run,
    gesture: (g: { slot: string; name: never; thread: string | null; args: Record<string, unknown>; allowed: (x: typeof sim.state) => boolean }) =>
      run({
        type: "gesture",
        ...g,
        label: g.thread === null ? null : threadLabel(sim.state, g.thread),
        allowed: g.allowed(sim.state),
        commandId: `c${posts.length}`,
        generation: "gen-1",
      }),
    retry: (slot: string, allowed: (x: typeof sim.state) => boolean) => run({ type: "retry", slot, allowed: allowed(sim.state) }),
  } as unknown as CommandRunner;
  const reply = () => {
    const last = posts.at(-1);
    if (last?.type === "post") run({ type: "reply", attempt: last.attempt, reply: BUSY });
  };
  // What CommandRunner.follow does on each sync change: the context, then superseded releases.
  const sync = () => {
    run({ type: "context", ctx: context(sim.state) });
    const gone = supersededSlots(s, sim.state);
    if (gone.length > 0) run({ type: "release", slots: gone });
  };
  return { runner, ops: bindOps(runner), posts, reply, state: () => s, sync };
}

function running(): Sim {
  const sim = seeded();
  sim.server.status(T, "process", "running");
  sim.server.launchReserved(T, L1);
  sim.dispatch({ type: "select", thread: T });
  sim.quiesce();
  return sim;
}

describe("retry targets", () => {
  test("Ready and Stop: a launch superseded in the same session offers and sends no Retry", () => {
    for (const kind of ["ready", "stop"] as const) {
      const sim = running();
      const h = harness(sim);
      if (kind === "ready") h.ops.ready(T, L1);
      else h.ops.stop(T, L1);
      h.reply();
      const slot = kind === "ready" ? slots.ready(T) : slots.launch(T);
      expect(canRetry(h.state(), sim.state, slot), kind).toBe(true);
      const session = context(sim.state).session;
      // Launch A ends and B runs, with no reopen.
      sim.server.launchExited(T, "stopped", true);
      sim.server.launchReserved(T, L2);
      sim.server.status(T, "process", "running");
      sim.quiesce();
      expect(context(sim.state).session).toBe(session);
      h.sync();
      expect(h.state().slots[slot]?.phase).toBe("released");
      expect(canRetry(h.state(), sim.state, slot), kind).toBe(false);
      render(<CommandLine state={sim.state} cmds={h.state()} slot={slot} ops={h.ops} />);
      expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
      cleanup();
      h.ops.retry(slot);
      expect(h.posts, kind).toHaveLength(1);
    }
  });

  test("an input that moved on offers and sends no Retry", () => {
    const sim = running();
    sim.server.inputCreated(T, I1, "x");
    sim.quiesce();
    const h = harness(sim);
    h.ops.cancel(T, I1);
    h.reply();
    expect(canRetry(h.state(), sim.state, slots.input(I1))).toBe(true);
    sim.server.inputState(T, I1, "sending", { kind: "gate_opened" });
    sim.quiesce();
    h.sync();
    expect(canRetry(h.state(), sim.state, slots.input(I1))).toBe(false);
    h.ops.retry(slots.input(I1));
    expect(h.posts).toHaveLength(1);
  });

  test("an unchanged target keeps its Retry, with the frozen body", () => {
    const sim = running();
    const h = harness(sim);
    h.ops.stop(T, L1);
    h.reply();
    render(<CommandLine state={sim.state} cmds={h.state()} slot={slots.launch(T)} ops={h.ops} />);
    expect(screen.getByRole("button", { name: "Retry" })).toBeTruthy();
    h.ops.retry(slots.launch(T));
    expect(h.posts).toHaveLength(2);
    const [a, b] = h.posts as Extract<CmdEffect, { type: "post" }>[];
    expect(b!.body).toBe(a!.body);
  });

  test("Resume and It arrived keep their Retry on an unchanged target", () => {
    // Resume on an exited launch whose death is confirmed.
    const a = running();
    a.server.status(T, "process", "exited");
    a.server.launchExited(T, "stopped", true);
    a.quiesce();
    const h = harness(a);
    h.ops.resume(T, L1);
    h.reply();
    expect(canRetry(h.state(), a.state, slots.launch(T))).toBe(true);
    h.ops.retry(slots.launch(T));
    expect(h.posts).toHaveLength(2);

    // It arrived on an unconfirmed input.
    const b = running();
    b.server.inputCreated(T, I1, "x");
    b.server.inputState(T, I1, "unconfirmed", { kind: "launch_exited" });
    b.quiesce();
    const g = harness(b);
    g.ops.resolve(T, I1, true);
    g.reply();
    expect(canRetry(g.state(), b.state, slots.input(I1))).toBe(true);
    g.ops.retry(slots.input(I1));
    expect(g.posts).toHaveLength(2);
  });

  test("a superseded not-done Stop frees its slot: the banner's Stop for the new launch is enabled", () => {
    const sim = running();
    const h = harness(sim);
    h.ops.stop(T, L1);
    h.reply();
    expect(h.state().slots[slots.launch(T)]?.phase).toBe("not_done");
    // A exits on its own; B starts and stalls, in the same socket session.
    sim.server.launchExited(T, "exit", true);
    sim.server.launchReserved(T, L2);
    sim.server.status(T, "process", "running");
    sim.server.status(T, "attention", [{ kind: "startup_stall" }]);
    sim.quiesce();
    h.sync();
    expect(h.state().slots[slots.launch(T)]?.phase).toBe("released");
    render(<AttentionBanner state={sim.state} cmds={h.state()} ops={h.ops} />);
    const stop = screen.getByRole("region", { name: "Attention" }).querySelector('[data-cause="startup_stall"] button');
    expect(stop?.textContent).toBe("Stop");
    expect((stop as HTMLButtonElement).disabled).toBe(false);
  });

  test("an ambiguous outcome keeps its hold when the target changes", () => {
    const sim = running();
    const h = harness(sim);
    h.ops.stop(T, L1);
    const last = h.posts.at(-1);
    if (last?.type === "post") h.runner.dispatch({ type: "reply", attempt: last.attempt, reply: { kind: "unknown", status: null, code: null } });
    sim.server.launchExited(T, "exit", true);
    sim.server.launchReserved(T, L2);
    sim.quiesce();
    h.sync();
    expect(h.state().slots[slots.launch(T)]?.phase).toBe("check_thread");
  });

  test("a held slot takes no second gesture from the page", () => {
    const sim = running();
    const h = harness(sim);
    h.ops.stop(T, L1);
    h.ops.stop(T, L1);
    h.ops.ready(T, L1);
    h.ops.ready(T, L1);
    expect(h.posts.map((p) => (p.type === "post" ? p.name : ""))).toEqual(["stop", "ready"]);
  });
});
