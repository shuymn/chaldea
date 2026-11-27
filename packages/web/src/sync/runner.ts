// The runner: the only module holding browser handles (fetch, WebSocket, timers, the
// credential store). It feeds actions to the pure reducer and carries out its effects;
// React reads the state through `useSyncExternalStore(runner.subscribe, runner.getState)`.

import { validators } from "@chaldea/contracts";

import type { Credential, CredentialStore } from "../auth/credential";
import { type Fetch, postTicket, readEvents } from "../http";
import type { Action, Effect, FetchResult } from "./effects";
import { reduce } from "./reducer";
import { type Config, DEFAULT_CONFIG, initialState, type State } from "./state";

/** How long a socket may take to open (the read timeout's length). */
export const SOCKET_OPEN_TIMEOUT_MS = 10_000;

type DistributiveOmit<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;
export type Input = DistributiveOmit<Action, "now">;

export interface RunnerDeps {
  readonly fetch: Fetch;
  readonly openSocket: (url: string) => WebSocket;
  readonly store: CredentialStore;
  readonly now: () => number;
  readonly socketUrl: (path: string) => string;
}

export class Runner {
  private state: State;
  private readonly listeners = new Set<() => void>();
  private readonly aborts = new Map<number, AbortController>();
  /** The ticket POST of each socket still waiting for one. */
  private readonly tickets = new Map<number, AbortController>();
  private readonly sockets = new Map<number, WebSocket>();
  /** The open deadline of each socket not yet open. */
  private readonly opening = new Map<number, ReturnType<typeof setTimeout>>();
  private timer: ReturnType<typeof setTimeout> | null = null;
  private cred: Credential | null = null;
  private readonly deps: RunnerDeps;
  /** Rereads of the store, one at a time, so the latest read is adopted last. */
  private adoption: Promise<void> = Promise.resolve();
  private unlisten: (() => void) | null = null;
  private stopped = false;
  /** Aborted by `stop`: boot and pairing run under it, so a late exchange is not applied. */
  private readonly lifetime = new AbortController();
  /** Generations refused with 401 whose delete failed: never used again by this page. */
  private readonly retired = new Set<string>();

  constructor(deps: RunnerDeps, cfg: Config = DEFAULT_CONFIG) {
    this.deps = deps;
    this.state = initialState(cfg, deps.now());
  }

  readonly subscribe = (l: () => void): (() => void) => {
    this.listeners.add(l);
    return () => this.listeners.delete(l);
  };

  readonly getState = (): State => this.state;

  /**
   * Listens for other tabs first, then runs `boot` (the pair flow, which may store a
   * credential) under the runner's lifetime signal, then adopts what the store holds.
   * Startup and every notice take the same path, a reread of the store, so a change
   * committed by another tab during startup is not missed. A store that cannot be read
   * at startup shows as `storageFailed`, which `retryStorage` retries.
   */
  async start(boot: (signal: AbortSignal) => Promise<unknown> = async () => {}): Promise<void> {
    this.unlisten = this.deps.store.listen(() => {
      void this.reread("keep");
    });
    await boot(this.lifetime.signal).catch(() => null);
    if (this.stopped) return;
    await this.reread("report");
  }

  /** Reads the store again after it failed, as at startup. */
  async retryStorage(): Promise<void> {
    await this.reread("report");
  }

  /** Ends every request, socket, and timer, and stops listening; the runner does nothing after. */
  stop(): void {
    this.stopped = true;
    this.lifetime.abort();
    this.unlisten?.();
    this.unlisten = null;
    for (const ac of this.aborts.values()) ac.abort();
    this.aborts.clear();
    for (const ac of this.tickets.values()) ac.abort();
    this.tickets.clear();
    for (const t of this.opening.values()) clearTimeout(t);
    this.opening.clear();
    for (const ws of this.sockets.values()) {
      ws.onopen = null;
      ws.onmessage = null;
      ws.onclose = null;
      ws.close();
    }
    this.sockets.clear();
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
  }

  /**
   * Rereads the store and adopts what it holds. If it cannot be read, `keep` keeps the
   * current credential and `report` shows the storage failure.
   *
   * The one adoption rule: a reread triggered by a generation (a 401's clear) acts only
   * if that generation is still the current one when the read completes; then a read
   * failure retires it. Notices, startup, and Retry name no generation and adopt what
   * the store holds.
   */
  private reread(onError: "keep" | "report", trigger?: string): Promise<void> {
    this.adoption = this.adoption.then(async () => {
      if (this.stopped) return;
      let cred: Credential | null;
      let failed = false;
      try {
        cred = await this.deps.store.read();
      } catch {
        failed = true;
        cred = null;
      }
      if (trigger !== undefined) {
        if (this.cred?.generation !== trigger) return;
        this.adopt(cred);
        return;
      }
      if (failed) {
        if (onError === "report") this.dispatch({ type: "storageFailed" });
        return;
      }
      this.adopt(cred);
    });
    return this.adoption;
  }

  /** Adopts `cred`, unless it is a generation retired here after a failed delete. */
  private adopt(cred: Credential | null): void {
    if (this.stopped) return;
    const usable = cred !== null && this.retired.has(cred.generation) ? null : cred;
    this.cred = usable;
    this.dispatch({ type: "credential", generation: usable?.generation ?? null });
  }

