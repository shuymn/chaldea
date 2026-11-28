// W2-5, W2-6, W2-10, W2-11 (docs/design/w2-composer.md §2.1, §2.2, §4): the reply table
// (`commands.classify`), the command reducer under any interleaving (`commands.retry`), one
// command per slot (`commands.slots`), the draft rule, the availability map
// (`commands.map`), and the locks (`commands.locked`).

import fc from "fast-check";
import { describe, expect, test } from "vitest";

import { threadId } from "../sync/model/server";
import { seeded } from "../sync/ops";
import type { Sim } from "../sync/sim";
import { threadStream } from "../sync/state";
import { type CommandName, classify, type Reply } from "./client";
import {
  canCancel,
  canResolve,
  canSend,
  canSpawn,
  context,
  hintLaunch,
  lockReason,
  readyLaunch,
  resumeLaunch,
  slots,
  stopLaunch,
} from "./controls";
import { commandText } from "./messages";
import {
  BUSY,
  type CmdAction,
  type CmdEffect,
  type CmdState,
  type Context,
  draftOf,
  type Gesture,
  initialCmdState,
  MAX_PROMPT_BYTES,
  reduceCommands,
  retryable,
} from "./reducer";

type Input = CmdAction extends infer A ? (A extends unknown ? Omit<A, "now"> : never) : never;

const T1 = threadId(1);
const T2 = threadId(2);
const S1: Context = { session: "1|e1|e1|g1", gone: {} };

/** A command reducer driven by hand, with the effects it asked for. */
class Cmds {
  s: CmdState = initialCmdState(0);
  now = 0;
  readonly posts: Extract<CmdEffect, { type: "post" }>[] = [];
  readonly effects: CmdEffect[] = [];
  constructor(ctx: Context = S1) {
    this.do({ type: "context", ctx });
  }
  do(a: Input): CmdEffect[] {
    const [s, effects] = reduceCommands(this.s, { ...a, now: this.now } as CmdAction);
    this.s = s;
    this.effects.push(...effects);
    for (const e of effects) if (e.type === "post") this.posts.push(e);
    return effects;
  }
  gesture(g: Partial<Gesture> & Pick<Gesture, "slot" | "name">): CmdEffect[] {
    return this.do({
      type: "gesture",
      thread: null,
      args: {},
      allowed: true,
      commandId: `cmd-${this.posts.length + 1}-${g.slot}`,
      generation: "g1",
      ...g,
    });
  }
  send(thread: string, text?: string): CmdEffect[] {
    if (text !== undefined) this.do({ type: "edit", thread, text });
    return this.gesture({ slot: slots.send(thread), name: "send", thread });
  }
  reply(reply: Reply, attempt = this.posts.at(-1)?.attempt ?? 0): CmdEffect[] {
    return this.do({ type: "reply", attempt, reply });
  }
  slot(key: string) {
    return this.s.slots[key];
  }
}

const QUEUED: Reply = { kind: "ok", status: 200, result: { input_id: "i1", state: "queued", reject_reason: null } };
const UNKNOWN: Reply = { kind: "unknown", status: 503, code: "outcome_unknown" };
const BUSY_1: Reply = { kind: "not_done", status: 503, code: "busy", retryAfter: 1 };

