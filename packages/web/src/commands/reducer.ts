// The commands' and drafts' pure reducer (docs/design/w2-composer.md §2.1, §2.2, D4),
// `(CmdState, CmdAction) → (CmdState, CmdEffect[])`, beside the sync reducer. It never reads
// the event stream: a command settles only on its own reply (D7), and the page shows input
// state from the stream.
//
// A slot is held while its command is in flight or its outcome is unknown, reserved
// synchronously at the gesture, so a second gesture finds it busy and is refused with a
// notice. Nothing retries automatically: an ambiguous reply shows "check the thread" with
// Forget only. A not-done command did nothing, so it frees its slot at once; its Retry,
// after `retry_after` and only while the attempt's socket session is current, reserves
// the slot again through the same busy check. A reply that arrives after its context
// changed updates only its own command's shown result.

import type { CommandName, Reply } from "./client";
import { commandText, INVALID_TEXT, refusedText } from "./messages";

export type Phase =
  /** An attempt is in flight. */
  | "inflight"
  /** The outcome is unknown: the owner checks the thread; only Forget is offered. */
  | "check_thread"
  /** Not done: the slot is free; Retry (after `retryAt`) while in session, and Forget. */
  | "not_done"
  /** Done: the reply is the result. */
  | "settled"
  /** Not done, and no longer retryable (its session ended, a 401, or nothing was sent). */
  | "released"
  /** An unknown outcome the owner forgot: a resend would be a new input. */
  | "forgotten";

/** Phases that hold their slot. */
export const BUSY: ReadonlySet<Phase> = new Set(["inflight", "check_thread"]);

/** Why a busy slot refuses a gesture, with the way out. */
function busyReason(c: Command): string {
  return c.phase === "check_thread"
    ? "check the thread first, or Forget the earlier one"
    : "the previous one is still pending";
}

/** The UTF-8 limit of a prompt (`core::input::MAX_PROMPT_BYTES`, mvp:186). */
export const MAX_PROMPT_BYTES = 256 * 1024;

export interface Command {
  /** This attempt's id; a Retry is a new attempt of the same command. */
  readonly attempt: number;
  readonly slot: string;
  readonly name: CommandName;
  /** Frozen at the gesture with the body bytes and the credential generation. */
  readonly commandId: string;
  readonly body: string;
  readonly generation: string;
  /** The socket session (socket, epochs, generation) the command was first sent in. */
  readonly session: string;
  /** The thread it acts on (null for `spawn`). */
  readonly thread: string | null;
  /** The thread as named at the gesture (agent, cwd, short id), frozen for its notices. */
  readonly label: string | null;
  /** A `send`'s frozen text and the draft revision it came from. */
  readonly text: string | null;
  readonly revision: number | null;
  readonly phase: Phase;
  readonly reply: Reply | null;
  /** When Retry may go (ms); null when not offered. */
  readonly retryAt: number | null;
  /** Its session or target ended: later replies change only its shown result. */
  readonly stale: boolean;
  /** POSTs made for this command, Retry included. */
  readonly posts: number;
}

export interface Draft {
  readonly text: string;
  /** Bumped on every edit. */
  readonly revision: number;
}

export type Invalid = "empty" | "too_large";

/** An outcome to announce and show, its text frozen when recorded. `seq` grows with each. */
export interface Notice {
  readonly seq: number;
  readonly text: string;
}

const prefixed = (label: string | null, text: string) => (label === null ? text : `${label}: ${text}`);

/** What the sync side says about the page's context, for session checks. */
export interface Context {
  /** `<socket id>|<socket epoch>|<copy epoch>|<generation>`; null without a `hello`. */
  readonly session: string | null;
  /** Threads a 404 removed in this epoch. */
  readonly gone: Readonly<Record<string, true>>;
}

export interface CmdState {
  readonly now: number;
  readonly drafts: Readonly<Record<string, Draft>>;
  readonly slots: Readonly<Record<string, Command>>;
  /** A refused send's reason per thread, until the draft changes. */
  readonly invalid: Readonly<Record<string, Invalid>>;
  readonly ctx: Context;
  readonly nextAttempt: number;
  readonly wakeAt: number | null;
  /**
   * The latest outcome to announce and show in the status bar, whatever is selected: a
   * command reply (by slot), or a refused send (by thread). `seq` grows with each.
   */
  readonly notice: Notice | null;
}

export function initialCmdState(now = 0): CmdState {
  return {
    now,
    drafts: {},
    slots: {},
    invalid: {},
    ctx: { session: null, gone: {} },
    nextAttempt: 1,
    wakeAt: null,
    notice: null,
  };
}

