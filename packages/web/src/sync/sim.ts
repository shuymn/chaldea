// Test harness: the reducer against the model server over a network that can lose,
// delay, and reorder requests, responses, and frames (test only).

import type { WsServerMessage } from "@chaldea/contracts/generated/WsServerMessage";

import type { Action, Effect, FetchResult } from "./effects";
import { type ModelConfig, ModelServer, ModelSocket } from "./model/server";
import { reduce } from "./reducer";
import { type Config, DEFAULT_CONFIG, initialState, type Request, type State } from "./state";

type DistributiveOmit<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;

export interface Flight {
  readonly req: Request;
  /** The request was lost: it is never served, and the client's timeout fires. */
  lost: boolean;
  /** The client made it older (the runner aborted it); its answer may still arrive. */
  aborted: boolean;
}

export interface Delivery {
  readonly id: number;
  readonly result: FetchResult;
  /** The answer came from this database epoch (diagnostics). */
  readonly epoch: string;
}

interface Conn {
  readonly id: number;
  readonly server: ModelSocket;
  opened: boolean;
}

export const TEST_CONFIG: Config = {
  ...DEFAULT_CONFIG,
  forwardLimit: 4,
  backwardLimit: 3,
  followMax: 2,
  windowMaxEvents: 6,
  // The window cap equals the read limit, as in production. The largest model event
  // (2,500 code points of Japanese, about 8.5 KB) fits a backward page of three, so a tail
  // read is never split, while a forward page of four can be oversized.
  windowMaxBytes: 32 * 1024,
  readMaxBytes: 32 * 1024,
};

export const MODEL_CONFIG: ModelConfig = { replayMaxEvents: 6, replayMaxBytes: 4096, recent: 1 };

export class Sim {
  state: State;
  readonly server: ModelServer;
  now = 0;
  readonly flights = new Map<number, Flight>();
  readonly responses: Delivery[] = [];
  readonly tickets: number[] = [];
  readonly opens: { socket: number; ticket: string }[] = [];
  readonly conns = new Map<number, Conn>();
  readonly frames: { socket: number; msg: WsServerMessage }[] = [];
  readonly log: Effect[] = [];
  up = true;
  /** Every frame but `hello` is lost: the client learns heads from its reads only. */
  withholdHeads = false;
  /** The credential store: the generation stored, or null. */
  stored: string | null = "gen-1";
  private ticketN = 1;
  /** Bodies over this many bytes are refused unparsed, as the runner does. */
  readonly readMaxBytes: number;

  constructor(cfg: Config = TEST_CONFIG, model: ModelConfig = MODEL_CONFIG) {
    this.state = initialState(cfg, 0);
    this.server = new ModelServer(model);
    this.readMaxBytes = cfg.readMaxBytes;
  }

  dispatch(a: DistributiveOmit<Action, "now">): Effect[] {
    const [next, effects] = reduce(this.state, { ...a, now: this.now } as Action);
    this.state = next;
    for (const e of effects) this.effect(e);
    this.log.push(...effects);
    return effects;
  }

  private effect(e: Effect): void {
    switch (e.type) {
      case "fetch":
        this.flights.set(e.req.id, { req: e.req, lost: false, aborted: false });
        return;
      case "abort": {
        const f = this.flights.get(e.id);
        if (f) f.aborted = true;
        return;
      }
      case "ticket":
        this.tickets.push(e.socket);
        return;
      case "connect":
        this.opens.push({ socket: e.socket, ticket: e.ticket });
        return;
      case "send": {
        const c = this.conns.get(e.socket);
        if (c && e.msg.type === "subscribe") c.server.inbox.push([...e.msg.stream_ids]);
        return;
      }
      case "close": {
        const c = this.conns.get(e.socket);
        c?.server.close();
        this.conns.delete(e.socket);
        return;
      }
      case "wake":
        return;
      case "clearCredential":
        // Compare-and-delete in the model store.
        if (this.stored === e.generation) this.stored = null;
        this.dispatch({ type: "credential", generation: this.stored });
        return;
    }
  }