describe("commands.classify", () => {
  const body = (code: string, extra = "") => `{"code":"${code}"${extra}}`;
  const rows: [number, string | null, Reply["kind"], CommandName?][] = [
    [200, '{"input_id":"0193abcd-ef01-7000-8000-000000000001","state":"queued","reject_reason":null}', "ok", "send"],
    [200, '{"input_id":"0193abcd-ef01-7000-8000-000000000001","state":"rejected","reject_reason":"lossy_text"}', "ok", "send"],
    [200, '{"code":"busy"}', "unreadable", "send"],
    [200, "not json", "unreadable", "send"],
    [200, null, "unreadable", "stop"],
    [400, body("bad_args"), "settled"],
    [400, null, "settled"],
    [403, body("forbidden"), "settled"],
    [404, body("not_found"), "settled"],
    [413, body("payload_too_large"), "settled"],
    [501, body("not_implemented"), "settled"],
    [409, body("conflict"), "settled"],
    [409, body("unknown_after_crash"), "settled"],
    [409, body("death_unconfirmed"), "settled"],
    [504, body("launch_busy"), "settled"],
    [409, body("in_progress", ',"retry_after":1'), "unknown"],
    [503, body("outcome_unknown", ',"retry_after":1'), "unknown"],
    [503, "garbage", "unknown"],
    [503, null, "unknown"],
    [500, body("internal"), "unknown"],
    [502, null, "unknown"],
    [408, body("request_timeout"), "not_done"],
    [503, body("busy", ',"retry_after":2'), "not_done"],
    [503, body("shutting_down"), "not_done"],
    [503, body("disk_full"), "not_done"],
    [503, body("store_failed"), "not_done"],
    [401, body("unauthorized"), "unauthorized"],
    [401, null, "unauthorized"],
  ];
  test.each(rows)("%i %s → %s", (status, text, kind, name = "send") => {
    expect(classify(name, status, text).kind).toBe(kind);
  });

  test("a not-done reply carries its retry_after; 401 offers no Retry", () => {
    expect(classify("send", 503, '{"code":"busy","retry_after":2}')).toEqual({
      kind: "not_done",
      status: 503,
      code: "busy",
      retryAfter: 2,
    });
    const c = new Cmds();
    c.send(T1, "x");
    c.reply({ kind: "unauthorized" });
    expect(c.slot(slots.send(T1))?.phase).toBe("released");
    c.now = 60_000;
    c.do({ type: "tick" });
    expect(retryable(c.s, slots.send(T1))).toBe(false);
    expect(c.effects).toContainEqual({ type: "unauthorized", generation: "g1" });
  });
});

describe("drafts", () => {
  test("queued clears the draft only while it is still the submitted revision", () => {
    const c = new Cmds();
    c.send(T1, "最初の文");
    c.reply(QUEUED);
    expect(draftOf(c.s, T1).text).toBe("");

    // Edited during the send, then back to the same text: a new revision, kept.
    c.send(T1, "二番目");
    c.do({ type: "edit", thread: T1, text: "二番目!" });
    c.do({ type: "edit", thread: T1, text: "二番目" });
    c.reply(QUEUED);
    expect(draftOf(c.s, T1).text).toBe("二番目");
  });

  test("rejected, unknown, not done, and Forget keep the draft", () => {
    for (const reply of [
      { kind: "ok", status: 200, result: { input_id: "i", state: "rejected", reject_reason: "lossy_text" } } as Reply,
      UNKNOWN,
      BUSY_1,
      { kind: "settled", status: 400, code: "bad_args" } as Reply,
    ]) {
      const c = new Cmds();
      c.send(T1, "!ls");
      c.reply(reply);
      c.do({ type: "forget", slot: slots.send(T1) });
      expect(draftOf(c.s, T1).text, reply.kind).toBe("!ls");
    }
  });

  test("Forget on an unknown outcome warns that resending is a new input; on not done it releases silently", () => {
    const c = new Cmds();
    c.send(T1, "a");
    c.reply(UNKNOWN);
    c.do({ type: "forget", slot: slots.send(T1) });
    const forgotten = c.slot(slots.send(T1));
    expect(forgotten?.phase).toBe("forgotten");
    expect(commandText(forgotten!)).toMatch(/new command/);
    c.send(T1);
    c.reply(BUSY_1);
    c.do({ type: "forget", slot: slots.send(T1) });
    expect(c.slot(slots.send(T1))).toBeUndefined();
  });

  test("an empty or oversized draft sends nothing and says why", () => {
    const c = new Cmds();
    c.send(T1, "");
    expect(c.s.invalid[T1]).toBe("empty");
    const big = "あ".repeat(Math.floor(MAX_PROMPT_BYTES / 3) + 1);
    c.send(T1, big);
    expect(c.s.invalid[T1]).toBe("too_large");
    expect(c.posts).toEqual([]);
    expect(draftOf(c.s, T1).text).toBe(big);
    // Exactly the limit goes.
    c.send(T1, "a".repeat(MAX_PROMPT_BYTES));
    expect(c.posts).toHaveLength(1);
    expect(c.s.invalid[T1]).toBeUndefined();
  });

  test("the body is the value unchanged, with origin pwa", () => {
    const c = new Cmds();
    const text = "  日本語 🎌\r\nが゙ ";
    c.send(T1, text);
    expect(JSON.parse(c.posts[0]!.body)).toEqual({
      command_id: `cmd-1-${slots.send(T1)}`,
      args: { thread_id: T1, text, origin: "pwa" },
    });
  });
});

