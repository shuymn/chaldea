// @vitest-environment jsdom
// W2-5 and W2-10 slots and Retry (docs/design/w2-composer.md §2.2): a not-done command did
// nothing, so it frees its slot at once; its Retry is offered only through the gate on its
// frozen target, and reserves the slot again at the click through the reducer's busy check.

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, test } from "vitest";

import { canRetry, context, refusal, slots, threadLabel } from "../commands/controls";
import {
  type CmdAction,
  type CmdEffect,
  type CmdState,
  initialCmdState,
  reduceCommands,
} from "../commands/reducer";
import type { CmdInput, CommandRunner } from "../commands/runner";
import { threadId } from "../sync/model/server";
import { seeded } from "../sync/ops";
import { MODEL_CONFIG, Sim, TEST_CONFIG } from "../sync/sim";
import { AttentionBanner } from "./AttentionBanner";
import { CommandLine } from "./CommandLine";
import { bindOps } from "./ops";

afterEach(cleanup);

const T = threadId(2);
const L1 = "0193cccc-0000-7000-8000-000000000001";
const L2 = "0193cccc-0000-7000-8000-000000000002";
const I1 = "0193dddd-0000-7000-8000-000000000001";
const BUSY = { kind: "not_done", status: 503, code: "busy", retryAfter: 0 } as const;

type Post = Extract<CmdEffect, { type: "post" }>;

/** The command reducer against a live sim, with a CommandRunner-shaped facade for `bindOps`. */
function harness(sim: Sim) {
  let s: CmdState = initialCmdState(0);
  const posts: Post[] = [];
  const run = (a: CmdInput) => {
    const [next, effects] = reduceCommands(s, { ...a, now: 0 } as CmdAction);
    s = next;
    for (const e of effects) if (e.type === "post") posts.push(e);
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
    retry: (slot: string, attempt: number, name: never, allowed: (x: typeof sim.state) => boolean) =>
      run({ type: "retry", slot, attempt, name, allowed: allowed(sim.state), why: refusal(sim.state, name, s.slots[slot]?.thread ?? null, true) }),
  } as unknown as CommandRunner;
  /** Answers the latest POST with 503 `busy`. */
  const busy = () => {
    const last = posts.at(-1);
    if (last) run({ type: "reply", attempt: last.attempt, reply: BUSY });
  };
  return { runner, ops: bindOps(runner), posts, busy, state: () => s, sync: () => run({ type: "context", ctx: context(sim.state) }) };
}

function running(): Sim {
  const sim = seeded();
  sim.server.status(T, "process", "running");
  sim.server.launchReserved(T, L1);
  sim.dispatch({ type: "select", thread: T });
  sim.quiesce();
  return sim;
}

/** A's launch replaced by B on the model server. */
function replace(sim: Sim, cause: "exit" | "stopped" = "exit") {
  sim.server.launchExited(T, cause, true);
  sim.server.launchReserved(T, L2);
  sim.server.status(T, "process", "running");
}

const attemptOf = (h: ReturnType<typeof harness>, slot: string) => h.state().slots[slot]!.attempt;

