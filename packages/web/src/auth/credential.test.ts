// W1-10 `credential.generation` (fake IndexedDB): a 401 deletes the stored credential only
// when it is the request's generation, and posts only committed changes.

import { IDBFactory, IDBObjectStore as FDBObjectStore } from "fake-indexeddb";
import fc from "fast-check";
import { describe, expect, test, vi } from "vitest";

import { threadId } from "../sync/model/server";
import { seeded } from "../sync/ops";
import { threadStream } from "../sync/state";
import { CredentialStore, type CredentialNotice } from "./credential";
import { pairCode, pairFromLink } from "./pair";

function harness() {
  const posted: CredentialNotice[] = [];
  const listeners = new Set<() => void>();
  let n = 0;
  const idb = new IDBFactory();
  const make = () =>
    new CredentialStore({
      indexedDB: idb,
      uuid: () => `gen-${++n}`,
      channel: () =>
        ({
          postMessage: (m: CredentialNotice) => {
            posted.push(m);
            for (const l of listeners) l();
          },
          close: () => {},
          set onmessage(f: () => void) {
            listeners.add(f);
          },
        }) as unknown as BroadcastChannel,
    });
  return { posted, make };
}

describe("credential.generation", () => {
  test("an old 401 delayed until after a new credential is stored deletes nothing and posts nothing", async () => {
    const { posted, make } = harness();
    const tabA = make();
    const tabB = make();
    const a = await tabA.store("token-a");
    const b = await tabB.store("token-b");
    posted.length = 0;
    const after = await tabA.clearIf(a.generation);
    expect(after).toEqual(b);
    expect(await tabB.read()).toEqual(b);
    expect(posted).toEqual([]);
  });

  test("a store queued after a 401's compare began survives it: compare and delete are one transaction", async () => {
    const { make } = harness();
    const tabA = make();
    const tabB = make();
    const a = await tabA.store("token-a");
    await tabB.read();
    // B's store starts the moment A's compare reads the record.
    let storing: Promise<unknown> | null = null;
    const original = FDBObjectStore.prototype.get;
    const spy = vi.spyOn(FDBObjectStore.prototype, "get").mockImplementation(function (
      this: IDBObjectStore,
      key: IDBValidKey | IDBKeyRange,
    ) {
      const req = original.call(this, key);
      storing ??= tabB.store("token-b");
      return req;
    });
    let b: unknown;
    try {
      await tabA.clearIf(a.generation);
      b = await storing;
    } finally {
      spy.mockRestore();
    }
    expect(storing).not.toBeNull();
    expect(await tabA.read()).toEqual(b);
  });

  test("a 401 of the stored generation deletes it and posts the clear after the commit", async () => {
    const { posted, make } = harness();
    const tab = make();
    const other = make();
    const a = await tab.store("token-a");
    let seenByOther: unknown = "unset";
    other.listen(() => {
      void other.read().then((c) => {
        seenByOther = c;
      });
    });
    posted.length = 0;
    expect(await tab.clearIf(a.generation)).toBeNull();
    expect(posted).toEqual([{ cleared: a.generation }]);
    await new Promise((r) => setTimeout(r, 10));
    // The receiver rereads the store, which already shows the delete.
    expect(seenByOther).toBeNull();
  });

  test("a clear is posted only once its transaction commits", async () => {
    const { posted, make } = harness();
    const tab = make();
    const a = await tab.store("token-a");
    posted.length = 0;
    // The delete succeeds as a request, then its transaction aborts before committing.
    const original = FDBObjectStore.prototype.delete;
    const spy = vi.spyOn(FDBObjectStore.prototype, "delete").mockImplementation(function (
      this: IDBObjectStore,
      key: IDBValidKey | IDBKeyRange,
    ) {
      const req = original.call(this, key);
      const tx = this.transaction;
      req.addEventListener("success", () => queueMicrotask(() => tx.abort()));
      return req;
    });
    try {
      await expect(tab.clearIf(a.generation)).rejects.toThrow();
    } finally {
      spy.mockRestore();
    }
    expect(posted).toEqual([]);
    expect(await tab.read()).toEqual(a);
  });

  test("close releases the connection, so the database can be deleted without blocking", async () => {
    const idb = new IDBFactory();
    const store = new CredentialStore({
      indexedDB: idb,
      uuid: () => "g",
      channel: () => ({ postMessage() {}, close() {}, onmessage: null }) as unknown as BroadcastChannel,
    });
    // A write is in flight while the close is asked for: it still commits.
    const write = store.store("token");
    store.close();
    await write;
    const blocked = await new Promise<boolean>((resolve) => {
      const req = idb.deleteDatabase("chaldea");
      req.onblocked = () => resolve(true);
      req.onsuccess = () => resolve(false);
    });
    expect(blocked).toBe(false);
  });

  test("a losing 401 that finds the record empty adopts the cleared state and posts no second clear", async () => {
    const { posted, make } = harness();
    const tab = make();
    const a = await tab.store("token-a");
    await tab.clearIf(a.generation);
    posted.length = 0;
    expect(await tab.clearIf(a.generation)).toBeNull();
    expect(posted).toEqual([]);
  });

  test("any order of stores and 401s leaves the newest stored generation unless its own 401 came", async () => {
    await fc.assert(
      fc.asyncProperty(fc.array(fc.oneof(fc.constant("store" as const), fc.nat(5)), { maxLength: 12 }), async (ops) => {
        const { make } = harness();
        const tab = make();
        const gens: string[] = [];
        let want: string | null = null;
        for (const op of ops) {
          if (op === "store") {
            const c = await tab.store(`t${gens.length}`);
            gens.push(c.generation);
            want = c.generation;
          } else {
            const g = gens[op % Math.max(1, gens.length)];
            if (g === undefined) continue;
            await tab.clearIf(g);
            if (g === want) want = null;
          }
        }
        expect((await tab.read())?.generation ?? null).toBe(want);
      }),
      { numRuns: 50 },
    );
  });

  test("adopting another generation retires the old one's responses and socket", () => {
    const sim = seeded();
    sim.dispatch({ type: "select", thread: threadId(1) });
    sim.quiesce();
    sim.server.message(threadId(1), 3);
    for (const id of sim.conns.keys()) while (sim.socketStep(id));
    while (sim.deliverFrame());
    const T1 = threadStream(threadId(1));
    const old = sim.flight(T1, "forward") ?? -1;
    sim.serve(old);
    const sock = sim.state.socket?.id;
    sim.stored = "gen-2";
    sim.dispatch({ type: "credential", generation: "gen-2" });
    expect(sim.log.some((e) => e.type === "close" && e.socket === sock)).toBe(true);
    const before = JSON.stringify({ ...sim.state, now: 0 });
    sim.deliverId(old);
    expect(JSON.stringify({ ...sim.state, now: 0 })).toBe(before);
    // A delayed 401 of the old generation leaves the new one stored and prompts nobody.
    sim.dispatch({ type: "response", id: old, result: { kind: "status", status: 401, retryAfter: null } });
    expect(sim.stored).toBe("gen-2");
    sim.quiesce();
    expect(sim.state.generation).toBe("gen-2");
    expect(sim.flight(T1, "forward")).toBeUndefined();
  });

  test("a 401 on a current request clears that generation; with none stored the page asks for a link", () => {
    const sim = seeded();
    sim.quiesce();
    sim.restart();
    sim.server.swap([1], false);
    sim.quiesce();
    expect(sim.stored).toBeNull();
    expect(sim.state.generation).toBeNull();
  });
});