describe("commands.slots", () => {
  test("one command per slot: a second gesture, Retry, or the banner's twin does nothing while busy", () => {
    const c = new Cmds();
    c.send(T1, "a");
    c.send(T1, "b");
    expect(c.posts).toHaveLength(1);
    // The input slot is shared by the row and the banner.
    c.gesture({ slot: slots.input("i1"), name: "input_resolve", thread: T1, args: { input_id: "i1", delivered: true } });
    c.gesture({ slot: slots.input("i1"), name: "input_resolve", thread: T1, args: { input_id: "i1", delivered: false } });
    expect(c.posts).toHaveLength(2);
    // Ready and Stop are separate slots: an unknown Ready does not block Stop.
    c.gesture({ slot: slots.ready(T1), name: "ready", thread: T1 });
    c.reply(UNKNOWN);
    c.gesture({ slot: slots.launch(T1), name: "stop", thread: T1 });
    expect(c.posts.map((p) => p.name)).toEqual(["send", "input_resolve", "ready", "stop"]);
    // An unknown slot stays busy until Forget.
    c.gesture({ slot: slots.ready(T1), name: "ready", thread: T1 });
    expect(c.posts).toHaveLength(4);
    c.do({ type: "forget", slot: slots.ready(T1) });
    c.gesture({ slot: slots.ready(T1), name: "ready", thread: T1 });
    expect(c.posts).toHaveLength(5);
  });

  test("a gesture the gate refuses sends nothing", () => {
    const c = new Cmds();
    c.do({ type: "edit", thread: T1, text: "x" });
    c.gesture({ slot: slots.send(T1), name: "send", thread: T1, allowed: false });
    c.gesture({ slot: slots.spawn, name: "spawn", allowed: false });
    expect(c.posts).toEqual([]);
    const off = new Cmds({ session: null, gone: {} });
    off.send(T1, "x");
    expect(off.posts).toEqual([]);
  });
});

