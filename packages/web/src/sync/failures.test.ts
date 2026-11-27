// W1-6 `sync.failures` and `sync.status` (docs/design/w1-pwa.md §2.3 "Failures",
// "Status"; amendments 1 and 4).

import fc from "fast-check";
import { describe, expect, test } from "vitest";

import type { FetchResult } from "./effects";
import { threadId } from "./model/server";
import { seeded } from "./ops";
import { MODEL_CONFIG, Sim, TEST_CONFIG } from "./sim";
import { HOST, listRows, rowView, status, threadStream } from "./state";

const T1 = threadStream(threadId(1));
const T2 = threadStream(threadId(2));

function live(): Sim {
  const sim = seeded();
  sim.dispatch({ type: "select", thread: threadId(1) });
  sim.dispatch({ type: "visible", threads: [threadId(2)] });
  sim.quiesce();
  return sim;
}

/** Makes thread 1's stream due and returns its forward read. */
function forwardRead(sim: Sim): number {
  sim.server.message(threadId(1), 3);
  for (const id of sim.conns.keys()) while (sim.socketStep(id));
  while (sim.deliverFrame());
  const id = sim.flight(T1, "forward");
  if (id === undefined) throw new Error("no read");
  return id;
}

function failWith(sim: Sim, id: number, result: FetchResult): void {
  sim.fail(id, result);
  sim.deliverId(id);
}

