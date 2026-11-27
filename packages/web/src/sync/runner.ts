// The runner: the only module holding browser handles (fetch, WebSocket, timers, the
// credential store). It feeds actions to the pure reducer and carries out its effects;
// React reads the state through `useSyncExternalStore(runner.subscribe, runner.getState)`.

import { validators } from "@chaldea/contracts";

import type { Credential, CredentialStore } from "../auth/credential";
import { type Fetch, postTicket, readEvents } from "../http";
import type { Action, Effect } from "./effects";
import { reduce } from "./reducer";
import { type Config, DEFAULT_CONFIG, initialState, type State } from "./state";

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
  private readonly sockets = new Map<number, WebSocket>();
  private timer: ReturnType<typeof setTimeout> | null = null;
  private cred: Credential | null = null;
  private readonly deps: RunnerDeps;

  constructor(deps: RunnerDeps, cfg: Config = DEFAULT_CONFIG) {
    this.deps = deps;
    this.state = initialState(cfg, deps.now());
  }

  readonly subscribe = (l: () => void): (() => void) => {
    this.listeners.add(l);
    return () => this.listeners.delete(l);
  };

  readonly getState = (): State => this.state;

  /** Adopts the credential read at start (or after pairing) and listens for other tabs. */
  start(cred: Credential | null): () => void {
    this.adopt(cred);
    return this.deps.store.listen(() => {
      void this.reread();
    });
  }

  private async reread(): Promise<void> {
    try {
      this.adopt(await this.deps.store.read());
    } catch {
      // The store could not be read: keep the current credential.
    }
  }

  private adopt(cred: Credential | null): void {
    this.cred = cred;
    this.dispatch({ type: "credential", generation: cred?.generation ?? null });
  }

  dispatch(input: Input): void {
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
        const done = (result: Parameters<typeof this.dispatchResponse>[1]) =>
          this.dispatchResponse(id, result);
        if (token === null) {
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
        void postTicket(this.deps.fetch, token).then((result) =>
          this.dispatch({ type: "ticket", socket: sock, result }),
        );
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
        this.sockets.get(e.socket)?.close();
        this.sockets.delete(e.socket);
        return;
      case "wake": {
        if (this.timer !== null) clearTimeout(this.timer);
        const delay = Math.max(0, e.at - this.deps.now());
        this.timer = setTimeout(() => {
          this.timer = null;
          this.dispatch({ type: "tick" });
        }, delay);
        return;
      }
      case "clearCredential": {
        const gen = e.generation;
        void this.deps.store.clearIf(gen).then(
          (cur) => this.adopt(cur),
          () => this.adopt(this.cred?.generation === gen ? null : this.cred),
        );
        return;
      }
    }
  }

  private dispatchResponse(id: number, result: import("./effects").FetchResult): void {
    this.aborts.delete(id);
    this.dispatch({ type: "response", id, result });
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
    // Every callback is bound to this socket's id; the reducer ignores a superseded one.
    ws.onopen = () => this.dispatch({ type: "socketOpen", socket: id });
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