type At = { readonly now: number };

export type Gesture = {
  readonly slot: string;
  readonly name: CommandName;
  readonly thread: string | null;
  /** The thread as the page names it now (controls.ts `threadLabel`); null for `spawn`. */
  readonly label: string | null;
  readonly args: Readonly<Record<string, unknown>>;
  /** From the gate at the gesture: `live`, and the current copy the command needs. */
  readonly allowed: boolean;
  /** Why the gate refused, for the notice (controls.ts `refusal`). */
  readonly why?: string;
  readonly commandId: string;
  readonly generation: string;
};

export type CmdAction = At &
  (
    | { readonly type: "context"; readonly ctx: Context }
    | { readonly type: "edit"; readonly thread: string; readonly text: string }
    /** A gesture starting a command; a `send` takes its text from the thread's draft. */
    | ({ readonly type: "gesture" } & Gesture)
    | { readonly type: "retry"; readonly slot: string; readonly attempt: number; readonly allowed: boolean; readonly why?: string }
    | { readonly type: "forget"; readonly slot: string }
    | { readonly type: "reply"; readonly attempt: number; readonly reply: Reply }
    | { readonly type: "tick" }
  );

export type CmdEffect =
  | { readonly type: "post"; readonly attempt: number; readonly name: CommandName; readonly body: string; readonly generation: string }
  /** A 401: W1's compare-and-delete with the attempt's frozen generation. */
  | { readonly type: "unauthorized"; readonly generation: string }
  /** A fresh spawn's thread becomes the selection. */
  | { readonly type: "select"; readonly thread: string }
  | { readonly type: "wake"; readonly at: number | null };

const utf8 = new TextEncoder();

export function draftOf(s: CmdState, thread: string): Draft {
  return s.drafts[thread] ?? { text: "", revision: 0 };
}

export function busy(s: CmdState, slot: string): boolean {
  const c = s.slots[slot];
  return c !== undefined && BUSY.has(c.phase);
}

/** Whether Retry is offered for the slot's command now. */
export function retryable(s: CmdState, slot: string): boolean {
  const c = s.slots[slot];
  return c?.phase === "not_done" && !c.stale && c.retryAt !== null && s.now >= c.retryAt;
}

function inSession(ctx: Context, c: Command): boolean {
  return ctx.session !== null && c.session === ctx.session && (c.thread === null || !ctx.gone[c.thread]);
}

