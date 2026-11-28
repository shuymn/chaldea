// @vitest-environment jsdom
// Command outcomes stay reachable (docs/design/w2-composer.md §2.2): the latest outcome and
// a refused send are announced through W1's `role="status"` region and shown in the status
// bar, naming the thread, whatever is selected; and the banner renders `awaiting_approval`
// with no action (§2.4).

import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, test } from "vitest";

import { context, slots } from "../commands/controls";
import { type CmdAction, type CmdState, initialCmdState, reduceCommands } from "../commands/reducer";
import { threadId } from "../sync/model/server";
import { seeded } from "../sync/ops";
import { AttentionBanner } from "./AttentionBanner";
import type { Ops } from "./ops";
import { StatusBar } from "./StatusBar";

afterEach(cleanup);

const T1 = threadId(1);
const T2 = threadId(2);
type Input = CmdAction extends infer A ? (A extends unknown ? Omit<A, "now"> : never) : never;

function step(s: CmdState, a: Input): CmdState {
  return reduceCommands(s, { ...a, now: 0 } as CmdAction)[0];
}

const announced = () => screen.getByRole("status").textContent;
const shown = () => document.querySelector("[data-last-command]")?.textContent ?? null;

describe("command visibility", () => {
  test("a late queued after the thread's 404 is announced and stays visible", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: T2 });
    sim.quiesce();
    let s = step(initialCmdState(), { type: "context", ctx: context(sim.state) });
    s = step(s, { type: "edit", thread: T2, text: "held" });
    s = step(s, {
      type: "gesture",
      slot: slots.send(T2),
      name: "send",
      thread: T2,
      args: {},
      allowed: true,
      commandId: "c1",
      generation: "gen-1",
    });
    const view = render(<StatusBar state={sim.state} cmds={s} />);
    expect(shown()).toBeNull();
    // The thread vanishes (404): its selection and composer go.
    s = step(s, { type: "context", ctx: { ...context(sim.state), gone: { [T2]: true } } });
    const attempt = s.slots[slots.send(T2)]!.attempt;
    s = step(s, {
      type: "reply",
      attempt,
      reply: { kind: "ok", status: 200, result: { input_id: "i", state: "queued", reject_reason: null } },
    });
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    expect(shown()).toBe("claude /work/p2: Send: queued.");
    expect(announced()).toBe("claude /work/p2: Send: queued.");
  });

  test("a background banner Ready's result is visible and announced", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: T2 });
    sim.quiesce();
    let s = step(initialCmdState(), { type: "context", ctx: context(sim.state) });
    const view = render(<StatusBar state={sim.state} cmds={s} />);
    s = step(s, {
      type: "gesture",
      slot: slots.ready(T1),
      name: "ready",
      thread: T1,
      args: { thread_id: T1, expected_launch_id: "l" },
      allowed: true,
      commandId: "c1",
      generation: "gen-1",
    });
    s = step(s, { type: "reply", attempt: s.slots[slots.ready(T1)]!.attempt, reply: { kind: "settled", status: 409, code: "launch_changed" } });
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    expect(shown()).toMatch(/^codex \/work\/p1: Mark ready: Refused: the thread's launch changed/);
    expect(announced()).toBe(shown());
  });

  test("a refused send is announced", () => {
    const sim = seeded();
    sim.quiesce();
    let s = step(initialCmdState(), { type: "context", ctx: context(sim.state) });
    const view = render(<StatusBar state={sim.state} cmds={s} />);
    s = step(s, {
      type: "gesture",
      slot: slots.send(T2),
      name: "send",
      thread: T2,
      args: {},
      allowed: true,
      commandId: "c1",
      generation: "gen-1",
    });
    act(() => view.rerender(<StatusBar state={sim.state} cmds={s} />));
    expect(announced()).toBe("claude /work/p2: Write a message first.");
  });

  test("the banner renders awaiting_approval with no action", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: T2 });
    sim.quiesce();
    sim.server.status(T2, "turn", "awaiting_approval");
    sim.quiesce();
    render(<AttentionBanner state={sim.state} cmds={initialCmdState()} ops={{} as Ops} />);
    const banner = screen.getByRole("region", { name: "Attention" });
    const entry = banner.querySelector('[data-cause="awaiting_approval"]');
    expect(entry?.textContent).toBe("Awaiting an approval in the CLI");
    expect(entry?.querySelectorAll("button")).toHaveLength(0);
  });
});