describe("sync.failures", () => {
  test("408, 503, and network failures stay due, retried at max(backoff, retry_after)", () => {
    fc.assert(
      fc.property(
        fc.array(
          fc.oneof(
            fc.constant<FetchResult>({ kind: "transport" }),
            fc.constant<FetchResult>({ kind: "timeout" }),
            fc.constant<FetchResult>({ kind: "status", status: 408, retryAfter: null }),
            fc.integer({ min: 0, max: 40 }).map<FetchResult>((s) => ({ kind: "status", status: 503, retryAfter: s })),
          ),
          { minLength: 1, maxLength: 8 },
        ),
        (failures) => {
          const sim = live();
          let id = forwardRead(sim);
          const backoff = TEST_CONFIG.backoffMs;
          failures.forEach((f, k) => {
            failWith(sim, id, f);
            const ra = f.kind === "status" && f.retryAfter !== null ? f.retryAfter * 1000 : 0;
            const wait = Math.max(backoff[Math.min(k, backoff.length - 1)] ?? 30000, ra);
            expect(sim.state.streams[T1]?.fwd).toBeNull();
            sim.advance(wait - 1);
            expect(sim.flight(T1, "forward")).toBeUndefined();
            sim.advance(1);
            const next = sim.flight(T1, "forward");
            expect(next).toBeDefined();
            id = next ?? -1;
          });
          sim.quiesce();
          expect(sim.state.streams[T1]?.copy?.cursor).toBe(sim.server.head(T1));
        },
      ),
      { numRuns: 60 },
    );
  });

  test("schema and other refusals make no second request until the user retries", () => {
    fc.assert(
      fc.property(
        fc.constantFrom<FetchResult>(
          { kind: "schema" },
          { kind: "status", status: 400, retryAfter: null },
          { kind: "status", status: 500, retryAfter: null },
          { kind: "status", status: 403, retryAfter: null },
        ),
        (failure) => {
          const sim = live();
          failWith(sim, forwardRead(sim), failure);
          expect(sim.state.streams[T1]?.error).not.toBeNull();
          sim.server.message(threadId(1), 3);
          sim.refresh();
          for (const id of sim.conns.keys()) while (sim.socketStep(id));
          while (sim.deliverFrame());
          sim.advance(120_000);
          expect(sim.flight(T1, "forward")).toBeUndefined();
          // The copy is kept.
          expect(sim.state.streams[T1]?.copy).not.toBeNull();
          sim.dispatch({ type: "retry" });
          expect(sim.flight(T1, "forward")).toBeDefined();
        },
      ),
      { numRuns: 8 },
    );
  });

  test("a failed backward read is not retried automatically", () => {
    const sim = live();
    sim.dispatch({ type: "older" });
    const id = sim.flight(T1, "backward") ?? -1;
    failWith(sim, id, { kind: "status", status: 503, retryAfter: 1 });
    sim.advance(120_000);
    expect(sim.flight(T1, "backward")).toBeUndefined();
    expect(sim.state.window?.error).not.toBeNull();
    sim.dispatch({ type: "retry" });
    expect(sim.flight(T1, "backward")).toBeDefined();
  });

  test("an oversized forward page halves its limit down to 1, then reports the limit", () => {
    const sim = live();
    let id = forwardRead(sim);
    const limits: number[] = [];
    for (let k = 0; k < 10 && id !== undefined; k++) {
      limits.push(sim.flights.get(id)?.req.limit ?? -1);
      failWith(sim, id, { kind: "oversize" });
      id = sim.flight(T1, "forward") ?? -1;
      if (id === -1) break;
    }
    expect(limits).toEqual([4, 2, 1]);
    expect(sim.state.streams[T1]?.error?.message).toBe("this thread's data exceeds the 16 MiB read limit");
    sim.advance(120_000);
    expect(sim.flight(T1, "forward")).toBeUndefined();
  });

  test("an oversized Newer keeps the captured end and halves n (nearest-before semantics)", () => {
    const sim = new Sim({ ...TEST_CONFIG, backwardLimit: 8, windowMaxEvents: 8 }, MODEL_CONFIG);
    sim.server.register(1, false);
    for (let k = 0; k < 30; k++) sim.server.message(threadId(1), 3);
    sim.boot();
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    sim.dispatch({ type: "older" });
    sim.quiesce();
    expect(sim.state.window?.attached).toBe(false);
    const last = sim.state.window?.hi ?? -1;
    sim.dispatch({ type: "newer" });
    const seen: [number | null, number][] = [];
    for (let k = 0; k < 3; k++) {
      const id = sim.flight(T1, "backward") ?? -1;
      const r = sim.flights.get(id)?.req;
      seen.push([r?.beforeSeq ?? null, r?.limit ?? -1]);
      failWith(sim, id, { kind: "oversize" });
    }
    expect(seen).toEqual([
      [last + 8 + 1, 8],
      [last + 4 + 1, 4],
      [last + 2 + 1, 2],
    ]);
    // The halved read, answered by the model with the store's `seq < before_seq`.
    const id = sim.flight(T1, "backward") ?? -1;
    sim.serve(id);
    const answer = sim.responses.find((d) => d.id === id)?.result;
    expect(answer?.kind === "ok" && answer.body.mode === "replay" && answer.body.events.map((e) => e.seq)).toEqual([last + 1]);
    sim.deliverId(id);
    expect(sim.state.window?.hi).toBe(last + 1);
  });

  test("an oversized backward page halves its limit, then reports the limit", () => {
    const sim = live();
    sim.dispatch({ type: "older" });
    const limits: number[] = [];
    for (let k = 0; k < 5; k++) {
      const id = sim.flight(T1, "backward");
      if (id === undefined) break;
      limits.push(sim.flights.get(id)?.req.limit ?? -1);
      failWith(sim, id, { kind: "oversize" });
    }
    expect(limits).toEqual([3, 1]);
    expect(sim.state.window?.error?.message).toBe("this thread's data exceeds the 16 MiB read limit");
  });
});