export function reduceCommands(prev: CmdState, a: CmdAction): [CmdState, CmdEffect[]] {
  const effects: CmdEffect[] = [];
  let s: CmdState = { ...prev, now: Math.max(prev.now, a.now) };
  const put = (c: Command) => {
    s = { ...s, slots: { ...s.slots, [c.slot]: c } };
  };
  const notify = (text: string) => {
    s = { ...s, notice: { seq: (s.notice?.seq ?? 0) + 1, text } };
  };
  switch (a.type) {
    case "tick":
      break;
    case "context": {
      s = { ...s, ctx: a.ctx };
      // A reopen, `hello`, epoch or generation change, or the target's 404 ends the session
      // of every command sent in it: a not-done one is released; an unknown one stays.
      for (const c of Object.values(s.slots)) {
        if (c.stale || inSession(a.ctx, c)) continue;
        put({
          ...c,
          stale: true,
          ...(c.phase === "not_done" ? { phase: "released" as const, retryAt: null } : {}),
        });
      }
      break;
    }
    case "edit": {
      const d = draftOf(s, a.thread);
      const { [a.thread]: _, ...invalid } = s.invalid;
      s = { ...s, drafts: { ...s.drafts, [a.thread]: { text: a.text, revision: d.revision + 1 } }, invalid };
      break;
    }
    case "gesture": {
      // A refused gesture sends nothing and keeps the draft, but says why (a notice).
      const held = s.slots[a.slot];
      const why = held !== undefined && busy(s, a.slot)
        ? busyReason(held)
        : s.ctx.session === null
          ? "the page is not live"
          : !a.allowed
            ? (a.why ?? "it is not available now")
            : null;
      if (why !== null) {
        notify(prefixed(a.label, refusedText(a.name, why)));
        break;
      }
      let args = a.args;
      let text: string | null = null;
      let revision: number | null = null;
      if (a.name === "send") {
        if (a.thread === null) break;
        const d = draftOf(s, a.thread);
        // The value is sent unchanged (mvp:184).
        if (d.text.length === 0 || utf8.encode(d.text).length > MAX_PROMPT_BYTES) {
          s = { ...s, invalid: { ...s.invalid, [a.thread]: d.text.length === 0 ? "empty" : "too_large" } };
          notify(prefixed(a.label, INVALID_TEXT[d.text.length === 0 ? "empty" : "too_large"]));
          break;
        }
        text = d.text;
        revision = d.revision;
        args = { thread_id: a.thread, text, origin: "pwa" };
      }
      const c: Command = {
        attempt: s.nextAttempt,
        slot: a.slot,
        name: a.name,
        commandId: a.commandId,
        body: JSON.stringify({ command_id: a.commandId, args }),
        generation: a.generation,
        session: s.ctx.session ?? "",
        thread: a.thread,
        label: a.label,
        text,
        revision,
        phase: "inflight",
        reply: null,
        retryAt: null,
        stale: false,
        posts: 1,
      };
      s = { ...s, nextAttempt: s.nextAttempt + 1 };
      put(c);
      effects.push({ type: "post", attempt: c.attempt, name: c.name, body: c.body, generation: c.generation });
      break;
    }
    case "retry": {
      const c = s.slots[a.slot];
      if (!c) break;
      // The slot is reserved again at the click: a newer command in it refuses the Retry.
      if (c.attempt !== a.attempt) {
        notify(prefixed(c.label, refusedText(c.name, busy(s, a.slot) ? busyReason(c) : "a newer one replaced it")));
        break;
      }
      if (!a.allowed || !retryable(s, a.slot) || !inSession(s.ctx, c)) {
        notify(prefixed(c.label, refusedText(c.name, a.why ?? "it is not available now")));
        break;
      }
      // The same command: its frozen id, body bytes, and generation.
      const next: Command = { ...c, attempt: s.nextAttempt, phase: "inflight", reply: null, retryAt: null, posts: c.posts + 1 };
      s = { ...s, nextAttempt: s.nextAttempt + 1 };
      put(next);
      effects.push({ type: "post", attempt: next.attempt, name: next.name, body: next.body, generation: next.generation });
      break;
    }
    case "forget": {
      const c = s.slots[a.slot];
      if (c?.phase === "check_thread") {
        put({ ...c, phase: "forgotten" });
      } else if (c?.phase === "not_done") {
        const { [a.slot]: _, ...slots } = s.slots;
        s = { ...s, slots };
      }
      break;
    }
    case "reply": {
      const c = Object.values(s.slots).find((x) => x.attempt === a.attempt && x.phase === "inflight");
      if (!c) break;
      const fresh = !c.stale && inSession(s.ctx, c);
      const r = a.reply;
      switch (r.kind) {
        case "ok":
        case "unreadable":
        case "settled": {
          put({ ...c, phase: "settled", reply: r, stale: !fresh });
          if (!fresh || r.kind !== "ok") break;
          if (c.name === "send" && c.thread !== null && "state" in r.result && r.result.state === "queued") {
            // The draft clears only if it is still the submitted revision.
            const d = draftOf(s, c.thread);
            if (d.revision === c.revision) {
              s = { ...s, drafts: { ...s.drafts, [c.thread]: { text: "", revision: d.revision + 1 } } };
            }
          }
          if (c.name === "spawn" && "thread_id" in r.result) effects.push({ type: "select", thread: r.result.thread_id });
          break;
        }
        case "unknown":
          put({ ...c, phase: "check_thread", reply: r, stale: !fresh });
          break;
        case "not_done":
          put(
            fresh
              ? { ...c, phase: "not_done", reply: r, retryAt: s.now + Math.max(0, r.retryAfter) * 1000 }
              : { ...c, phase: "released", reply: r, retryAt: null, stale: true },
          );
          break;
        case "unauthorized":
          put({ ...c, phase: "released", reply: r, retryAt: null });
          effects.push({ type: "unauthorized", generation: c.generation });
          break;
        case "not_sent":
          put({ ...c, phase: "released", reply: r, retryAt: null });
          break;
      }
      // The outcome as shown now, frozen with its target.
      const settled = s.slots[c.slot];
      if (settled) notify(prefixed(c.label, commandText(settled)));
      break;
    }
  }
  // A wake-up when the next Retry becomes available, so the page shows it.
  let at: number | null = null;
  for (const c of Object.values(s.slots)) {
    if (c.phase === "not_done" && c.retryAt !== null && c.retryAt > s.now) at = at === null ? c.retryAt : Math.min(at, c.retryAt);
  }
  if (at !== s.wakeAt) {
    effects.push({ type: "wake", at });
    s = { ...s, wakeAt: at };
  }
  return [s, effects];
}