  // ---- network steps ---------------------------------------------------------------------

  /** Serves a request in flight: the server answers now, from its current database. */
  serve(id: number): boolean {
    const f = this.flights.get(id);
    if (!f || f.lost) return false;
    this.flights.delete(id);
    if (!this.up) {
      this.responses.push({ id, result: { kind: "transport" }, epoch: "" });
      return true;
    }
    const r = f.req;
    const a = this.server.answer({
      stream: r.stream,
      afterSeq: r.afterSeq,
      beforeSeq: r.beforeSeq,
      limit: r.limit,
      epoch: r.epoch,
    });
    let result: FetchResult;
    if (a.status === 200) {
      const size = new TextEncoder().encode(JSON.stringify(a.body)).length;
      result = size > this.readMaxBytes ? { kind: "oversize" } : { kind: "ok", body: a.body };
    } else {
      result = { kind: "status", status: a.status, retryAfter: null };
    }
    this.responses.push({ id, result, epoch: this.server.db.epoch });
    return true;
  }

  /** Answers a request with a given failure (408, 503, transport, ...). */
  fail(id: number, result: FetchResult): boolean {
    if (!this.flights.delete(id)) return false;
    this.responses.push({ id, result, epoch: "" });
    return true;
  }

  deliver(index = 0): boolean {
    const d = this.responses.splice(index, 1)[0];
    if (!d) return false;
    this.dispatch({ type: "response", id: d.id, result: d.result });
    return true;
  }

  loseRequest(id: number): void {
    const f = this.flights.get(id);
    if (f) f.lost = true;
  }

  loseResponse(index: number): void {
    const d = this.responses.splice(index, 1)[0];
    if (d) this.flights.set(d.id, { req: this.reqOf(d.id), lost: true, aborted: false });
  }

  private reqOf(id: number): Request {
    const r = this.state.inflight[id];
    if (r) return r;
    return {
      id,
      stream: "host",
      dir: "forward",
      purpose: "forward",
      afterSeq: 0,
      beforeSeq: null,
      limit: 1,
      epoch: null,
      generation: "",
    };
  }

  /** The client's timeout of a lost request. */
  timeout(id: number): boolean {
    const f = this.flights.get(id);
    if (!f || !f.lost) return false;
    this.flights.delete(id);
    if (!f.aborted) this.dispatch({ type: "response", id, result: { kind: "timeout" } });
    return true;
  }

  serveTicket(index = 0): boolean {
    const sock = this.tickets.splice(index, 1)[0];
    if (sock === undefined) return false;
    if (!this.up) {
      this.dispatch({ type: "ticket", socket: sock, result: { kind: "transport" } });
    } else if (!this.server.db.tokens) {
      this.dispatch({ type: "ticket", socket: sock, result: { kind: "status", status: 401, retryAfter: null } });
    } else {
      this.dispatch({ type: "ticket", socket: sock, result: { kind: "ok", ticket: `t${this.ticketN++}` } });
    }
    return true;
  }

  open(index = 0): boolean {
    const o = this.opens.splice(index, 1)[0];
    if (!o) return false;
    if (!this.up) {
      this.dispatch({ type: "socketClosed", socket: o.socket });
      return true;
    }
    const conn: Conn = { id: o.socket, server: new ModelSocket(this.server), opened: true };
    this.conns.set(o.socket, conn);
    this.dispatch({ type: "socketOpen", socket: o.socket });
    return true;
  }

  /** The server takes in one subscribe and selects one frame, which goes in transit. */
  socketStep(socket: number): boolean {
    const c = this.conns.get(socket);
    if (!c) return false;
    const got = c.server.receive();
    const msg = c.server.select();
    if (msg) this.frames.push({ socket, msg });
    return got || msg !== null;
  }

  deliverFrame(index = 0): boolean {
    const f = this.frames.splice(index, 1)[0];
    if (!f) return false;
    if (this.withholdHeads && f.msg.type !== "hello") return true;
    if (this.conns.has(f.socket)) {
      this.dispatch({ type: "socketMessage", socket: f.socket, msg: f.msg });
    }
    return true;
  }

