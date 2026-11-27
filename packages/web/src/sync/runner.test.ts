// The runner's handles: the socket's open deadline, its cleanup, and frame validation.

import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

import type { CredentialStore } from "../auth/credential";
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
}

const store = { listen: () => () => {}, read: async () => null, clearIf: async () => null } as unknown as CredentialStore;

function runner() {
  const sockets: FakeSocket[] = [];
  let now = 0;
  const r = new Runner({
    fetch: (path) =>
      path === "/api/ws-ticket"
        ? Promise.resolve(new Response(JSON.stringify({ ticket: "t", expires_in: 30 }), { status: 200 }))
        : new Promise<Response>(() => {}),
    openSocket: () => {
      const s = new FakeSocket();
      sockets.push(s);
      return s as unknown as WebSocket;
    },
    store,
    now: () => now,
    socketUrl: (p) => `ws://127.0.0.1:1${p}`,
  });
  return { r, sockets, advance: (ms: number) => (now += ms) };
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.stubGlobal("WebSocket", FakeSocket);
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("runner", () => {
  test("a socket that does not open in time is closed and counts as closed", async () => {
    const { r, sockets, advance } = runner();
    r.start({ generation: "g", token: "tok" });
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
    const { r, sockets } = runner();
    r.start({ generation: "g", token: "tok" });
    await vi.advanceTimersByTimeAsync(0);
    const s = sockets[0];
    if (!s) throw new Error("no socket");
    s.readyState = 1;
    s.onopen?.();
    expect(r.getState().socket?.phase).toBe("open");
    expect(JSON.parse(s.sent[0] ?? "null")).toEqual({ type: "subscribe", stream_ids: ["host"] });
    s.onmessage?.({ data: JSON.stringify({ type: "hello", epoch: 7 }) });
    s.onmessage?.({ data: "not json" });
    expect(r.getState().socket?.epoch).toBeNull();
    s.onmessage?.({ data: JSON.stringify({ type: "hello", epoch: "e1", server_time: "2026-11-22T09:45:00Z" }) });
    expect(r.getState().socket?.epoch).toBe("e1");
    await vi.advanceTimersByTimeAsync(SOCKET_OPEN_TIMEOUT_MS * 2);
    expect(s.closed).toBe(false);
  });

  test("a new generation closes the socket and clears its open deadline", async () => {
    const { r, sockets } = runner();
    r.start({ generation: "g", token: "tok" });
    await vi.advanceTimersByTimeAsync(0);
    r.start({ generation: "g2", token: "tok2" });
    expect(sockets[0]?.closed).toBe(true);
    expect(vi.getTimerCount()).toBe(0);
  });
});