describe("commands.retry (unit)", () => {
  test("not done: Retry after retry_after, with the frozen bytes; a reopen releases it", () => {
    const c = new Cmds();
    c.send(T1, "frozen");
    c.reply(BUSY_1);
    expect(retryable(c.s, slots.send(T1))).toBe(false);
    expect(c.effects.at(-1)).toEqual({ type: "wake", at: 1000 });
    c.now = 1000;
    c.do({ type: "tick" });
    expect(retryable(c.s, slots.send(T1))).toBe(true);
    c.do({ type: "edit", thread: T1, text: "changed" });
    c.do({ type: "retry", slot: slots.send(T1), allowed: true });
    expect(c.posts).toHaveLength(2);
    expect(c.posts[1]!.body).toBe(c.posts[0]!.body);
    c.reply(BUSY_1);
    c.now = 5000;
    c.do({ type: "context", ctx: { session: "2|e1|e1|g1", gone: {} } });
    expect(c.slot(slots.send(T1))?.phase).toBe("released");
    c.do({ type: "retry", slot: slots.send(T1), allowed: true });
    expect(c.posts).toHaveLength(2);
  });

  test("an ambiguous reply never offers Retry", () => {
    for (const r of [UNKNOWN, { kind: "unknown", status: null, code: null } as Reply]) {
      const c = new Cmds();
      c.send(T1, "a");
      c.reply(r);
      c.now = 600_000;
      c.do({ type: "tick" });
      expect(c.slot(slots.send(T1))?.phase).toBe("check_thread");
      c.do({ type: "retry", slot: slots.send(T1), allowed: true });
      expect(c.posts).toHaveLength(1);
    }
  });

  test("a reply in flight across a reopen: ambiguous goes to check the thread; queued keeps the draft", () => {
    const a = new Cmds();
    a.send(T1, "a");
    a.do({ type: "context", ctx: { session: "2|e1|e1|g1", gone: {} } });
    a.reply(UNKNOWN);
    expect(a.slot(slots.send(T1))?.phase).toBe("check_thread");

    const b = new Cmds();
    b.send(T1, "keep me");
    b.do({ type: "context", ctx: { session: "2|e1|e1|g1", gone: {} } });
    // Single flight: a newer Send waits for the old slot.
    b.send(T1);
    expect(b.posts).toHaveLength(1);
    b.reply(QUEUED);
    expect(b.slot(slots.send(T1))?.phase).toBe("settled");
    expect(draftOf(b.s, T1).text).toBe("keep me");
    // Not done after the reopen: released, no Retry.
    b.send(T1);
    b.do({ type: "context", ctx: { session: "3|e1|e1|g1", gone: {} } });
    b.reply(BUSY_1);
    expect(b.slot(slots.send(T1))?.phase).toBe("released");
  });

  test("a thread 404 invalidates the target in the same socket session (review r4)", () => {
    const c = new Cmds();
    c.send(T1, "draft");
    c.do({ type: "context", ctx: { session: S1.session, gone: { [T1]: true } } });
    c.reply(QUEUED);
    expect(c.slot(slots.send(T1))?.phase).toBe("settled");
    expect(draftOf(c.s, T1).text).toBe("draft");
    // A late not-done cannot restore Retry, even once the 404 is forgotten (epoch reset).
    c.send(T1);
    c.do({ type: "context", ctx: { session: S1.session, gone: { [T1]: true } } });
    c.reply(BUSY_1);
    c.do({ type: "context", ctx: { session: S1.session, gone: {} } });
    c.now = 10_000;
    c.do({ type: "tick" });
    expect(c.slot(slots.send(T1))?.phase).toBe("released");
    expect(retryable(c.s, slots.send(T1))).toBe(false);
  });

  test("a stale spawn reply does not move the selection; a fresh one does", () => {
    const ok: Reply = {
      kind: "ok",
      status: 200,
      result: { thread_id: T2, launch_id: "l", input_id: null, outcome: "started" },
    };
    const c = new Cmds();
    c.gesture({ slot: slots.spawn, name: "spawn", args: { agent: "claude", cwd: "/work/a" } });
    c.reply(ok);
    expect(c.effects).toContainEqual({ type: "select", thread: T2 });
    const d = new Cmds();
    d.gesture({ slot: slots.spawn, name: "spawn", args: { agent: "claude", cwd: "/work/a" } });
    d.do({ type: "context", ctx: { session: "9|e2|e2|g1", gone: {} } });
    d.reply(ok);
    expect(d.effects.some((e) => e.type === "select")).toBe(false);
    expect(d.slot(slots.spawn)?.phase).toBe("settled");
  });
});

// ---- commands.retry: any interleaving --------------------------------------------------

const REPLIES: Reply[] = [
  { kind: "unreadable", status: 200 },
  { kind: "settled", status: 400, code: "bad_args" },
  { kind: "settled", status: 409, code: "conflict" },
  { kind: "settled", status: 409, code: "unknown_after_crash" },
  { kind: "settled", status: 501, code: "not_implemented" },
  { kind: "settled", status: 409, code: "death_unconfirmed" },
  { kind: "unknown", status: 409, code: "in_progress" },
  { kind: "unknown", status: 503, code: "outcome_unknown" },
  { kind: "unknown", status: 503, code: null },
  { kind: "unknown", status: 500, code: "internal" },
  { kind: "unknown", status: null, code: null },
  { kind: "not_done", status: 408, code: "request_timeout", retryAfter: 0 },
  { kind: "not_done", status: 503, code: "busy", retryAfter: 1 },
  { kind: "not_done", status: 503, code: "shutting_down", retryAfter: 0 },
  { kind: "not_done", status: 503, code: "disk_full", retryAfter: 0 },
  { kind: "not_done", status: 503, code: "store_failed", retryAfter: 0 },
  { kind: "unauthorized" },
  { kind: "not_sent" },
];

function okFor(name: CommandName, queued: boolean): Reply {
  switch (name) {
    case "send":
      return {
        kind: "ok",
        status: 200,
        result: { input_id: "i", state: queued ? "queued" : "rejected", reject_reason: queued ? null : "tab" },
      };
    case "spawn":
      return { kind: "ok", status: 200, result: { thread_id: T2, launch_id: "l", input_id: null, outcome: "started" } };
    case "stop":
      return {
        kind: "ok",
        status: 200,
        result: {
          launch_id: "l",
          outcome: "stopped",
          death_confirmed: queued,
          confirmed_within_deadline: queued,
          exit_code: null,
          signal: null,
        },
      };
    case "ready":
      return { kind: "ok", status: 200, result: { launch_id: "l" } };
    default:
      return { kind: "ok", status: 200, result: { input_id: "i", state: "acked" } };
  }
}