  dispatch(input: Input): void {
    if (this.stopped) return;
    const [next, effects] = reduce(this.state, { ...input, now: this.deps.now() } as Action);
    this.state = next;
    for (const e of effects) this.run(e);
    for (const l of this.listeners) l();
  }

  private tokenFor(generation: string): string | null {
    return this.cred?.generation === generation ? this.cred.token : null;
  }

  private run(e: Effect): void {
    switch (e.type) {
      case "fetch": {
        const token = this.tokenFor(e.req.generation);
        const ac = new AbortController();
        this.aborts.set(e.req.id, ac);
        const id = e.req.id;
        const done = (result: FetchResult) => {
          this.aborts.delete(id);
          this.dispatch({ type: "response", id, result });
        };
        if (token === null) {
          // A generation already retired: the reducer discards the answer.
          queueMicrotask(() => done({ kind: "timeout" }));
          return;
        }
        void readEvents(this.deps.fetch, e.req, token, ac.signal, this.state.cfg.readMaxBytes).then(
          done,
        );
        return;
      }
      case "abort":
        this.aborts.get(e.id)?.abort();
        this.aborts.delete(e.id);
        return;
      case "ticket": {
        const token = this.tokenFor(e.generation);
        const sock = e.socket;
        if (token === null) {
          queueMicrotask(() =>
            this.dispatch({ type: "ticket", socket: sock, result: { kind: "transport" } }),
          );
          return;
        }
        const ac = new AbortController();
        this.tickets.set(sock, ac);
        void postTicket(this.deps.fetch, token, ac.signal).then((result) => {
          this.tickets.delete(sock);
          this.dispatch({ type: "ticket", socket: sock, result });
        });
        return;
      }
      case "connect":
        this.connect(e.socket, e.ticket);
        return;
      case "send": {
        const ws = this.sockets.get(e.socket);
        if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(e.msg));
        return;
      }
      case "close":
        // A socket still waiting for its ticket gives the POST up.
        this.tickets.get(e.socket)?.abort();
        this.tickets.delete(e.socket);
        clearTimeout(this.opening.get(e.socket));
        this.opening.delete(e.socket);
        this.sockets.get(e.socket)?.close();
        this.sockets.delete(e.socket);
        return;
      case "wake": {
        if (this.timer !== null) clearTimeout(this.timer);
        this.timer = null;
        if (e.at === null) return;
        const delay = Math.max(0, e.at - this.deps.now());
        this.timer = setTimeout(() => {
          this.timer = null;
          this.dispatch({ type: "tick" });
        }, delay);
        return;
      }
      case "clearCredential": {
        // Compare-and-delete, then a reread triggered by this generation. If the delete
        // failed and the generation is still current, it is retired here, so a reread
        // that still returns it does not reuse it; a newer generation is never touched.
        const gen = e.generation;
        void this.deps.store.clearIf(gen).then(
          () => this.reread("keep", gen),
          () => {
            if (this.cred?.generation === gen) this.retired.add(gen);
            return this.reread("keep", gen);
          },
        );
        return;
      }
    }
  }

  private connect(id: number, ticket: string): void {
    let ws: WebSocket;
    try {
      ws = this.deps.openSocket(
        this.deps.socketUrl(`/api/ws?ticket=${encodeURIComponent(ticket)}`),
      );
    } catch {
      queueMicrotask(() => this.dispatch({ type: "socketClosed", socket: id }));
      return;
    }
    this.sockets.set(id, ws);
    // A socket that does not open in time is closed and counts as closed.
    const deadline = setTimeout(() => {
      this.opening.delete(id);
      ws.onopen = null;
      ws.onclose = null;
      ws.close();
      this.sockets.delete(id);
      this.dispatch({ type: "socketClosed", socket: id });
    }, SOCKET_OPEN_TIMEOUT_MS);
    this.opening.set(id, deadline);
    // Every callback is bound to this socket's id; the reducer ignores a superseded one.
    ws.onopen = () => {
      clearTimeout(deadline);
      this.opening.delete(id);
      this.dispatch({ type: "socketOpen", socket: id });
    };
    ws.onmessage = (ev: MessageEvent) => {
      if (typeof ev.data !== "string") return;
      let msg: unknown;
      try {
        msg = JSON.parse(ev.data);
      } catch {
        return;
      }
      if (validators.WsServerMessage(msg)) {
        this.dispatch({ type: "socketMessage", socket: id, msg });
      }
    };
    ws.onclose = () => {
      clearTimeout(deadline);
      this.opening.delete(id);
      this.sockets.delete(id);
      this.dispatch({ type: "socketClosed", socket: id });
    };
  }
}

/** The runner over the page's own globals. */
export function browserRunner(store: CredentialStore): Runner {
  return new Runner({
    fetch: (input, init) => fetch(input, init),
    openSocket: (url) => new WebSocket(url),
    store,
    now: () => Date.now(),
    socketUrl: (path) => {
      const u = new URL(path, location.href);
      u.protocol = u.protocol === "https:" ? "wss:" : "ws:";
      return u.toString();
    },
  });
}
