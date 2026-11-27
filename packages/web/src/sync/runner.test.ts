// The runner's handles: startup adoption, the socket's open deadline, stop, and the
// cancellation of superseded work.

import { IDBFactory } from "fake-indexeddb";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

import { type Credential, CredentialStore } from "../auth/credential";
import { pairFromLink } from "../auth/pair";
import { Runner, SOCKET_OPEN_TIMEOUT_MS } from "./runner";

class FakeSocket {
  static readonly OPEN = 1;
  readyState = 0;
  closed = false;
  sent: string[] = [];
  onopen: (() => void) | null = null;
  onclose: (() => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  send(s: string) {
    this.sent.push(s);
  }
  close() {
    this.closed = true;
  }
  open() {
    this.readyState = 1;
    this.onopen?.();
  }
  hello(epoch = "e1") {
    this.onmessage?.({ data: JSON.stringify({ type: "hello", epoch, server_time: "2026-11-22T09:45:00Z" }) });
  }
}

/** A store whose record the test sets, and whose notices the test fires. */
function fakeStore(initial: Credential | null) {
  const listeners = new Set<() => void>();
  const s = {
    stored: initial,
    reads: 0,
    beforeRead: null as null | (() => void),
    listen: (f: () => void) => {
      listeners.add(f);
      return () => listeners.delete(f);
    },
    read: async () => {
      s.reads += 1;
      const v = s.stored;
      const hook = s.beforeRead;
      s.beforeRead = null;
      hook?.();
      return v;
    },
    clearIf: async () => s.stored,
    notify: () => {
      for (const f of listeners) f();
    },
    listeners,
  };
  return s;
}

type Call = { path: string; init: RequestInit };

function runner(store: unknown) {
  const sockets: FakeSocket[] = [];
  const calls: Call[] = [];
  let now = 0;
  const r = new Runner({
    fetch: (path, init) => {
      calls.push({ path, init });
      return path === "/api/ws-ticket" && tickets
        ? Promise.resolve(new Response(JSON.stringify({ ticket: "t", expires_in: 30 }), { status: 200 }))
        : new Promise<Response>(() => {});
    },
    openSocket: () => {
      const s = new FakeSocket();
      sockets.push(s);
      return s as unknown as WebSocket;
    },
    store: store as CredentialStore,
    now: () => now,
    socketUrl: (p) => `ws://127.0.0.1:1${p}`,
  });
  let tickets = true;
  return {
    r,
    sockets,
    calls,
    advance: (ms: number) => (now += ms),
    holdTickets: () => {
      tickets = false;
    },
  };
}

const A: Credential = { generation: "a", token: "tok-a" };
const B: Credential = { generation: "b", token: "tok-b" };
const auth = (c: Call) => new Headers(c.init.headers).get("Authorization");

beforeEach(() => {
  vi.useFakeTimers();
  vi.stubGlobal("WebSocket", FakeSocket);
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("runner", () => {
  test("startup: a replacement committed during the startup read is adopted", async () => {
    const store = fakeStore(A);
    // Another tab commits B and posts its notice while this tab reads A.
    store.beforeRead = () => {
      store.stored = B;
      store.notify();
    };
    const { r, calls } = runner(store);
    await r.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.getState().generation).toBe("b");
    expect(calls.length).toBeGreaterThan(0);
    expect(calls.filter((c) => auth(c) === "Bearer tok-a").every((c) => c.init.signal?.aborted)).toBe(true);
    expect(calls.some((c) => auth(c) === "Bearer tok-b")).toBe(true);
  });

  test("startup: an ambiguous pairing keeps the stored credential", async () => {
    vi.useRealTimers();
    const store = new CredentialStore({
      indexedDB: new IDBFactory(),
      uuid: () => "stored",
      channel: () => ({ postMessage() {}, close() {}, onmessage: null }) as unknown as BroadcastChannel,
    });
    await store.store("tok-stored");
    for (const answer of [() => Promise.reject(new TypeError("down")), async () => new Response("{}", { status: 503 })]) {
      const { r } = runner(store);
      await r.start(() =>
        pairFromLink({
          location: { hash: "#pair=code", pathname: "/", search: "" },
          history: { replaceState: () => {} },
          fetch: answer,
          store,
        }),
      );
      expect(r.getState().generation).toBe("stored");
      r.stop();
    }
  });

  test("after a notice, the old socket's callbacks change nothing", async () => {
    const store = fakeStore(A);
    const { r, sockets } = runner(store);
    await r.start();
    await vi.advanceTimersByTimeAsync(0);
    const old = sockets[0];
    if (!old) throw new Error("no socket");
    old.open();
    old.hello();
    expect(r.getState().socket?.phase).toBe("open");
    store.stored = B;
    store.notify();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.getState().generation).toBe("b");
    expect(old.closed).toBe(true);
    const before = JSON.stringify({ ...r.getState(), now: 0 });
    old.hello("e9");
    old.onmessage?.({ data: JSON.stringify({ type: "heads", heads: [{ stream_id: "host", head_seq: 9 }] }) });
    old.onclose?.();
    expect(JSON.stringify({ ...r.getState(), now: 0 })).toBe(before);
  });

  test("a socket that does not open in time is closed and counts as closed", async () => {
    const { r, sockets, advance } = runner(fakeStore(A));
    await r.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(sockets).toHaveLength(1);
    expect(r.getState().socket?.phase).toBe("connecting");
    advance(SOCKET_OPEN_TIMEOUT_MS);
    await vi.advanceTimersByTimeAsync(SOCKET_OPEN_TIMEOUT_MS);
    expect(sockets[0]?.closed).toBe(true);
    expect(r.getState().socket).toBeNull();
    expect(r.getState().socketAttempt).toBe(1);
  });

  test("an open socket keeps no deadline; frames that fail validation are ignored", async () => {
    const { r, sockets } = runner(fakeStore(A));
    await r.start();
    await vi.advanceTimersByTimeAsync(0);
    const s = sockets[0];
    if (!s) throw new Error("no socket");
    s.open();
    expect(r.getState().socket?.phase).toBe("open");
    expect(JSON.parse(s.sent[0] ?? "null")).toEqual({ type: "subscribe", stream_ids: ["host"] });
    s.onmessage?.({ data: JSON.stringify({ type: "hello", epoch: 7 }) });
    s.onmessage?.({ data: "not json" });
    expect(r.getState().socket?.epoch).toBeNull();
    s.hello();
    expect(r.getState().socket?.epoch).toBe("e1");
    await vi.advanceTimersByTimeAsync(SOCKET_OPEN_TIMEOUT_MS * 2);
    expect(s.closed).toBe(false);
  });

  test("stop ends the reads, the ticket POST, the sockets, the timers, and the listener", async () => {
    const store = fakeStore(A);
    const { r, calls, sockets, holdTickets } = runner(store);
    holdTickets();
    await r.start();
    await vi.advanceTimersByTimeAsync(0);
    expect(calls.map((c) => c.path.split("?")[0]).sort()).toEqual(["/api/events", "/api/ws-ticket"]);
    r.stop();
    expect(calls.every((c) => c.init.signal?.aborted)).toBe(true);
    expect(sockets).toEqual([]);
    expect(vi.getTimerCount()).toBe(0);
    expect(store.listeners.size).toBe(0);
    store.stored = B;
    store.notify();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.getState().generation).toBe("a");
  });

  test("a credential change aborts the superseded ticket POST", async () => {
    const store = fakeStore(A);
    const { r, calls, holdTickets } = runner(store);
    holdTickets();
    await r.start();
    await vi.advanceTimersByTimeAsync(0);
    const ticket = calls.find((c) => c.path === "/api/ws-ticket");
    expect(ticket?.init.signal?.aborted).toBe(false);
    store.stored = B;
    store.notify();
    await vi.advanceTimersByTimeAsync(0);
    expect(ticket?.init.signal?.aborted).toBe(true);
  });

  test("a wake deadline that goes away stops its timer", async () => {
    const store = fakeStore(A);
    const { r, sockets, advance } = runner(store);
    await r.start();
    await vi.advanceTimersByTimeAsync(0);
    // The socket fails to open: a reconnect is scheduled (a wake timer).
    advance(SOCKET_OPEN_TIMEOUT_MS);
    await vi.advanceTimersByTimeAsync(SOCKET_OPEN_TIMEOUT_MS);
    expect(sockets[0]?.closed).toBe(true);
    expect(r.getState().wakeAt).not.toBeNull();
    const timers = vi.getTimerCount();
    // The credential goes: nothing is due, and the wake timer goes with it.
    store.stored = null;
    store.notify();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.getState().wakeAt).toBeNull();
    expect(vi.getTimerCount()).toBe(timers - 1);
  });
});