type Op =
  | { t: "edit"; th: number; text: string }
  | { t: "gesture"; k: number; th: number; allowed: boolean }
  | { t: "retry"; k: number; th: number; allowed: boolean }
  | { t: "forget"; k: number; th: number }
  | { t: "reply"; i: number; row: number; ok: boolean }
  | { t: "reopen" }
  | { t: "epoch" }
  | { t: "generation" }
  | { t: "gone"; th: number }
  | { t: "advance"; ms: number };

const KINDS: { name: CommandName; slot: (t: string) => string }[] = [
  { name: "send", slot: slots.send },
  { name: "ready", slot: slots.ready },
  { name: "stop", slot: slots.launch },
  { name: "resume", slot: slots.launch },
  { name: "input_resolve", slot: () => slots.input("i1") },
  { name: "input_cancel", slot: () => slots.input("i1") },
  { name: "spawn", slot: () => slots.spawn },
];
const THREADS = [T1, T2];

const opArb: fc.Arbitrary<Op> = fc.oneof(
  fc.record({ t: fc.constant("edit" as const), th: fc.nat(1), text: fc.constantFrom("", "a", "日本語🎌", "b") }),
  fc.record({ t: fc.constant("gesture" as const), k: fc.nat(KINDS.length - 1), th: fc.nat(1), allowed: fc.boolean() }),
  fc.record({ t: fc.constant("retry" as const), k: fc.nat(KINDS.length - 1), th: fc.nat(1), allowed: fc.boolean() }),
  fc.record({ t: fc.constant("forget" as const), k: fc.nat(KINDS.length - 1), th: fc.nat(1) }),
  fc.record({ t: fc.constant("reply" as const), i: fc.nat(5), row: fc.nat(REPLIES.length + 1), ok: fc.boolean() }),
  fc.record({ t: fc.constant("reply" as const), i: fc.nat(5), row: fc.nat(REPLIES.length + 1), ok: fc.boolean() }),
  fc.constant({ t: "reopen" as const }),
  fc.constant({ t: "epoch" as const }),
  fc.constant({ t: "generation" as const }),
  fc.record({ t: fc.constant("gone" as const), th: fc.nat(1) }),
  fc.record({ t: fc.constant("advance" as const), ms: fc.constantFrom(100, 1000, 5000) }),
);

