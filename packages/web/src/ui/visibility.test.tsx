// @vitest-environment jsdom
// Command outcomes stay reachable (docs/design/w2-composer.md §2.2): each outcome and each
// refused send is announced through W1's `role="status"` region, once per notice, ahead of
// the debounced event counter, and the latest stays visible in the status bar. Its text and
// thread (agent, cwd, short id) are frozen when recorded. The banner renders
// `awaiting_approval` with no action (§2.4).

import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, test, vi } from "vitest";

import { canSend, context, refusal, slots, threadLabel } from "../commands/controls";
import { type CmdAction, type CmdState, type Gesture, initialCmdState, reduceCommands } from "../commands/reducer";
import { threadId } from "../sync/model/server";
import { seeded } from "../sync/ops";
import type { Sim } from "../sync/sim";
import type { HostCopy, State } from "../sync/state";
import { AttentionBanner } from "./AttentionBanner";
import type { Ops } from "./ops";
import { StatusBar } from "./StatusBar";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

const T1 = threadId(1);
const T2 = threadId(2);
type Input = CmdAction extends infer A ? (A extends unknown ? Omit<A, "now"> : never) : never;

function step(s: CmdState, a: Input): CmdState {
  return reduceCommands(s, { ...a, now: 0 } as CmdAction)[0];
}

function gesture(sim: Sim, s: CmdState, g: Pick<Gesture, "slot" | "name" | "thread"> & Partial<Gesture>): CmdState {
  return step(s, {
    type: "gesture",
    args: {},
    allowed: true,
    commandId: `c-${s.nextAttempt}`,
    generation: "gen-1",
    label: g.thread === null ? null : threadLabel(sim.state, g.thread),
    ...g,
  });
}

const reply = (s: CmdState, slot: string, r: Extract<Input, { type: "reply" }>["reply"]) =>
  step(s, { type: "reply", attempt: s.slots[slot]!.attempt, reply: r });

const announced = () => screen.getByRole("status").textContent;
const shown = () => document.querySelector("[data-last-command]")?.textContent ?? null;

function live(): Sim {
  const sim = seeded();
  sim.dispatch({ type: "select", thread: T2 });
  sim.quiesce();
  return sim;
}