describe("sync.status", () => {
  test("a socket frame does not make the page live while a read fails in transport", () => {
    const sim = live();
    const id = forwardRead(sim);
    failWith(sim, id, { kind: "transport" });
    expect(status(sim.state)).toBe("updates paused");
    const socket = sim.state.socket?.id ?? -1;
    sim.dispatch({ type: "socketMessage", socket, msg: { type: "heads", heads: [] } });
    sim.dispatch({ type: "socketMessage", socket, msg: { type: "stream_advanced", stream_id: HOST, head_seq: 1 } });
    expect(status(sim.state)).toBe("updates paused");
    // The stream's next read succeeding is what restores live.
    sim.advance(1000);
    const next = sim.flight(T1, "forward") ?? -1;
    sim.serve(next);
    sim.deliverId(next);
    expect(status(sim.state)).toBe("live");
  });

  test("selecting a thread not yet followed is not live until its first forward read arrives", () => {
    const sim = seeded();
    sim.quiesce();
    expect(status(sim.state)).toBe("live");
    sim.dispatch({ type: "select", thread: threadId(1) });
    expect(sim.state.streams[T1]?.lastRead).toBeNull();
    expect(status(sim.state)).toBe("updates paused");
    const id = sim.flight(T1, "forward") ?? -1;
    sim.serve(id);
    sim.deliverId(id);
    expect(status(sim.state)).toBe("live");
  });

  test("a background 408 or 503 in backoff marks the row retrying", () => {
    for (const code of [408, 503]) {
      const sim = live();
      sim.server.message(threadId(2), 3);
      for (const id of sim.conns.keys()) while (sim.socketStep(id));
      while (sim.deliverFrame());
      const bg = sim.flight(T2, "forward") ?? -1;
      failWith(sim, bg, { kind: "status", status: code, retryAfter: 20 });
      const row = listRows(sim.state).find((r) => r.info.thread_id === threadId(2));
      if (!row) throw new Error("no row");
      expect(rowView(sim.state, row).retrying).toBe(true);
      expect(status(sim.state)).toBe("live");
      sim.quiesce();
      expect(rowView(sim.state, row).retrying).toBe(false);
    }
  });

  test("a failed history read shows on the window only; the page stays live (N1)", () => {
    const sim = live();
    sim.dispatch({ type: "older" });
    const id = sim.flight(T1, "backward") ?? -1;
    failWith(sim, id, { kind: "timeout" });
    expect(sim.state.window?.error).not.toBeNull();
    expect(sim.state.streams[T1]?.lastRead).toBe("ok");
    expect(status(sim.state)).toBe("live");
    // Selecting another thread leaves nothing behind that could hold the page.
    sim.dispatch({ type: "select", thread: threadId(2) });
    sim.quiesce();
    expect(status(sim.state)).toBe("live");
  });

  test("a background stream's failing reads show on its row, not in the page status (N2)", () => {
    const sim = live();
    sim.server.message(threadId(2), 3);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const bg = sim.flight(T2, "forward") ?? -1;
    failWith(sim, bg, { kind: "transport" });
    expect(status(sim.state)).toBe("live");
    const row = listRows(sim.state).find((r) => r.info.thread_id === threadId(2));
    expect(row && rowView(sim.state, row).retrying).toBe(true);
    sim.quiesce();
    expect(row && rowView(sim.state, row).retrying).toBe(false);
  });

  test("a history timeout on one thread, then selecting another, does not stick (sol M1)", () => {
    const sim = seeded();
    sim.quiesce();
    sim.dispatch({ type: "visible", threads: [threadId(2)] });
    sim.dispatch({ type: "select", thread: threadId(2) });
    sim.quiesce();
    sim.dispatch({ type: "latest" });
    const old = sim.flight(T2, "backward") ?? -1;
    failWith(sim, old, { kind: "timeout" });
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    sim.advance(120_000);
    sim.server.message(threadId(1), 3);
    sim.quiesce();
    expect(sim.state.streams[T1]?.copy?.cursor).toBe(sim.server.head(T1));
    expect(status(sim.state)).toBe("live");
  });

  test("the selected thread's read failing with the socket down is updates paused while the host read works", () => {
    const sim = live();
    const id = forwardRead(sim);
    sim.restart();
    failWith(sim, id, { kind: "transport" });
    expect(status(sim.state)).toBe("updates paused");
  });

  test("a forward 404 does not set last synced", () => {
    const sim = live();
    sim.advance(2000);
    const before = sim.state.lastSyncedAt;
    const id = forwardRead(sim);
    failWith(sim, id, { kind: "status", status: 404, retryAfter: null });
    expect(sim.state.gone[threadId(1)]).toBe(true);
    expect(sim.state.lastSyncedAt).toBe(before);
  });

  test("last synced waits for has_more alone and for the reported head alone", () => {
    for (const gate of ["has_more", "head"] as const) {
      const sim = live();
      sim.advance(1000);
      const before = sim.state.lastSyncedAt;
      const id = forwardRead(sim);
      const a = sim.server.answer({
        stream: T1,
        afterSeq: sim.state.streams[T1]?.copy?.cursor ?? 0,
        beforeSeq: null,
        limit: 4,
        epoch: sim.state.epoch,
      });
      if (a.status !== 200 || a.body.mode !== "replay") throw new Error("no page");
      // A page that reaches the head but says has_more, or ends below a head it reports.
      const body =
        gate === "has_more"
          ? { ...a.body, has_more: true }
          : { ...a.body, head_seq: a.body.head_seq + 1, has_more: false };
      sim.fail(id, { kind: "ok", body });
      sim.deliverId(id);
      expect(sim.state.lastSyncedAt).toBe(before);
    }
  });

  test("connecting, then live", () => {
    const sim = seeded();
    expect(status(sim.state)).toBe("connecting");
    sim.quiesce();
    expect(status(sim.state)).toBe("live");
  });

  test("a stop seen from an idle page becomes offline through the ticket failure; last synced is kept", () => {
    const sim = live();
    const synced = sim.state.lastSyncedAt;
    expect(synced).not.toBeNull();
    expect(Object.keys(sim.state.inflight)).toEqual([]);
    sim.restart();
    sim.up = false;
    expect(status(sim.state)).toBe("updates paused");
    sim.advance(1000);
    sim.serveTicket();
    expect(status(sim.state)).toBe("offline");
    sim.advance(60_000);
    for (let k = 0; k < 5 && sim.serveTicket(); k++) sim.advance(30_000);
    expect(status(sim.state)).toBe("offline");
    expect(sim.state.lastSyncedAt).toBe(synced);
    sim.server.message(threadId(1), 3);
    sim.quiesce();
    expect(status(sim.state)).toBe("live");
    expect(sim.state.lastSyncedAt).toBeGreaterThan(synced ?? 0);
  });

  test("a socket-only failure is updates paused while reads work", () => {
    const sim = live();
    sim.restart();
    sim.advance(1000);
    expect(status(sim.state)).toBe("updates paused");
    // The ticket works but the socket does not open.
    sim.serveTicket();
    sim.up = false;
    sim.open();
    sim.up = true;
    expect(status(sim.state)).toBe("updates paused");
  });

  test("one stream in error does not freeze last synced for the others", () => {
    const sim = live();
    sim.server.message(threadId(2), 3);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const t2 = sim.flight(T2, "forward") ?? -1;
    failWith(sim, t2, { kind: "schema" });
    const before = sim.state.lastSyncedAt ?? 0;
    sim.advance(5000);
    sim.server.message(threadId(1), 3);
    sim.quiesce();
    expect(sim.state.streams[T2]?.error).not.toBeNull();
    expect(sim.state.lastSyncedAt).toBeGreaterThan(before);
  });

  test("last synced waits for has_more and the head the read reported, socket heads withheld", () => {
    fc.assert(
      fc.property(fc.integer({ min: 5, max: 24 }), (events) => {
        const sim = new Sim(TEST_CONFIG, { ...MODEL_CONFIG, replayMaxEvents: 1000, replayMaxBytes: 1 << 30 });
        sim.withholdHeads = true;
        sim.server.register(1, false);
        sim.boot();
        sim.dispatch({ type: "select", thread: threadId(1) });
        sim.quiesce(60, 0);
        const before = sim.state.lastSyncedAt;
        for (let k = 0; k < events; k++) sim.server.message(threadId(1), 3);
        sim.advance(1000);
        sim.dispatch({ type: "retry" });
        // Ask for a read the way a reconnect does: a refetch of every stream.
        sim.restart();
        sim.advance(1000);
        sim.connect();
        let pages = 0;
        while (pages < 100) {
          const id = sim.flight(T1, "forward");
          if (id === undefined) break;
          sim.serve(id);
          sim.deliverId(id);
          const host = sim.flight(HOST, "forward");
          if (host !== undefined) {
            sim.serve(host);
            sim.deliverId(host);
          }
          pages += 1;
          const done = sim.state.streams[T1]?.copy?.cursor === sim.server.head(T1);
          if (!done) expect(sim.state.lastSyncedAt).toBe(before);
        }
        expect(pages).toBeGreaterThan(1);
        expect(sim.state.lastSyncedAt).toBeGreaterThan(before ?? -1);
      }),
      { numRuns: 20 },
    );
  });
});