  /**
   * Loses a frame. `hello` is never lost: it is the socket's first frame, and losing it
   * means losing the connection (formal-specs §7.1 does not model it either).
   */
  dropFrame(index = 0): void {
    if (this.frames[index]?.msg.type === "hello") return;
    this.frames.splice(index, 1);
  }

  refresh(): void {
    for (const c of this.conns.values()) c.server.refresh();
  }

  /** A daemon restart: every socket and every request in flight dropped; answers already sent may still arrive. */
  restart(): void {
    for (const [id, c] of [...this.conns]) {
      c.server.close();
      this.conns.delete(id);
      this.dispatch({ type: "socketClosed", socket: id });
    }
    this.frames.length = 0;
    for (const f of this.flights.values()) f.lost = true;
    for (const t of this.tickets.splice(0)) {
      this.dispatch({ type: "ticket", socket: t, result: { kind: "transport" } });
    }
    for (const o of this.opens.splice(0)) this.dispatch({ type: "socketClosed", socket: o.socket });
  }

  advance(ms: number): void {
    this.now += ms;
    this.dispatch({ type: "tick" });
  }

  /** Writes on the server, announced to the sockets as commits. */
  boot(): void {
    this.dispatch({ type: "credential", generation: this.stored });
  }

  /** The id of the latest request in flight for `stream` in direction `dir`, if any. */
  flight(stream: string, dir: "forward" | "backward"): number | undefined {
    let found: number | undefined;
    for (const [id, f] of this.flights) if (f.req.stream === stream && f.req.dir === dir) found = id;
    return found;
  }

  /** Serves the latest request for `stream`/`dir` and returns the index of its answer. */
  serveOne(stream: string, dir: "forward" | "backward"): number {
    const id = this.flight(stream, dir);
    if (id === undefined) throw new Error(`no ${dir} request for ${stream} in flight`);
    this.serve(id);
    return this.responses.length - 1;
  }

  /** Delivers the answer to request `id`. */
  deliverId(id: number): void {
    const i = this.responses.findIndex((d) => d.id === id);
    if (i < 0) throw new Error(`no answer for ${id}`);
    this.deliver(i);
  }

  /** Brings a socket up: ticket, open, and the server's first frames. */
  connect(): void {
    while (this.serveTicket());
    while (this.open());
    for (const id of [...this.conns.keys()]) while (this.socketStep(id));
    while (this.deliverFrame());
  }

  // ---- quiesce ---------------------------------------------------------------------------

  /**
   * Restores connectivity, stops losses and writes, and runs one fair schedule for up to
   * 60 drain iterations (formal-specs §7.1 "Convergence"): responses, requests, the
   * socket, due fetches, timeouts of lost requests, then up to two heads refreshes.
   */
  iters = 0;
  quiesce(maxIterations = 60, maxRefreshes = 2): void {
    this.up = true;
    let refreshes = 0;
    for (let i = 0; i < maxIterations; i++) {
      let progress = false;
      this.iters = i;
      while (this.deliver()) progress = true;
      for (const id of [...this.flights.keys()]) progress = this.serve(id) || progress;
      for (const id of [...this.flights.keys()]) progress = this.timeout(id) || progress;
      while (this.serveTicket()) progress = true;
      while (this.open()) progress = true;
      for (const id of [...this.conns.keys()]) {
        while (this.socketStep(id)) progress = true;
      }
      while (this.deliverFrame()) progress = true;
      if (this.responses.length > 0 || this.flights.size > 0) continue;
      if (progress) continue;
      const wake = this.state.wakeAt;
      if (wake !== null && wake > this.now) {
        this.now = wake;
        this.dispatch({ type: "tick" });
        continue;
      }
      if (refreshes < maxRefreshes) {
        refreshes += 1;
        this.refresh();
        continue;
      }
      return;
    }
  }
}