describe("slots and Retry", () => {
  test("a not-done Stop on A frees the slot at once: after A exits and B starts, the banner's Stop for B is enabled", () => {
    const sim = running();
    const h = harness(sim);
    h.ops.stop(T, L1);
    h.busy();
    expect(h.state().slots[slots.launch(T)]?.phase).toBe("not_done");
    replace(sim);
    sim.server.status(T, "attention", [{ kind: "startup_stall" }]);
    sim.quiesce();
    h.sync();
    render(<AttentionBanner state={sim.state} cmds={h.state()} ops={h.ops} />);
    const stop = screen.getByRole("region", { name: "Attention" }).querySelector('[data-cause="startup_stall"] button');
    expect(stop?.textContent).toBe("Stop");
    expect((stop as HTMLButtonElement).disabled).toBe(false);
    // A's Retry is not offered (its target changed), and B's Stop goes.
    expect(canRetry(h.state(), sim.state, slots.launch(T))).toBe(false);
    h.ops.stop(T, L2);
    expect(h.posts.map((p) => JSON.parse(p.body).args.expected_launch_id)).toEqual([L1, L2]);
  });

  test("a refollow replaying an old launch hides B's Retry only until the replay reaches B", () => {
    // One-event forward pages and no other followed threads, so a refollow replays from 0.
    const sim = new Sim({ ...TEST_CONFIG, forwardLimit: 1, followMax: 0 }, MODEL_CONFIG);
    sim.server.register(1, false);
    sim.server.register(2, false);
    sim.server.status(T, "process", "running");
    sim.server.launchReserved(T, L1);
    sim.server.launchExited(T, "stopped", true);
    sim.server.launchReserved(T, L2);
    sim.boot();
    sim.dispatch({ type: "select", thread: T });
    sim.quiesce();
    const h = harness(sim);
    h.ops.stop(T, L2);
    h.busy();
    expect(canRetry(h.state(), sim.state, slots.launch(T))).toBe(true);
    const session = context(sim.state).session;
    // Unfollow (another selection), then follow again: the copy replays from seq 1, page by page.
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    sim.dispatch({ type: "select", thread: T });
    let sawA = false;
    for (let k = 0; k < 40; k++) {
      for (const id of [...sim.flights.keys()]) sim.serve(id);
      if (!sim.deliver()) break;
      const launch = sim.state.streams[`thread:${T}`]?.copy;
      if (launch?.kind === "thread" && launch.launch?.id === L1) {
        sawA = true;
        expect(canRetry(h.state(), sim.state, slots.launch(T))).toBe(false);
      }
    }
    sim.quiesce();
    h.sync();
    expect(sawA).toBe(true);
    expect(context(sim.state).session).toBe(session);
    expect(canRetry(h.state(), sim.state, slots.launch(T))).toBe(true);
    h.ops.retry(slots.launch(T), attemptOf(h, slots.launch(T)), "stop");
    expect(h.posts).toHaveLength(2);
    expect(h.posts[1]!.body).toBe(h.posts[0]!.body);
  });

  test("a refollow replaying A on its way to B offers no Retry for A, and posts nothing", () => {
    const sim = new Sim({ ...TEST_CONFIG, forwardLimit: 1, followMax: 0 }, MODEL_CONFIG);
    sim.server.register(1, false);
    sim.server.register(2, false);
    sim.server.status(T, "process", "running");
    sim.server.launchReserved(T, L1);
    sim.boot();
    sim.dispatch({ type: "select", thread: T });
    sim.quiesce();
    const h = harness(sim);
    h.ops.stop(T, L1);
    h.busy();
    expect(canRetry(h.state(), sim.state, slots.launch(T))).toBe(true);
    // B replaces A: A's Retry goes.
    replace(sim, "stopped");
    sim.quiesce();
    h.sync();
    expect(canRetry(h.state(), sim.state, slots.launch(T))).toBe(false);
    // Unfollow, then follow again: the replay passes through A, page by page.
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    sim.dispatch({ type: "select", thread: T });
    let sawA = false;
    for (let k = 0; k < 40; k++) {
      for (const id of [...sim.flights.keys()]) sim.serve(id);
      if (!sim.deliver()) break;
      const copy = sim.state.streams[`thread:${T}`]?.copy;
      if (copy?.kind === "thread" && copy.launch?.id === L1 && !copy.launch.exited) {
        sawA = true;
        expect(canRetry(h.state(), sim.state, slots.launch(T))).toBe(false);
        h.ops.retry(slots.launch(T), attemptOf(h, slots.launch(T)), "stop");
      }
    }
    sim.quiesce();
    expect(sawA).toBe(true);
    expect(canRetry(h.state(), sim.state, slots.launch(T))).toBe(false);
    expect(h.posts).toHaveLength(1);
  });

  test("a not-done Send shows the frozen text Retry will send, whatever the draft is now", () => {
    const sim = running();
    const h = harness(sim);
    h.ops.edit(T, "earlier prompt");
    h.ops.send(T);
    h.busy();
    h.ops.edit(T, "current draft");
    render(<CommandLine state={sim.state} cmds={h.state()} slot={slots.send(T)} ops={h.ops} />);
    expect(h.state().slots[slots.send(T)]?.phase).toBe("not_done");
    expect(document.querySelector("[data-frozen-text]")?.textContent).toBe("Retry sends: earlier prompt");
  });

  test("a Stop or Resume Retry refused during catch-up says it waits for the thread", () => {
    for (const kind of ["stop", "resume"] as const) {
      const sim = running();
      if (kind === "resume") {
        sim.server.status(T, "process", "exited");
        sim.server.launchExited(T, "stopped", true);
        sim.quiesce();
      }
      const h = harness(sim);
      if (kind === "stop") h.ops.stop(T, L1);
      else h.ops.resume(T, L1);
      h.busy();
      // The copy falls behind an announced head: not current, same target.
      sim.server.message(T, 3);
      sim.refresh();
      for (const id of [...sim.conns.keys()]) while (sim.socketStep(id));
      while (sim.deliverFrame());
      h.ops.retry(slots.launch(T), attemptOf(h, slots.launch(T)), kind);
      expect(h.posts, kind).toHaveLength(1);
      expect(h.state().notice?.text, kind).toMatch(
        new RegExp(`Retry ${kind === "stop" ? "Stop" : "Resume"}: not sent — waiting for the thread to catch up\\.$`),
      );
    }
  });

  test("Retry with the slot taken by a newer command is refused with a notice", () => {
    const sim = running();
    const h = harness(sim);
    h.ops.stop(T, L1);
    h.busy();
    const old = attemptOf(h, slots.launch(T));
    // The free slot takes a newer Stop, in flight.
    h.ops.stop(T, L1);
    expect(h.posts).toHaveLength(2);
    h.ops.retry(slots.launch(T), old, "stop");
    expect(h.posts).toHaveLength(2);
    expect(h.state().notice?.text).toBe("claude /work/p2 [0193abcd]: Retry Stop: not sent — the previous one is still pending.");
  });

  test("a held slot refuses a gesture with the way out: wait while in flight, check the thread when unknown", () => {
    const sim = running();
    const h = harness(sim);
    h.ops.stop(T, L1);
    h.ops.stop(T, L1);
    expect(h.state().notice?.text).toMatch(/Stop: not sent — the previous one is still pending\.$/);
    const last = h.posts.at(-1)!;
    h.runner.dispatch({ type: "reply", attempt: last.attempt, reply: { kind: "unknown", status: null, code: null } });
    h.ops.stop(T, L1);
    expect(h.state().notice?.text).toMatch(/Stop: not sent — check the thread first, or Forget the earlier one\.$/);
    expect(h.posts).toHaveLength(1);
  });

  test("Ready and Stop: a launch replaced in the same session offers and sends no Retry", () => {
    for (const kind of ["ready", "stop"] as const) {
      const sim = running();
      const h = harness(sim);
      if (kind === "ready") h.ops.ready(T, L1);
      else h.ops.stop(T, L1);
      h.busy();
      const slot = kind === "ready" ? slots.ready(T) : slots.launch(T);
      expect(canRetry(h.state(), sim.state, slot), kind).toBe(true);
      const session = context(sim.state).session;
      replace(sim, "stopped");
      sim.quiesce();
      expect(context(sim.state).session).toBe(session);
      h.sync();
      expect(canRetry(h.state(), sim.state, slot), kind).toBe(false);
      render(<CommandLine state={sim.state} cmds={h.state()} slot={slot} ops={h.ops} />);
      expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
      cleanup();
      h.ops.retry(slot, attemptOf(h, slot), kind);
      expect(h.posts, kind).toHaveLength(1);
    }
  });

  test("an input that moved on offers and sends no Retry", () => {
    const sim = running();
    sim.server.inputCreated(T, I1, "x");
    sim.quiesce();
    const h = harness(sim);
    h.ops.cancel(T, I1);
    h.busy();
    expect(canRetry(h.state(), sim.state, slots.input(I1))).toBe(true);
    sim.server.inputState(T, I1, "sending", { kind: "gate_opened" });
    sim.quiesce();
    h.sync();
    expect(canRetry(h.state(), sim.state, slots.input(I1))).toBe(false);
    h.ops.retry(slots.input(I1), attemptOf(h, slots.input(I1)), "input_cancel");
    expect(h.posts).toHaveLength(1);
  });

  test("an unchanged target keeps its Retry, with the frozen body", () => {
    const sim = running();
    const h = harness(sim);
    h.ops.stop(T, L1);
    h.busy();
    render(<CommandLine state={sim.state} cmds={h.state()} slot={slots.launch(T)} ops={h.ops} />);
    expect(screen.getByRole("button", { name: "Retry" })).toBeTruthy();
    h.ops.retry(slots.launch(T), attemptOf(h, slots.launch(T)), "stop");
    expect(h.posts).toHaveLength(2);
    expect(h.posts[1]!.body).toBe(h.posts[0]!.body);
  });

  test("Resume and It arrived keep their Retry on an unchanged target", () => {
    const a = running();
    a.server.status(T, "process", "exited");
    a.server.launchExited(T, "stopped", true);
    a.quiesce();
    const h = harness(a);
    h.ops.resume(T, L1);
    h.busy();
    expect(canRetry(h.state(), a.state, slots.launch(T))).toBe(true);
    h.ops.retry(slots.launch(T), attemptOf(h, slots.launch(T)), "resume");
    expect(h.posts).toHaveLength(2);

    const b = running();
    b.server.inputCreated(T, I1, "x");
    b.server.inputState(T, I1, "unconfirmed", { kind: "launch_exited" });
    b.quiesce();
    const g = harness(b);
    g.ops.resolve(T, I1, true);
    g.busy();
    expect(canRetry(g.state(), b.state, slots.input(I1))).toBe(true);
    g.ops.retry(slots.input(I1), attemptOf(g, slots.input(I1)), "input_resolve");
    expect(g.posts).toHaveLength(2);
  });
});
