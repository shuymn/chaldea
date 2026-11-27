// The request policy (docs/design/w1-pwa.md §2.2) and the read limit (§2.3 "Failures").

import { describe, expect, test, vi } from "vitest";

import { eventsPath, readEvents, postTicket } from "./http";
import type { Request } from "./sync/state";

const req: Request = {
  id: 1,
  stream: "thread:0193abcd-ef01-7000-8000-000000000001",
  dir: "forward",
  purpose: "forward",
  afterSeq: 5,
  beforeSeq: null,
  limit: 200,
  epoch: "e1",
  generation: "g",
};

const never = new AbortController().signal;

describe("http", () => {
  test("a read is a fixed relative path under the policy, the token only in Authorization", async () => {
    let seen: [string, RequestInit] | null = null;
    const r = await readEvents(
      async (path, init) => {
        seen = [path, init];
        return new Response(JSON.stringify({ code: "busy", retry_after: 3 }), { status: 503 });
      },
      req,
      "tok",
      never,
      1024,
    );
    expect(r).toEqual({ kind: "status", status: 503, retryAfter: 3 });
    if (!seen) throw new Error("no request");
    const [path, init] = seen as [string, RequestInit];
    expect(path).toBe(eventsPath(req));
    expect(path.startsWith("/api/events?")).toBe(true);
    expect(path).not.toContain("tok");
    expect(init).toMatchObject({ mode: "cors", credentials: "omit", redirect: "error", referrerPolicy: "no-referrer", method: "GET" });
    expect(new Headers(init.headers).get("Authorization")).toBe("Bearer tok");
    expect(new URLSearchParams(path.split("?")[1]).get("after_seq")).toBe("5");
    expect(eventsPath({ ...req, dir: "backward", afterSeq: null, beforeSeq: 9, epoch: null })).not.toContain("epoch");
  });

  test("a body over the limit is refused unparsed, by Content-Length or by counting", async () => {
    const big = "x".repeat(2048);
    expect(
      await readEvents(async () => new Response(big, { status: 200, headers: { "Content-Length": "2048" } }), req, "t", never, 1024),
    ).toEqual({ kind: "oversize" });
    const stream = new ReadableStream({
      start(c) {
        c.enqueue(new TextEncoder().encode(big));
        c.close();
      },
    });
    expect(await readEvents(async () => new Response(stream, { status: 200 }), req, "t", never, 1024)).toEqual({ kind: "oversize" });
  });

  test("a timeout while reading an error body settles as a timeout, never as the status", async () => {
    for (const status of [404, 401]) {
      const deadline = new AbortController();
      const spy = vi.spyOn(AbortSignal, "timeout").mockReturnValue(deadline.signal);
      const stalled = () =>
        new ReadableStream({
          start(c) {
            deadline.signal.addEventListener("abort", () => c.error(deadline.signal.reason));
          },
        });
      const read = readEvents(async () => new Response(stalled(), { status }), req, "t", never, 1024);
      const ticket = postTicket(async () => new Response(stalled(), { status }), "t", never);
      setTimeout(() => deadline.abort(new DOMException("deadline", "TimeoutError")), 0);
      expect(await read).toEqual({ kind: "timeout" });
      expect(await ticket).toEqual({ kind: "transport" });
      spy.mockRestore();
    }
    vi.restoreAllMocks();
  });

  test("a network failure is transport, a bad body schema", async () => {
    expect(await readEvents(async () => Promise.reject(new TypeError("x")), req, "t", never, 1024)).toEqual({ kind: "transport" });
    expect(await readEvents(async () => new Response("{}", { status: 200 }), req, "t", never, 1024)).toEqual({ kind: "schema" });
    expect(await postTicket(async () => Promise.reject(new TypeError("x")), "t", never)).toEqual({ kind: "transport" });
  });
});
