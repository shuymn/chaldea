// The window oracle (test only): an independent model of the history window of
// docs/design/w1-pwa.md §2.4, fed with what the client was asked to do and what it
// received. Each window the client opens starts empty at its cursor; every page the
// client accepted for it (its size as served, a halved one included) and every live event
// is applied by these rules, not the reducer's, so the expected range follows the
// navigation intent: the tail, Older, Newer, and Jump to latest.

import type { EventEnvelope } from "@chaldea/contracts/generated/EventEnvelope";

import type { Action } from "./effects";
import type { State } from "./state";

const bytesOf = (e: EventEnvelope) => new TextEncoder().encode(JSON.stringify(e)).length;

interface Expected {
  readonly id: number;
  readonly thread: string;
  events: EventEnvelope[];
  attached: boolean;
  /** Where the window began, before any event was in it. */
  lo: number;
}

export class WindowOracle {
  expected: Expected | null = null;
  /** Problems seen as they happened (a window opened away from its cursor). */
  readonly seen: string[] = [];

  private trimOld(x: Expected, s: State): void {
    const { windowMaxEvents, windowMaxBytes } = s.cfg;
    let bytes = x.events.reduce((n, e) => n + bytesOf(e), 0);
    while (x.events.length > 0 && (x.events.length > windowMaxEvents || bytes > windowMaxBytes)) {
      const e = x.events.shift();
      if (e) bytes -= bytesOf(e);
    }
  }

  private trimNew(x: Expected, s: State): boolean {
    const { windowMaxEvents, windowMaxBytes } = s.cfg;
    let bytes = x.events.reduce((n, e) => n + bytesOf(e), 0);
    let dropped = false;
    while (x.events.length > 0 && (x.events.length > windowMaxEvents || bytes > windowMaxBytes)) {
      const e = x.events.pop();
      if (e) bytes -= bytesOf(e);
      dropped = true;
    }
    return dropped;
  }

  private loOf(x: Expected): number {
    return x.events[0]?.seq ?? x.lo;
  }

  private hiOf(x: Expected): number {
    const last = x.events[x.events.length - 1];
    return last ? last.seq : this.loOf(x) - 1;
  }

  observe(before: State, a: Action, after: State): void {
    const x = this.expected;
    const wb = before.window;
    if (x && wb && x.id === wb.id && a.type === "response" && a.result.kind === "ok") {
      const req = before.inflight[a.id];
      const st = req ? before.streams[req.stream] : undefined;
      const current =
        req !== undefined &&
        st !== undefined &&
        (req.dir === "forward" ? st.fwd : st.bwd) === a.id &&
        req.generation === before.generation;
      const body = a.result.body;
      if (current && req.stream === `thread:${x.thread}` && body.mode === "replay" && body.epoch === before.epoch) {
        const cursor = st.copy?.cursor ?? 0;
        if (req.dir === "forward") {
          for (const e of body.events) {
            if (x.attached && e.seq === this.hiOf(x) + 1) {
              x.events.push(e);
              this.trimOld(x, before);
            }
          }
        } else if (wb.pending?.purpose === "newer") {
          const last = wb.pending.last;
          const page = body.events.filter((e) => e.seq > last && e.seq <= cursor);
          if (this.hiOf(x) === last && page[0]?.seq === last + 1) {
            x.events.push(...page);
            this.trimOld(x, before);
          }
          if (this.hiOf(x) === cursor) x.attached = true;
        } else if (wb.pending) {
          const lo = this.loOf(x);
          const page = body.events.filter((e) => e.seq < lo);
          if (page.length > 0 && page[page.length - 1]?.seq === lo - 1) {
            x.events.unshift(...page);
            if (wb.pending.purpose === "tail") this.trimOld(x, before);
            else if (this.trimNew(x, before)) x.attached = false;
          }
        }
      }
    }
    const wa = after.window;
    if (wa === null) {
      this.expected = null;
      return;
    }
    if (this.expected?.id === wa.id) return;
    // A new window opens empty, attached, just above its thread's cursor.
    const cursor = after.streams[`thread:${wa.thread}`]?.copy?.cursor;
    if (wa.events.length !== 0 || cursor === undefined || wa.lo !== cursor + 1 || !wa.attached) {
      this.seen.push(`window ${wa.id} opened at ${wa.lo} with ${wa.events.length} events, cursor ${cursor}`);
    }
    this.expected = { id: wa.id, thread: wa.thread, events: [], attached: true, lo: (cursor ?? 0) + 1 };
  }

  /** How the client's window differs from the expected one. */
  problems(s: State): string[] {
    const out = [...this.seen];
    const w = s.window;
    const x = this.expected;
    if (!w || !x || w.id !== x.id) return w || x ? [...out, "window not the one opened"] : out;
    const got = w.events.map((e) => e.event_id).join();
    const want = x.events.map((e) => e.event_id).join();
    if (got !== want || w.lo !== this.loOf(x)) {
      const seqs = (es: readonly EventEnvelope[]) => es.map((e) => e.seq).join(",");
      out.push(`window [${seqs(w.events)}] != expected [${seqs(x.events)}]`);
    }
    if (w.attached !== x.attached) out.push(`window attached ${w.attached}, expected ${x.attached}`);
    return out;
  }
}