describe("command visibility", () => {
  test("a late queued after the thread's 404 is announced and stays visible", () => {
    const sim = live();
    let s = step(initialCmdState(), { type: "context", ctx: context(sim.state) });
    s = step(s, { type: "edit", thread: T2, text: "held" });
    s = gesture(sim, s, { slot: slots.send(T2), name: "send", thread: T2 });
    const view = render(<StatusBar state={sim.state} cmds={s} />);
    expect(shown()).toBeNull();
    // The thread vanishes (404): its selection and composer go.
    s = step(s, { type: "context", ctx: { ...context(sim.state), gone: { [T2]: true } } });
    s = reply(s, slots.send(T2), { kind: "ok", status: 200, result: { input_id: "i", state: "queued", reject_reason: null } });
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    expect(shown()).toBe("claude /work/p2 [0193abcd]: Send: queued.");
    expect(announced()).toBe(shown());
  });

  test("a background banner Ready's result is visible and announced", () => {
    const sim = live();
    let s = step(initialCmdState(), { type: "context", ctx: context(sim.state) });
    const view = render(<StatusBar state={sim.state} cmds={s} />);
    s = gesture(sim, s, { slot: slots.ready(T1), name: "ready", thread: T1 });
    s = reply(s, slots.ready(T1), { kind: "settled", status: 409, code: "launch_changed" });
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    expect(shown()).toMatch(/^codex \/work\/p1 \[0193abcd\]: Mark ready: Refused: the thread's launch changed/);
    expect(announced()).toBe(shown());
  });

  test("the notice is frozen: a newer attempt in the slot and a host relabel do not change it", () => {
    const sim = live();
    let s = step(initialCmdState(), { type: "context", ctx: context(sim.state) });
    const view = render(<StatusBar state={sim.state} cmds={s} />);
    s = gesture(sim, s, { slot: slots.ready(T1), name: "ready", thread: T1 });
    s = reply(s, slots.ready(T1), { kind: "settled", status: 409, code: "launch_changed" });
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    const first = shown();
    // Ready again: in flight, no reply yet. Then the host copy names the thread differently.
    s = gesture(sim, s, { slot: slots.ready(T1), name: "ready", thread: T1 });
    const host = sim.state.streams.host!.copy as HostCopy;
    const row = host.threads[T1]!;
    const relabelled: State = {
      ...sim.state,
      streams: {
        ...sim.state.streams,
        host: {
          ...sim.state.streams.host!,
          copy: { ...host, threads: { ...host.threads, [T1]: { ...row, info: { ...row.info, cwd: "/work/renamed" } } } },
        },
      },
    };
    expect(threadLabel(relabelled, T1)).not.toBe(threadLabel(sim.state, T1));
    act(() => view.rerender(<StatusBar state={relabelled} cmds={s} />));
    expect(s.slots[slots.ready(T1)]?.phase).toBe("inflight");
    expect(shown()).toBe(first);
    expect(shown()).toContain("/work/p1");
    expect(announced()).toBe(first);
  });

  test("a pending Back online is announced after a command notice that took priority", () => {
    vi.useFakeTimers();
    const sim = live();
    let s = step(initialCmdState(), { type: "context", ctx: context(sim.state) });
    const view = render(<StatusBar state={sim.state} cmds={s} />);
    // Offline, then back: "Back online" is pending (debounced) when a refusal arrives.
    sim.restart();
    sim.up = false;
    for (let k = 0; k < 3; k++) {
      sim.advance(31_000);
      while (sim.serveTicket());
      for (const id of [...sim.flights.keys()]) sim.serve(id);
      while (sim.deliver());
    }
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    expect(screen.getByRole("alert")).toBeTruthy();
    sim.quiesce();
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    s = gesture(sim, s, { slot: slots.send(T2), name: "send", thread: T2 });
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    expect(announced()).toBe("claude /work/p2 [0193abcd]: Write a message first.");
    act(() => vi.advanceTimersByTime(2000));
    expect(announced()).toBe("Back online.");
  });

  test("an identical refusal twice is announced twice", () => {
    const sim = live();
    let s = step(initialCmdState(), { type: "context", ctx: context(sim.state) });
    const view = render(<StatusBar state={sim.state} cmds={s} />);
    const region = screen.getByRole("status");
    const mo = new MutationObserver(() => {});
    mo.observe(region, { childList: true, subtree: true, characterData: true });
    /** Submits an empty draft; returns the live region's mutations it caused. */
    const empty = () => {
      s = gesture(sim, s, { slot: slots.send(T2), name: "send", thread: T2 });
      act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
      return mo.takeRecords().length;
    };
    expect(empty()).toBeGreaterThan(0);
    expect(announced()).toBe("claude /work/p2 [0193abcd]: Write a message first.");
    expect(empty()).toBeGreaterThan(0);
    expect(announced()).toBe("claude /work/p2 [0193abcd]: Write a message first.");
    mo.disconnect();
  });

  test("a command notice replaces a pending event-count message", () => {
    vi.useFakeTimers();
    const sim = live();
    let s = step(initialCmdState(), { type: "context", ctx: context(sim.state) });
    const view = render(<StatusBar state={sim.state} cmds={s} />);
    // New events arrive: the counter's announcement is pending (debounced).
    sim.server.message(T2, 5);
    sim.quiesce();
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    s = gesture(sim, s, { slot: slots.send(T2), name: "send", thread: T2 });
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    act(() => vi.advanceTimersByTime(2000));
    expect(announced()).toBe("claude /work/p2 [0193abcd]: Write a message first.");
  });

  test("a gesture refused for a stale copy is shown and announced, posts nothing, keeps the draft", () => {
    const sim = live();
    sim.server.status(T2, "process", "running");
    sim.quiesce();
    let s = step(initialCmdState(), { type: "context", ctx: context(sim.state) });
    s = step(s, { type: "edit", thread: T2, text: "kept" });
    const view = render(<StatusBar state={sim.state} cmds={s} />);
    expect(canSend(sim.state, T2)).toBe(true);
    // Between render and click the copy falls behind: an announced head it has not read.
    sim.server.message(T2, 3);
    sim.refresh();
    for (const id of [...sim.conns.keys()]) while (sim.socketStep(id));
    while (sim.deliverFrame());
    expect(canSend(sim.state, T2)).toBe(false);
    const [next, effects] = reduceCommands(s, {
      type: "gesture",
      slot: slots.send(T2),
      name: "send",
      thread: T2,
      label: threadLabel(sim.state, T2),
      args: {},
      allowed: canSend(sim.state, T2),
      why: refusal(sim.state, "send", T2),
      commandId: "c1",
      generation: "gen-1",
      now: 0,
    });
    s = next;
    expect(effects.filter((e) => e.type === "post")).toEqual([]);
    expect(s.drafts[T2]?.text).toBe("kept");
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    const want = "claude /work/p2 [0193abcd]: Send: not sent — waiting for the thread to catch up.";
    expect(shown()).toBe(want);
    expect(announced()).toBe(want);
  });

  test("a gesture on a held slot is shown and announced and posts nothing", () => {
    const sim = live();
    let s = step(initialCmdState(), { type: "context", ctx: context(sim.state) });
    s = gesture(sim, s, { slot: slots.launch(T2), name: "stop", thread: T2 });
    const view = render(<StatusBar state={sim.state} cmds={s} />);
    const [next, effects] = reduceCommands(s, {
      type: "gesture",
      slot: slots.launch(T2),
      name: "stop",
      thread: T2,
      label: threadLabel(sim.state, T2),
      args: {},
      allowed: true,
      commandId: "c2",
      generation: "gen-1",
      now: 0,
    });
    expect(effects.filter((e) => e.type === "post")).toEqual([]);
    act(() => view.rerender(<StatusBar state={sim.state} cmds={next} />));
    expect(shown()).toBe("claude /work/p2 [0193abcd]: Stop: not sent — the previous one is still pending.");
    expect(announced()).toBe(shown());
  });

  test("the banner renders awaiting_approval with no action", () => {
    const sim = live();
    sim.server.status(T2, "turn", "awaiting_approval");
    sim.quiesce();
    render(<AttentionBanner state={sim.state} cmds={initialCmdState()} ops={{} as Ops} />);
    const banner = screen.getByRole("region", { name: "Attention" });
    const entry = banner.querySelector('[data-cause="awaiting_approval"]');
    expect(entry?.textContent).toBe("Awaiting an approval in the CLI");
    expect(entry?.querySelectorAll("button")).toHaveLength(0);
  });
});
