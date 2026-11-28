// @vitest-environment jsdom
// W2-4 `composer.keys` (docs/design/w2-composer.md §2.1): each composing guard alone blocks
// submission, the 50 ms window's edges, Shift+Enter, the shortcuts, `repeat`, and the
// pointer rule; then the component: the flag and timestamp reset on blur and on a thread
// switch, a blocked Enter is not prevented, a repeat is.

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, test, vi } from "vitest";

import { type CmdState, initialCmdState } from "../commands/reducer";
import { threadId } from "../sync/model/server";
import { seeded } from "../sync/ops";
import { Composer } from "./Composer";
import { decide, type KeyFacts } from "./keys";
import type { Ops } from "./ops";

afterEach(cleanup);

const BASE: KeyFacts = {
  key: "Enter",
  repeat: false,
  shift: false,
  ctrlOrMeta: false,
  isComposing: false,
  keyCode: 13,
  composingFlag: false,
  timeStamp: 1000,
  lastCompositionEnd: null,
  finePointer: true,
};

describe("composer.keys", () => {
  test.each<[string, Partial<KeyFacts>, ReturnType<typeof decide>]>([
    ["plain Enter, fine pointer", {}, "submit"],
    ["isComposing only", { isComposing: true }, "ime"],
    ["keyCode 229 only", { keyCode: 229 }, "ime"],
    ["composing flag only", { composingFlag: true }, "ime"],
    ["window only (10 ms after compositionend)", { lastCompositionEnd: 990 }, "ime"],
    ["49 ms after compositionend", { lastCompositionEnd: 951 }, "ime"],
    ["50 ms after compositionend", { lastCompositionEnd: 950 }, "submit"],
    ["inverted timestamp (keydown 10 ms before compositionend)", { lastCompositionEnd: 1010 }, "ime"],
    ["the window is absolute: 50 ms before compositionend", { lastCompositionEnd: 1050 }, "submit"],
    ["no compositionend with a small timeStamp", { timeStamp: 5, lastCompositionEnd: null }, "submit"],
    ["each guard blocks Ctrl+Enter too", { ctrlOrMeta: true, isComposing: true }, "ime"],
    ["229 blocks Cmd+Enter", { ctrlOrMeta: true, keyCode: 229 }, "ime"],
    ["Shift+Enter", { shift: true }, "newline"],
    ["Shift+Enter repeat stays native", { shift: true, repeat: true }, "newline"],
    ["Ctrl/Cmd+Enter on a coarse pointer", { ctrlOrMeta: true, finePointer: false }, "submit"],
    ["plain Enter on a coarse pointer", { finePointer: false }, "newline"],
    ["repeat after the window", { repeat: true, lastCompositionEnd: 900 }, "ignore"],
    ["repeat of Ctrl+Enter", { repeat: true, ctrlOrMeta: true }, "ignore"],
    ["another key", { key: "a" }, "newline"],
  ])("%s", (_, facts, want) => {
    expect(decide({ ...BASE, ...facts })).toBe(want);
  });
});

// ---- the component -----------------------------------------------------------------------

const T1 = threadId(2);

function setup(text = "こんにちは") {
  const sim = seeded();
  sim.server.status(T1, "process", "running");
  sim.dispatch({ type: "select", thread: T1 });
  sim.quiesce();
  const cmds: CmdState = { ...initialCmdState(0), drafts: { [T1]: { text, revision: 1 } } };
  const send = vi.fn();
  const ops = { edit: vi.fn(), send } as unknown as Ops;
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: q === "(pointer: fine)" }));
  const view = render(<Composer state={sim.state} cmds={cmds} thread={T1} ops={ops} />);
  const box = screen.getByLabelText("Message");
  return { sim, cmds, ops, send, view, box };
}

/** Fires a keydown; returns whether it was not prevented. */
function key(el: Element, init: KeyboardEventInit & { keyCode?: number; timeStamp?: number }) {
  const e = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true, ...init });
  if (init.keyCode !== undefined) Object.defineProperty(e, "keyCode", { value: init.keyCode });
  if (init.timeStamp !== undefined) Object.defineProperty(e, "timeStamp", { value: init.timeStamp });
  return el.dispatchEvent(e);
}

function compositionEnd(el: Element, timeStamp: number) {
  const e = new CompositionEvent("compositionend", { bubbles: true, data: "日本" });
  Object.defineProperty(e, "timeStamp", { value: timeStamp });
  el.dispatchEvent(e);
}