describe("pair flow", () => {
  test("the fragment is removed before the exchange; a refusal keeps a stored credential", async () => {
    const { make } = harness();
    const store = make();
    const kept = await store.store("token-old");
    const order: string[] = [];
    const location = { hash: "#pair=abc", pathname: "/", search: "" };
    const cred = await pairFromLink({
      location,
      history: {
        replaceState: () => {
          order.push("replaceState");
          location.hash = "";
        },
      },
      fetch: async (path, init) => {
        order.push(`${init.method} ${path} ${location.hash}`);
        expect(init).toMatchObject({ mode: "cors", credentials: "omit", redirect: "error", referrerPolicy: "no-referrer" });
        expect(new Headers(init.headers).get("Authorization")).toBeNull();
        return new Response(JSON.stringify({ code: "unauthorized" }), { status: 401 });
      },
      store,
    });
    expect(order).toEqual(["replaceState", "POST /api/pair "]);
    expect(cred).toEqual(kept);
  });

  test("a 200 stores a new generation; a spent link with nothing stored yields none", async () => {
    const { make } = harness();
    const store = make();
    const ok = await pairFromLink({
      location: { hash: "#pair=abc", pathname: "/", search: "" },
      history: { replaceState: () => {} },
      fetch: async () => new Response(JSON.stringify({ token: "tok" }), { status: 200 }),
      store,
    });
    expect(ok?.token).toBe("tok");
    const { make: make2 } = harness();
    const none = await pairFromLink({
      location: { hash: "#pair=spent", pathname: "/", search: "" },
      history: { replaceState: () => {} },
      fetch: async () => new Response("{}", { status: 401 }),
      store: make2(),
    });
    expect(none).toBeNull();
  });

  test("pairCode reads only #pair=<code>", () => {
    expect(pairCode("#pair=a-b_c")).toBe("a-b_c");
    expect(pairCode("")).toBeNull();
    expect(pairCode("#other=1")).toBeNull();
    expect(pairCode("#pair=%E0%A4%A")).toBeNull();
  });
});
