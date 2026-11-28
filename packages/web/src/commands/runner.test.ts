// The commands runner's handles: each POST carries the frozen body and Bearer token of its
// generation, a reply is adopted only for its own attempt, a 401 goes through the sync
// side's compare-and-delete, a missing token sends nothing, and `stop` (the page
// unloading) aborts every request and the Retry timer.

import { afterEach, describe, expect, test, vi } from "vitest";

import type { CredentialStore } from "../auth/credential";
import { Runner } from "../sync/runner";
import { slots } from "./controls";
import { CommandRunner } from "./runner";

class FakeSocket {
  static readonly OPEN = 1;
  readyState = 0;
  onopen: (() => void) | null = null;
  onclose: (() => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  send() {}
  close() {}
}

interface Held {
  path: string;
  init: RequestInit;
  answer: (r: Response) => void;
}

async function setup() {
  vi.stubGlobal("WebSocket", FakeSocket);
  const held: Held[] = [];
  const sockets: FakeSocket[] = [];
  let stored: { generation: string; token: string } | null = { generation: "g1", token: "chd_t1" };
  const cleared: string[] = [];
  const store = {
    listen: () => () => {},
    read: async () => stored,
    clearIf: async (g: string) => {
      cleared.push(g);
      if (stored?.generation === g) stored = null;
      return stored;
    },
  } as unknown as CredentialStore;
  let now = 0;
  const fetch = (path: string, init: RequestInit) => {
    if (path === "/api/ws-ticket") {
      return Promise.resolve(new Response(JSON.stringify({ ticket: "t", expires_in: 30 }), { status: 200 }));
    }
    if (path.startsWith("/api/commands/")) {
      return new Promise<Response>((answer, fail) => {
        init.signal?.addEventListener("abort", () => fail(new DOMException("aborted", "AbortError")));
        held.push({ path, init, answer });
      });
    }
    return new Promise<Response>(() => {});
  };
  const sync = new Runner({
    fetch,
    openSocket: () => {
      const s = new FakeSocket();
      sockets.push(s);
      return s as unknown as WebSocket;
    },
    store,
    now: () => now,
    socketUrl: (p) => p,
  });
  await sync.start();
  await vi.waitFor(() => expect(sockets).toHaveLength(1));
  sockets[0]!.readyState = 1;
  sockets[0]!.onopen?.();
  sockets[0]!.onmessage?.({ data: JSON.stringify({ type: "hello", epoch: "e1", server_time: "2026-11-22T09:45:00Z" }) });
  let n = 0;
  const cmds = new CommandRunner(sync, { fetch, now: () => now, uuid: () => `0193abcd-ef01-7000-8000-00000000000${++n}` });
  const spawn = () =>
    cmds.gesture({ slot: slots.spawn, name: "spawn", thread: null, args: { agent: "claude", cwd: "/work/a" }, allowed: () => true });
  return { sync, cmds, held, spawn, cleared, setNow: (t: number) => (now = t), setStored: (v: typeof stored) => (stored = v) };
}

const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status });

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("commands.runner", () => {
  test("a POST carries the frozen body and the generation's Bearer token; its reply settles it", async () => {
    const { cmds, held, spawn, sync } = await setup();
    spawn();
    expect(held).toHaveLength(1);
    const h = held[0]!;
    expect(h.path).toBe("/api/commands/spawn");
    expect(new Headers(h.init.headers).get("Authorization")).toBe("Bearer chd_t1");
    expect(h.init.method).toBe("POST");
    expect(h.init.redirect).toBe("error");
    expect(JSON.parse(h.init.body as string)).toEqual({
      command_id: "0193abcd-ef01-7000-8000-000000000001",
      args: { agent: "claude", cwd: "/work/a" },
    });
    h.answer(json(200, { thread_id: "0193abcd-ef01-7000-8000-00000000000a", launch_id: "0193abcd-ef01-7000-8000-00000000000b", input_id: null, outcome: "spawn_failed" }));
    await vi.waitFor(() => expect(cmds.getState().slots.spawn?.phase).toBe("settled"));
    expect(sync.getState().selected).toBe("0193abcd-ef01-7000-8000-00000000000a");
    cmds.stop();
    sync.stop();
  });

  test("a 401 runs the compare-and-delete of the attempt's generation", async () => {
    const { cmds, held, spawn, cleared, sync } = await setup();
    spawn();
    held[0]!.answer(json(401, { code: "unauthorized" }));
    await vi.waitFor(() => expect(cleared).toEqual(["g1"]));
    await vi.waitFor(() => expect(sync.getState().generation).toBeNull());
    expect(cmds.getState().slots.spawn?.phase).toBe("released");
    cmds.stop();
    sync.stop();
  });

  test("a 401 of an older generation leaves the newer credential alone", async () => {
    const { cmds, held, spawn, cleared, sync, setStored } = await setup();
    spawn();
    // Another tab re-paired: the store and this page now hold g2.
    setStored({ generation: "g2", token: "chd_t2" });
    await sync.retryStorage();
    expect(sync.getState().generation).toBe("g2");
    held[0]!.answer(json(401, { code: "unauthorized" }));
    await vi.waitFor(() => expect(cleared).toEqual(["g1"]));
    await new Promise((ok) => setTimeout(ok, 10));
    expect(sync.getState().generation).toBe("g2");
    expect(sync.getState().checkingCredential).toBe(false);
    cmds.stop();
    sync.stop();
  });

  test("an attempt whose generation the page no longer holds sends nothing", async () => {
    const { cmds, held, sync } = await setup();
    // A gesture frozen with g1, whose token is gone by the time the effect runs.
    cmds.dispatch({
      type: "gesture",
      slot: slots.spawn,
      name: "spawn",
      thread: null,
      args: {},
      allowed: true,
      commandId: "c",
      generation: "g-old",
    });
    await vi.waitFor(() => expect(cmds.getState().slots.spawn?.phase).toBe("released"));
    expect(held).toEqual([]);
    cmds.stop();
    sync.stop();
  });

  test("stop aborts the requests in flight and the Retry timer; nothing is adopted after", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const { cmds, held, spawn, sync } = await setup();
    spawn();
    held[0]!.answer(json(503, { code: "busy", retry_after: 1 }));
    await vi.waitFor(() => expect(cmds.getState().slots.spawn?.phase).toBe("not_done"));
    expect(cmds.getState().wakeAt).toBe(1000);
    expect(vi.getTimerCount()).toBeGreaterThan(0);
    const before = vi.getTimerCount();
    // A second, in-flight command on another slot.
    cmds.gesture({ slot: slots.ready("t"), name: "ready", thread: "t", args: {}, allowed: () => true });
    expect(held).toHaveLength(2);
    cmds.stop();
    expect(held[1]!.init.signal?.aborted).toBe(true);
    expect(vi.getTimerCount()).toBe(before - 1);
    await Promise.resolve();
    await new Promise((ok) => queueMicrotask(() => ok(null)));
    expect(cmds.getState().slots[slots.ready("t")]?.phase).toBe("inflight");
    sync.stop();
  });
});