describe("commands.retry", () => {
  test("any interleaving of gestures, replies, reopens, epochs, 404s, and generations", () => {
    fc.assert(
      fc.property(fc.array(opArb, { maxLength: 60 }), (ops) => {
        let sock = 1;
        let epoch = 1;
        let gen = 1;
        let gone: Record<string, true> = {};
        const ctx = (): Context => ({ session: `${sock}|e${epoch}|e${epoch}|g${gen}`, gone });
        const c = new Cmds(ctx());
        /** Attempts posted and not yet answered. */
        const open: { attempt: number; slot: string; name: CommandName }[] = [];
        /** The frozen body of each command id. */
        const bodies = new Map<string, string>();
        let n = 0;
        for (const op of ops) {
          const before = c.s;
          const posted = c.posts.length;
          switch (op.t) {
            case "edit":
              c.do({ type: "edit", thread: THREADS[op.th]!, text: op.text });
              break;
            case "gesture": {
              const k = KINDS[op.k]!;
              const th = THREADS[op.th]!;
              c.do({
                type: "gesture",
                slot: k.slot(th),
                name: k.name,
                thread: k.name === "spawn" ? null : th,
                args: {},
                allowed: op.allowed,
                commandId: `id-${n++}`,
                generation: `g${gen}`,
              });
              break;
            }
            case "retry":
              c.do({ type: "retry", slot: KINDS[op.k]!.slot(THREADS[op.th]!), allowed: op.allowed });
              break;
            case "forget":
              c.do({ type: "forget", slot: KINDS[op.k]!.slot(THREADS[op.th]!) });
              break;
            case "reply": {
              const o = open[op.i % Math.max(1, open.length)];
              if (!o) break;
              open.splice(open.indexOf(o), 1);
              const cmd = Object.values(c.s.slots).find((x) => x.attempt === o.attempt)!;
              const fresh = !cmd.stale && cmd.session === ctx().session && (cmd.thread === null || !gone[cmd.thread]);
              const reply = op.row >= REPLIES.length ? okFor(o.name, op.ok) : REPLIES[op.row]!;
              const others = Object.entries(c.s.slots).filter(([k]) => k !== o.slot);
              c.reply(reply, o.attempt);
              const after = c.s.slots[o.slot]!;
              // The reply changed only its own slot.
              expect(Object.entries(c.s.slots).filter(([k]) => k !== o.slot)).toEqual(others);
              // An ambiguous reply never yields Retry.
              if (reply.kind === "unknown") expect(after.phase).toBe("check_thread");
              if (after.phase === "not_done") expect(reply.kind).toBe("not_done");
              // A reply from an old session or target never clears a draft or moves selection.
              if (!fresh) {
                expect(c.s.drafts).toBe(before.drafts);
                expect(after.phase).not.toBe("not_done");
              }
              break;
            }
            case "reopen":
              sock += 1;
              c.do({ type: "context", ctx: ctx() });
              break;
            case "epoch":
              sock += 1;
              epoch += 1;
              gone = {};
              c.do({ type: "context", ctx: ctx() });
              break;
            case "generation":
              gen += 1;
              c.do({ type: "context", ctx: ctx() });
              break;
            case "gone":
              gone = { ...gone, [THREADS[op.th]!]: true };
              c.do({ type: "context", ctx: ctx() });
              break;
            case "advance":
              c.now += op.ms;
              c.do({ type: "tick" });
              break;
          }
          const newPosts = c.posts.slice(posted);
          // Each POST follows a gesture or a Retry, one at most.
          if (op.t !== "gesture" && op.t !== "retry") expect(newPosts).toEqual([]);
          expect(newPosts.length).toBeLessThanOrEqual(1);
          for (const p of newPosts) {
            const cmd = Object.values(c.s.slots).find((x) => x.attempt === p.attempt)!;
            if (op.t === "retry") {
              // Only a not-done attempt in its current session, after retry_after, with the frozen bytes.
              const prev = before.slots[cmd.slot]!;
              expect(prev.phase).toBe("not_done");
              expect(prev.stale).toBe(false);
              expect(prev.session).toBe(ctx().session);
              expect(before.now).toBeGreaterThanOrEqual(prev.retryAt!);
              expect(p.body).toBe(bodies.get(cmd.commandId));
            } else {
              expect(bodies.has(cmd.commandId)).toBe(false);
              bodies.set(cmd.commandId, p.body);
              const prev = before.slots[cmd.slot];
              expect(prev === undefined || !BUSY.has(prev.phase)).toBe(true);
            }
            open.push({ attempt: p.attempt, slot: cmd.slot, name: cmd.name });
          }
          // Context changes and ticks settle nothing: only not-done is released.
          if (op.t === "reopen" || op.t === "epoch" || op.t === "generation" || op.t === "gone" || op.t === "advance") {
            for (const [k, prev] of Object.entries(before.slots)) {
              const now = c.s.slots[k]!;
              if (now.phase !== prev.phase) expect([prev.phase, now.phase]).toEqual(["not_done", "released"]);
            }
          }
          // One attempt in flight per slot.
          const slotsInFlight = open.map((o) => o.slot);
          expect(new Set(slotsInFlight).size).toBe(slotsInFlight.length);
        }
      }),
      { numRuns: 2000 },
    );
  });
});

// ---- commands.map and commands.locked -----------------------------------------------------

const L1 = "0193cccc-0000-7000-8000-000000000001";
const I1 = "0193dddd-0000-7000-8000-000000000001";

function shown(): Sim {
  const sim = seeded();
  sim.server.status(T2, "process", "running");
  sim.server.launchReserved(T2, L1);
  sim.dispatch({ type: "select", thread: T2 });
  sim.quiesce();
  return sim;
}