describe("composer", () => {
  afterEach(() => vi.unstubAllGlobals());

  test("a composing Enter is neither sent nor prevented; a later Enter is sent and prevented", () => {
    const { box, send } = setup();
    fireEvent.compositionStart(box);
    expect(key(box, {})).toBe(true);
    compositionEnd(box, 1000);
    expect(key(box, { timeStamp: 1020 })).toBe(true);
    expect(send).not.toHaveBeenCalled();
    expect(key(box, { timeStamp: 1100 })).toBe(false);
    expect(send).toHaveBeenCalledTimes(1);
  });

  test("isComposing alone, through the component, holds the Enter without preventing it", () => {
    const { box, send } = setup();
    expect(key(box, { isComposing: true })).toBe(true);
    expect(key(box, { isComposing: true, ctrlKey: true })).toBe(true);
    expect(send).not.toHaveBeenCalled();
  });

  test("Ctrl+Enter while the send slot is held calls no send", () => {
    const sim = seeded();
    sim.server.status(T1, "process", "running");
    sim.dispatch({ type: "select", thread: T1 });
    sim.quiesce();
    const held = {
      attempt: 1,
      slot: `send:${T1}`,
      name: "send",
      commandId: "c",
      body: "{}",
      generation: "g",
      session: "s",
      thread: T1,
      text: "x",
      revision: 1,
      phase: "inflight",
      reply: null,
      retryAt: null,
      stale: false,
      posts: 1,
    } as const;
    const cmds: CmdState = {
      ...initialCmdState(0),
      drafts: { [T1]: { text: "next", revision: 2 } },
      slots: { [held.slot]: held },
    };
    const send = vi.fn();
    vi.stubGlobal("matchMedia", (q: string) => ({ matches: q === "(pointer: fine)" }));
    render(<Composer state={sim.state} cmds={cmds} thread={T1} ops={{ edit: vi.fn(), send } as unknown as Ops} />);
    const box = screen.getByLabelText("Message");
    expect(key(box, { ctrlKey: true })).toBe(false);
    expect(key(box, {})).toBe(false);
    expect(send).not.toHaveBeenCalled();
  });

  test("blur resets the flag and the timestamp", () => {
    const { box, send } = setup();
    fireEvent.compositionStart(box);
    fireEvent.blur(box);
    expect(key(box, { timeStamp: 5000 })).toBe(false);
    expect(send).toHaveBeenCalledTimes(1);
    compositionEnd(box, 6000);
    fireEvent.blur(box);
    key(box, { timeStamp: 6010 });
    expect(send).toHaveBeenCalledTimes(2);
  });

  test("a thread switch (a new composer) resets them", () => {
    const { box, send, view, sim, cmds, ops } = setup();
    fireEvent.compositionStart(box);
    view.rerender(<Composer key="other" state={sim.state} cmds={cmds} thread={T1} ops={ops} />);
    key(screen.getByLabelText("Message"), { timeStamp: 10 });
    expect(send).toHaveBeenCalledTimes(1);
  });

  test("a repeating submit shortcut is prevented and sends nothing", () => {
    const { box, send } = setup();
    expect(key(box, { repeat: true })).toBe(false);
    expect(key(box, { repeat: true, ctrlKey: true })).toBe(false);
    expect(send).not.toHaveBeenCalled();
  });

  test("on a coarse pointer plain Enter is left to the browser", () => {
    const { box, send } = setup();
    vi.stubGlobal("matchMedia", () => ({ matches: false }));
    expect(key(box, {})).toBe(true);
    expect(send).not.toHaveBeenCalled();
    expect(key(box, { metaKey: true })).toBe(false);
    expect(send).toHaveBeenCalledTimes(1);
  });

  test("an invalid draft is marked aria-invalid with a described error", () => {
    const sim = seeded();
    const cmds: CmdState = { ...initialCmdState(0), drafts: { [T1]: { text: "", revision: 1 } }, invalid: { [T1]: "empty" } };
    render(<Composer state={sim.state} cmds={cmds} thread={T1} ops={{} as Ops} />);
    const box = screen.getByLabelText("Message");
    expect(box.getAttribute("aria-invalid")).toBe("true");
    const ids = (box.getAttribute("aria-describedby") ?? "").split(" ");
    expect(ids.map((i) => document.getElementById(i)?.textContent).join(" ")).toMatch(/Write a message first/);
  });
});
