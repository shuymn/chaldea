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

  test("a store that cannot be read at startup shows as such, and Retry recovers", async () => {
    const store = fakeStore(A);
    const read = store.read;
    store.read = async () => {
      throw new Error("storage unavailable");
    };
    const { r } = runner(store);
    await r.start();
    expect(r.getState().generation).toBeUndefined();
    expect(r.getState().storageFailed).toBe(true);
    store.read = read;
    await r.retryStorage();
    expect(r.getState().storageFailed).toBe(false);
    expect(r.getState().generation).toBe("a");
  });

  test("Retry reopens a credential store whose first open failed (real CredentialStore)", async () => {
    vi.useRealTimers();
    const idb = new IDBFactory();
    let opens = 0;
    const flaky = {
      open: (name: string, version?: number) => {
        opens += 1;
        if (opens > 1) return idb.open(name, version);
        const req = {} as IDBOpenDBRequest;
        queueMicrotask(() => req.onerror?.(new Event("error")));
        return req;
      },
    } as unknown as IDBFactory;
    const store = new CredentialStore({
      indexedDB: flaky,
      uuid: () => "fresh",
      channel: () => ({ postMessage() {}, close() {}, onmessage: null }) as unknown as BroadcastChannel,
    });
    const { r } = runner(store);
    await r.start();
    expect(r.getState().storageFailed).toBe(true);
    await r.retryStorage();
    expect(opens).toBe(2);
    expect(r.getState().storageFailed).toBe(false);
    expect(r.getState().generation).toBeNull();
    r.stop();
  });

  test("a clear for A that completes after B is adopted never retires B", async () => {
    const store = fakeStore(A);
    let finishClear: () => void = () => {};
    store.clearIf = () =>
      new Promise((resolve) => {
        finishClear = () => resolve(null);
      });
    let first = true;
    const calls: Call[] = [];
    const r = new Runner({
      fetch: (path, init) => {
        calls.push({ path, init });
        if (path.startsWith("/api/events") && first) {
          first = false;
          return Promise.resolve(new Response("{}", { status: 401 }));
        }
        return new Promise<Response>(() => {});
      },
      openSocket: () => {
        throw new Error("unused");
      },
      store: store as unknown as CredentialStore,
      now: () => 0,
      socketUrl: (p) => p,
    });
    await r.start();
    await vi.advanceTimersByTimeAsync(0);
    store.stored = B;
    store.notify();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.getState().generation).toBe("b");
    // A's clear completes, and the reread it triggers fails.
    store.read = async () => {
      throw new Error("busy");
    };
    finishClear();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.getState().generation).toBe("b");
    const bCalls = calls.filter((c) => auth(c) === "Bearer tok-b");
    expect(bCalls.length).toBeGreaterThan(0);
    expect(bCalls.every((c) => !c.init.signal?.aborted)).toBe(true);
    r.stop();
  });

  test("resume reconciles with the store before any transport starts", async () => {
    const store = fakeStore(A);
    const { r, calls } = runner(store);
    await r.start();
    await vi.advanceTimersByTimeAsync(0);
    r.suspend();
    // B is committed while the page is frozen; its notice is not delivered.
    store.stored = B;
    const mark = calls.length;
    await r.resume();
    await vi.advanceTimersByTimeAsync(0);
    const resumed = calls.slice(mark);
    expect(resumed.length).toBeGreaterThan(0);
    expect(resumed.every((c) => auth(c) === "Bearer tok-b")).toBe(true);
    r.stop();
  });

  test("a 401 whose delete fails retires that generation; a newer one is still adopted", async () => {
    const store = fakeStore(A);
    let clears = 0;
    store.clearIf = async () => {
      clears += 1;
      throw new Error("write aborted");
    };
    const reads: string[] = [];
    const r = new Runner({
      fetch: (path, init) => {
        if (path.startsWith("/api/events")) {
          const auth = new Headers(init.headers).get("Authorization") ?? "";
          reads.push(auth);
          if (auth === "Bearer tok-a") return Promise.resolve(new Response("{}", { status: 401 }));
        }
        return new Promise<Response>(() => {});
      },
      openSocket: () => {
        throw new Error("unused");
      },
      store: store as unknown as CredentialStore,
      now: () => 0,
      socketUrl: (p) => p,
    });
    await r.start();
    await vi.advanceTimersByTimeAsync(10);
    expect(clears).toBe(1);
    expect(reads.filter((a) => a === "Bearer tok-a")).toEqual(["Bearer tok-a"]);
    expect(r.getState().generation).toBeNull();
    // Another tab pairs again: the new generation is adopted.
    store.stored = B;
    store.notify();
    await vi.advanceTimersByTimeAsync(0);
    expect(r.getState().generation).toBe("b");
    r.stop();
  });

  test("a pair exchange that answers after stop is not stored; no store read follows", async () => {
    vi.useRealTimers();
    let finish: (r: Response) => void = () => {};
    let signal: AbortSignal | undefined;
    let writes = 0;
    let reads = 0;
    const store = {
      listen: () => () => {},
      read: async () => {
        reads += 1;
        return null;
      },
      store: async (token: string) => {
        writes += 1;
        return { generation: "new", token };
      },
    };
    const { r } = runner(store);
    const done = r.start((lifetime) =>
      pairFromLink({
        store: store as unknown as CredentialStore,
        location: { hash: "#pair=code", pathname: "/", search: "" },
        history: { replaceState() {} },
        fetch: (_path, init) => {
          signal = init.signal ?? undefined;
          return new Promise<Response>((resolve) => {
            finish = resolve;
          });
        },
        signal: lifetime,
      }),
    );
    r.stop();
    finish(new Response(JSON.stringify({ token: "fresh" }), { status: 200 }));
    await done;
    expect(signal?.aborted).toBe(true);
    expect(writes).toBe(0);
    expect(reads).toBe(0);
  });

  test("suspend ends the transport and keeps the state; resume reconnects and refetches", async () => {
    const store = fakeStore(A);
    const { r, calls, sockets } = runner(store);
    await r.start();
    await vi.advanceTimersByTimeAsync(0);
    sockets[0]?.open();
    sockets[0]?.hello();
    r.dispatch({ type: "select", thread: "0193abcd-ef01-7000-8000-000000000001" });
    const before = r.getState();
    r.suspend();
    expect(calls.every((c) => c.path === "/api/ws-ticket" || c.init.signal?.aborted)).toBe(true);
    expect(sockets[0]?.closed).toBe(true);
    expect(vi.getTimerCount()).toBe(0);
    expect(r.getState().selected).toBe(before.selected);
    expect(r.getState().generation).toBe("a");
    const made = calls.length;
    await vi.advanceTimersByTimeAsync(60_000);
    expect(calls.length).toBe(made);
    r.resume();
    await vi.advanceTimersByTimeAsync(0);
    const after = calls.slice(made).map((c) => c.path.split("?")[0]);
    expect(after).toContain("/api/ws-ticket");
    expect(after.filter((p) => p === "/api/events").length).toBeGreaterThanOrEqual(2);
    expect(r.getState().selected).toBe(before.selected);
    r.stop();
  });
});