describe("commands.map", () => {
  test("each control's availability follows the copy: process, launch, inputs, closed", () => {
    const sim = shown();
    let s = sim.state;
    expect([canSend(s, T2), readyLaunch(s, T2), stopLaunch(s, T2), resumeLaunch(s, T2), hintLaunch(s, T2)]).toEqual([
      true,
      L1,
      L1,
      null,
      null,
    ]);
    expect(canSpawn(s)).toBe(true);
    sim.server.status(T2, "process", "starting");
    sim.quiesce();
    s = sim.state;
    expect([canSend(s, T2), readyLaunch(s, T2)]).toEqual([true, null]);
    sim.server.inputCreated(T2, I1, "x");
    sim.quiesce();
    expect([canCancel(sim.state, T2, I1), canResolve(sim.state, T2, I1)]).toEqual([true, false]);
    sim.server.inputState(T2, I1, "unconfirmed", { kind: "launch_exited" });
    // An exit whose death is not confirmed: no Stop, no Resume, the hint.
    sim.server.status(T2, "process", "exited");
    sim.server.launchExited(T2, "daemon_restart", false);
    sim.quiesce();
    s = sim.state;
    expect([canCancel(s, T2, I1), canResolve(s, T2, I1)]).toEqual([false, true]);
    expect([canSend(s, T2), stopLaunch(s, T2), resumeLaunch(s, T2), hintLaunch(s, T2)]).toEqual([false, null, null, L1]);
    sim.server.deathConfirmed(T2);
    sim.quiesce();
    s = sim.state;
    expect([resumeLaunch(s, T2), hintLaunch(s, T2)]).toEqual([L1, null]);
    // Closed: nothing but Spawn.
    sim.server.close(T2);
    sim.quiesce();
    s = sim.state;
    expect([canSend(s, T2), readyLaunch(s, T2), stopLaunch(s, T2), resumeLaunch(s, T2), hintLaunch(s, T2)]).toEqual([
      false,
      null,
      null,
      null,
      null,
    ]);
    expect(canSpawn(s)).toBe(true);
  });

  test("a snapshot with an exited launch and death unconfirmed shows the hint", () => {
    const sim = shown();
    sim.server.launchExited(T2, "daemon_restart", false);
    for (let k = 0; k < 7; k++) sim.server.message(T2, 3);
    sim.restart();
    sim.quiesce();
    expect([resumeLaunch(sim.state, T2), hintLaunch(sim.state, T2)]).toEqual([null, L1]);
  });
});

describe("commands.locked", () => {
  test("outside live nothing is available; send, ready, and input commands also need a current copy", () => {
    const sim = shown();
    expect(lockReason(sim.state, T2)).toBeNull();
    // Down: not live.
    sim.restart();
    sim.up = false;
    sim.dispatch({ type: "tick" });
    expect(context(sim.state).session).toBeNull();
    for (const s of [sim.state]) {
      expect([canSend(s, T2), readyLaunch(s, T2), stopLaunch(s, T2), canSpawn(s)]).toEqual([false, null, null, false]);
    }
    sim.quiesce();
    expect(canSend(sim.state, T2)).toBe(true);

    // Live, but the thread's copy behind an announced head: send and ready wait, Stop does not.
    sim.server.message(T2, 3);
    sim.refresh();
    for (const id of [...sim.conns.keys()]) while (sim.socketStep(id));
    while (sim.deliverFrame());
    expect(sim.state.streams[threadStream(T2)]?.fwd).not.toBeNull();
    expect(lockReason(sim.state, T2)).not.toBeNull();
    expect([canSend(sim.state, T2), readyLaunch(sim.state, T2), stopLaunch(sim.state, T2)]).toEqual([false, null, L1]);
    sim.quiesce();
    expect(canSend(sim.state, T2)).toBe(true);
  });

  test("a copy in a new epoch while the old socket holds is not current", () => {
    const sim = shown();
    // The database is replaced under a socket that stays open with the old epoch.
    sim.server.swap([1, 2, 3]);
    sim.server.status(T2, "process", "running");
    sim.server.launchReserved(T2, L1);
    sim.dispatch({ type: "retry" });
    sim.dispatch({ type: "select", thread: null });
    sim.dispatch({ type: "select", thread: T2 });
    for (let k = 0; k < 6; k++) {
      for (const id of [...sim.flights.keys()]) sim.serve(id);
      while (sim.deliver());
    }
    expect(sim.state.epoch).toBe(sim.server.db.epoch);
    expect(sim.state.socket?.epoch).not.toBe(sim.state.epoch);
    expect(lockReason(sim.state, T2)).not.toBeNull();
    expect(canSend(sim.state, T2)).toBe(false);
  });
});
