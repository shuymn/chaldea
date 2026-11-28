// The sync client's pure reducer, `(State, Action) → (State, Effect[])`
// (docs/design/w1-pwa.md §2.3, §2.4, D2). It implements the client rules of
// formal-specs §7.1 and mvp §2.4 "Replay", "Backward history", and "Live notifications".

import { decode } from "@chaldea/contracts";
import type { Content } from "@chaldea/contracts/generated/Content";
import type { EventEnvelope } from "@chaldea/contracts/generated/EventEnvelope";
import type { EventsResponse } from "@chaldea/contracts/generated/EventsResponse";

import type { WsServerMessage } from "@chaldea/contracts/generated/WsServerMessage";

import type { Action, Effect, FetchResult, TicketResult } from "./effects";
import {
  type Copy,
  HOST,
  type HostCopy,
  type HostRow,
  type InputView,
  listRows,
  newStream,
  type Pending,
  type Request,
  type State,
  type Stream,
  type StreamError,
  TERMINAL_INPUTS,
  TERMINAL_STATES,
  type ThreadCopy,
  threadOf,
  threadStream,
  type Window,
} from "./state";

type Mutable<T> = { -readonly [K in keyof T]: T[K] };

const OVERSIZE = "this thread's data exceeds the 16 MiB read limit";
const MISSING_HOST = "the daemon reported no host stream";

const utf8 = new TextEncoder();
/** The size the window counts for one event: its JSON in UTF-8 bytes. */
function eventBytes(e: EventEnvelope): number {
  return utf8.encode(JSON.stringify(e)).length;
}

export function reduce(prev: State, action: Action): [State, Effect[]] {
  const c = new Step(prev, action.now);
  c.apply(action);
  c.schedule();
  return [c.s, c.effects];
}

class Step {
  readonly s: Mutable<State>;
  readonly effects: Effect[] = [];
  private readonly touched = new Set<string>();

  constructor(prev: State, now: number) {
    this.s = {
      ...prev,
      now: Math.max(prev.now, now),
      streams: { ...prev.streams },
      inflight: { ...prev.inflight },
    };
  }

  // ---- helpers ------------------------------------------------------------------------

  private st(key: string): Mutable<Stream> | undefined {
    const cur = this.s.streams[key];
    if (!cur) return undefined;
    if (this.touched.has(key)) return cur as Mutable<Stream>;
    const copy = { ...cur };
    (this.s.streams as Record<string, Stream>)[key] = copy;
    this.touched.add(key);
    return copy;
  }

  private win(): Mutable<Window> | null {
    if (!this.s.window) return null;
    this.s.window = { ...this.s.window };
    return this.s.window as Mutable<Window>;
  }

  private displayedKey(): string | null {
    return this.s.selected === null ? null : threadStream(this.s.selected);
  }

  /** Makes one request older: it stops being current, and its slot is free. */
  private age(id: number | null): void {
    if (id === null || !this.s.inflight[id]) return;
    delete (this.s.inflight as Record<number, Request>)[id];
    this.effects.push({ type: "abort", id });
  }

  /** Every request issued so far stops being current (an epoch or socket change). */
  private olderAll(): void {
    for (const key of Object.keys(this.s.streams)) {
      const st = this.st(key);
      if (!st) continue;
      if (st.fwd !== null) {
        this.age(st.fwd);
        st.fwd = null;
        st.asked = true;
      }
      if (st.bwd !== null) {
        this.age(st.bwd);
        st.bwd = null;
      }
    }
    for (const id of Object.keys(this.s.inflight)) this.age(Number(id));
  }

  private olderBackward(key: string): void {
    const st = this.st(key);
    if (st && st.bwd !== null) {
      this.age(st.bwd);
      st.bwd = null;
    }
  }

  private failure(retryAfter: number | null, st: Mutable<Stream>): void {
    const b = this.s.cfg.backoffMs;
    const delay = b[Math.min(st.attempt, b.length - 1)] ?? 30000;
    st.attempt += 1;
    st.retryAt = this.s.now + Math.max(delay, (retryAfter ?? 0) * 1000);
    st.asked = true;
  }

  // ---- actions ------------------------------------------------------------------------

  apply(a: Action): void {
    switch (a.type) {
      case "tick":
        if (this.s.wakeAt !== null && this.s.now >= this.s.wakeAt) this.s.wakeAt = null;
        return;
      case "credential":
        this.s.storageFailed = false;
        return this.credential(a.generation);
      case "storageFailed":
        this.s.storageFailed = true;
        return;
      case "response":
        return this.response(a.id, a.result);
      case "ticket":
        return this.ticket(a.socket, a.result);
      case "socketOpen":
        return this.socketOpen(a.socket);
      case "socketMessage":
        return this.socketMessage(a.socket, a.msg);
      case "socketClosed":
        return this.socketClosed(a.socket);
      case "select":
        return this.select(a.thread);
      case "visible":
        this.s.visible = [...a.threads];
        this.refollow();
        return;
      case "older":
        return this.older();
      case "newer":
        return this.newer();
      case "latest":
        return this.latest();
      case "retry":
        return this.retry();
    }
  }

  private credential(generation: string | null): void {
    const changed = generation !== this.s.generation;
    this.s.checkingCredential = false;
    if (!changed) return;
    // Retire everything tagged with the old generation: responses, retries, the socket.
    this.olderAll();
    this.dropSocket();
    this.s.socketAttempt = 0;
    this.s.socketRetryAt = 0;
    this.s.generation = generation;
  }

  private dropSocket(): void {
    if (this.s.socket) this.effects.push({ type: "close", socket: this.s.socket.id });
    this.s.socket = null;
  }

  private unauthorized(generation: string): void {
    if (this.s.checkingCredential) return;
    this.s.checkingCredential = true;
    this.effects.push({ type: "clearCredential", generation });
  }

  private response(id: number, result: FetchResult): void {
    const req = this.s.inflight[id];
    if (!req) return; // older: discarded, changes nothing
    const st = this.s.streams[req.stream];
    const current =
      st !== undefined &&
      (req.dir === "forward" ? st.fwd : st.bwd) === id &&
      req.generation === this.s.generation;
    delete (this.s.inflight as Record<number, Request>)[id];
    if (!current) return;
    const ms = this.st(req.stream);
    if (!ms) return;
    if (req.dir === "forward") ms.fwd = null;
    else ms.bwd = null;

    // Forward reads alone record a stream's reachability; a history failure shows only
    // through the window's error and Retry.
    const down = result.kind === "transport" || result.kind === "timeout";
    if (req.dir === "forward") ms.lastRead = down ? "down" : "ok";
    if (!down) {
      this.s.ticketDown = false;
      this.s.reached = true;
    }
    if (result.kind === "status" && result.status === 401) {
      if (req.dir === "forward") ms.asked = true;
      this.unauthorized(req.generation);
      return;
    }
    if (result.kind === "status" && result.status === 404) {
      if (req.stream === HOST) {
        ms.error = { kind: "missing_host", message: MISSING_HOST };
      } else {
        this.vanish(threadOf(req.stream) ?? "");
      }
      return;
    }
    if (req.dir === "forward") this.forwardResult(req, ms, result);
    else this.backwardResult(req, result);
  }

  // ---- forward ------------------------------------------------------------------------

  private forwardResult(req: Request, st: Mutable<Stream>, r: FetchResult): void {
    switch (r.kind) {
      case "ok":
        if (r.body.stream_id !== req.stream) {
          st.error = { kind: "schema", message: "the daemon answered for another stream" };
          return;
        }
        this.forwardOk(req, r.body);
        this.syncedCheck();
        return;
      case "transport":
      case "timeout":
        this.failure(null, st);
        return;
      case "status":
        if (r.status === 408 || r.status === 503) {
          this.failure(r.retryAfter, st);
        } else {
          st.error = { kind: "status", message: `the daemon refused the read (${r.status})` };
        }
        return;
      case "oversize":
        if (st.fwdLimit > 1) {
          st.fwdLimit = Math.floor(st.fwdLimit / 2);
          st.asked = true;
        } else {
          st.error = { kind: "oversize", message: OVERSIZE };
        }
        return;
      case "schema":
        st.error = { kind: "schema", message: "the daemon's answer did not match its schema" };
        return;
    }
  }

  private forwardOk(req: Request, body: EventsResponse): void {
    const key = req.stream;
    const switching = this.s.epoch === null || body.epoch !== this.s.epoch;
    // A resync asked for after this request was issued stays due.
    const askedBefore = this.s.streams[key]?.asked ?? false;
    if (switching) this.switchEpoch(body.epoch);
    const st = this.st(key);
    if (!st) return;
    st.asked = askedBefore;
    st.attempt = 0;
    st.retryAt = 0;
    st.error = null;
    st.fwdLimit = this.s.cfg.forwardLimit;
    if (body.mode === "replay") {
      let copy = st.copy;
      if (copy === null) {
        // A page becomes the copy only if it starts at the stream's beginning.
        if (req.afterSeq !== 0) {
          st.asked = true;
          return;
        }
        copy = emptyCopy(key);
      }
      if (req.afterSeq !== copy.cursor) {
        st.asked = true;
        return;
      }
      const displayed = this.displayedKey() === key;
      for (const e of body.events) {
        if (e.seq !== copy.cursor + 1) {
          st.asked = true;
          break;
        }
        copy = applyEvent(copy, e);
        if (displayed) this.liveAppend(e);
      }
      st.copy = copy;
      st.hasMore = body.has_more;
      st.knownHead = body.head_seq;
      if (key === HOST) this.refollow();
      this.ensureWindow();
      return;
    }
    // A snapshot replaces the copy and makes the stream's other requests older.
    if (!switching) this.olderBackward(key);
    st.copy = snapshotCopy(body);
    st.hasMore = false;
    st.knownHead = body.as_of_seq;
    if (this.displayedKey() === key && !switching) {
      const w = this.s.window;
      if (body.reason === "cursor_ahead") {
        // The window may hold events this epoch's stream no longer has.
        this.s.window = null;
      } else if (w?.attached) {
        // A bound snapshot resets an attached window to the new tail.
        this.s.window = null;
      }
    }
    if (key === HOST) this.refollow();
    this.ensureWindow();
  }

  private switchEpoch(epoch: string): void {
    this.olderAll();
    this.s.epoch = epoch;
    this.s.gone = {};
    this.s.window = null;
    for (const key of Object.keys(this.s.streams)) {
      const st = this.st(key);
      if (!st) continue;
      st.copy = null;
      st.hasMore = false;
      st.knownHead = null;
      st.asked = true;
      st.error = null;
      st.attempt = 0;
      st.retryAt = 0;
    }
  }

  /** "Last synced": every followed stream not in error caught up after this response. */
  private syncedCheck(): void {
    const sockEpoch = this.s.socket?.epoch ?? null;
    for (const key of this.s.followed) {
      const st = this.s.streams[key];
      if (!st) return;
      if (st.error) continue;
      if (st.copy === null || st.asked || st.hasMore || st.fwd !== null) return;
      if (st.knownHead === null || st.copy.cursor < st.knownHead) return;
      if (
        st.announced !== null &&
        sockEpoch === this.s.epoch &&
        st.announced > st.copy.cursor
      ) {
        return;
      }
    }
    this.s.lastSyncedAt = this.s.now;
  }

  // ---- backward and the window ---------------------------------------------------------

  private backwardResult(req: Request, r: FetchResult): void {
    const thread = threadOf(req.stream);
    const w = this.s.window;
    if (r.kind === "ok" && r.body.epoch !== this.s.epoch) {
      // A backward page of another epoch switches nothing: it makes a forward resync due,
      // which stays due until a forward request is issued after it.
      const st = this.st(req.stream);
      if (st) {
        st.asked = true;
        st.resync = true;
      }
      return;
    }
    if (!w || w.thread !== thread || !w.pending) return;
    const mw = this.win();
    if (!mw) return;
    const pending = mw.pending;
    if (!pending) return;
    switch (r.kind) {
      case "ok": {
        if (r.body.mode !== "replay" || r.body.stream_id !== req.stream) {
          mw.error = { kind: "schema", message: "the daemon's answer did not match its schema" };
          return;
        }
        mw.limit = this.s.cfg.backwardLimit;
        mw.error = null;
        mw.pending = null;
        if (pending.purpose === "newer") this.mergeNewer(mw, pending.last, r.body.events);
        else this.mergeOlder(mw, pending.purpose, r.body.events);
        return;
      }
      case "oversize":
        if (pending.purpose === "newer") {
          if (pending.n > 1) {
            mw.pending = { purpose: "newer", last: pending.last, n: Math.floor(pending.n / 2) };
            return;
          }
        } else if (mw.limit > 1) {
          mw.limit = Math.floor(mw.limit / 2);
          return;
        }
        mw.error = { kind: "oversize", message: OVERSIZE };
        return;
      default:
        // A failed backward read is not retried automatically.
        mw.error = backwardError(r);
        return;
    }
  }

  /** Prepends a backward page that ends just below `lo`. */
  private mergeOlder(w: Mutable<Window>, purpose: "tail" | "older", page: readonly EventEnvelope[]) {
    const events = page.filter((e) => e.seq < w.lo);
    if (events.length === 0) return;
    if (!contiguous(events) || events[events.length - 1]?.seq !== w.lo - 1) return;
    const merged = [...events, ...w.events];
    const bytes = w.bytes + events.reduce((n, e) => n + eventBytes(e), 0);
    const first = events[0]?.seq ?? w.lo;
    if (purpose === "tail") {
      // Filling the tail: past the cap the oldest drop.
      setWindow(w, first, merged, bytes);
      this.trimLo(w);
    } else {
      // Older: past the cap the newest drop, detaching the window from live events.
      setWindow(w, first, merged, bytes);
      if (this.trimHi(w)) w.attached = false;
    }
  }

  /** Appends a "newer" page above the captured end `last`, up to the current cursor. */
  private mergeNewer(w: Mutable<Window>, last: number, page: readonly EventEnvelope[]) {
    const cursor = this.cursorOf(threadStream(w.thread));
    if (w.hi !== last || cursor === null) return;
    const events = page.filter((e) => e.seq > last && e.seq <= cursor);
    if (events.length > 0 && contiguous(events) && events[0]?.seq === last + 1) {
      const bytes = w.bytes + events.reduce((n, e) => n + eventBytes(e), 0);
      setWindow(w, w.lo, [...w.events, ...events], bytes);
      this.trimLo(w);
    }
    if (w.hi === cursor) w.attached = true;
  }

  private cursorOf(key: string): number | null {
    return this.s.streams[key]?.copy?.cursor ?? null;
  }

  /** Drops from the old end until within the caps. */
  private trimLo(w: Mutable<Window>): void {
    const { windowMaxEvents, windowMaxBytes } = this.s.cfg;
    let drop = 0;
    let bytes = w.bytes;
    while (
      drop < w.events.length &&
      (w.events.length - drop > windowMaxEvents || bytes > windowMaxBytes)
    ) {
      const e = w.events[drop];
      if (e) bytes -= eventBytes(e);
      drop += 1;
    }
    if (drop > 0) setWindow(w, w.lo + drop, w.events.slice(drop), bytes);
  }

  /** Drops from the new end until within the caps; true when anything dropped. */
  private trimHi(w: Mutable<Window>): boolean {
    const { windowMaxEvents, windowMaxBytes } = this.s.cfg;
    let keep = w.events.length;
    let bytes = w.bytes;
    while (keep > 0 && (keep > windowMaxEvents || bytes > windowMaxBytes)) {
      const e = w.events[keep - 1];
      if (e) bytes -= eventBytes(e);
      keep -= 1;
    }
    if (keep === w.events.length) return false;
    setWindow(w, w.lo, w.events.slice(0, keep), bytes);
    return true;
  }

  /** A live forward event of the displayed thread: appended while the window is attached. */
  private liveAppend(e: EventEnvelope): void {
    const w = this.s.window;
    if (!w || !w.attached || e.seq !== w.hi + 1) return;
    const mw = this.win();
    if (!mw) return;
    setWindow(mw, mw.lo, [...mw.events, e], mw.bytes + eventBytes(e));
    this.trimLo(mw);
    this.s.appended += 1;
  }

  /** Opens the displayed thread's window once its copy is valid: one tail read. */
  private ensureWindow(): void {
    const t = this.s.selected;
    if (t === null) {
      this.s.window = null;
      return;
    }
    if (this.s.window?.thread === t) return;
    const cursor = this.cursorOf(threadStream(t));
    if (cursor === null) {
      this.s.window = null;
      return;
    }
    this.s.window = {
      id: this.s.nextId++,
      thread: t,
      lo: cursor + 1,
      hi: cursor,
      events: [],
      bytes: 0,
      attached: true,
      pending: cursor > 0 ? { purpose: "tail" } : null,
      limit: this.s.cfg.backwardLimit,
      error: null,
    };
  }

  private select(thread: string | null): void {
    // Selecting the displayed row again keeps its window; Jump to latest resets it.
    if (thread !== null && thread === this.s.selected) return;
    const before = this.displayedKey();
    if (before !== null) this.olderBackward(before);
    this.s.selected = thread !== null && this.s.gone[thread] ? null : thread;
    this.s.window = null;
    this.refollow();
    this.ensureWindow();
  }

  private older(): void {
    const w = this.s.window;
    if (!w || w.pending || w.lo <= 1) return;
    const mw = this.win();
    if (!mw) return;
    mw.pending = { purpose: "older" };
    mw.error = null;
  }

  private newer(): void {
    const w = this.s.window;
    if (!w || w.pending) return;
    const cursor = this.cursorOf(threadStream(w.thread));
    if (cursor === null || w.hi >= cursor) return;
    const mw = this.win();
    if (!mw) return;
    mw.pending = {
      purpose: "newer",
      last: w.hi,
      n: Math.min(this.s.cfg.backwardLimit, cursor - w.hi),
    };
    mw.error = null;
  }

  private latest(): void {
    const key = this.displayedKey();
    if (key === null) return;
    this.olderBackward(key);
    this.s.window = null;
    this.ensureWindow();
  }

  private retry(): void {
    for (const key of this.s.followed) {
      const st = this.st(key);
      if (st?.error) {
        st.error = null;
        st.asked = true;
        st.fwdLimit = this.s.cfg.forwardLimit;
      }
    }
    const w = this.s.window;
    if (w?.error) {
      const mw = this.win();
      if (mw) {
        mw.error = null;
        mw.limit = this.s.cfg.backwardLimit;
      }
    }
  }

  // ---- following ----------------------------------------------------------------------

  /** A thread 404: its copy, selection, and row go for this epoch. */
  private vanish(thread: string): void {
    this.s.gone = { ...this.s.gone, [thread]: true };
    if (this.s.selected === thread) {
      this.s.selected = null;
      this.s.window = null;
    }
    this.refollow();
  }

  /**
   * The followed set: `host`, the selected thread, and up to `followMax` open threads,
   * visible first in list order. A followed thread that scrolled away stays until a
   * newly visible thread needs its slot (the hidden one last in list order goes first).
   */
  private refollow(): void {
    const keys: string[] = [HOST];
    const sel = this.s.selected;
    if (sel !== null && !this.s.gone[sel]) keys.push(threadStream(sel));
    const host = this.s.streams[HOST]?.copy;
    let threads: string[];
    if (host?.kind !== "host") {
      // No list yet (or an epoch switch dropped it): keep what is followed.
      threads = this.s.followed
        .map(threadOf)
        .filter((t): t is string => t !== null && t !== sel && !this.s.gone[t]);
    } else {
      const open = listRows(this.s)
        .filter((r) => !r.info.closed)
        .map((r) => r.info.thread_id);
      const order = new Map(open.map((t, i) => [t, i]));
      const visible = new Set(this.s.visible);
      threads = this.s.followed
        .map(threadOf)
        .filter((t): t is string => t !== null && t !== sel && order.has(t));
      const max = this.s.cfg.followMax;
      for (const t of open) {
        if (!visible.has(t) || threads.includes(t) || t === sel) continue;
        if (threads.length < max) {
          threads.push(t);
          continue;
        }
        const hidden = threads
          .filter((x) => !visible.has(x))
          .sort((a, b) => (order.get(b) ?? 0) - (order.get(a) ?? 0));
        const evict = hidden[0];
        if (evict === undefined) break;
        threads = threads.filter((x) => x !== evict);
        threads.push(t);
      }
      while (threads.length > max) threads.pop();
      threads.sort((a, b) => (order.get(a) ?? 0) - (order.get(b) ?? 0));
    }
    for (const t of threads) keys.push(threadStream(t));
    this.setFollowed(keys);
  }

  private setFollowed(keys: string[]): void {
    const next = new Set(keys);
    for (const key of Object.keys(this.s.streams)) {
      if (next.has(key)) continue;
      // Unfollowing drops the copy and makes its requests older.
      const st = this.s.streams[key];
      if (st) {
        this.age(st.fwd);
        this.age(st.bwd);
      }
      delete (this.s.streams as Record<string, Stream>)[key];
      this.touched.delete(key);
    }
    for (const key of keys) {
      if (!this.s.streams[key]) {
        (this.s.streams as Record<string, Stream>)[key] = newStream(this.s.cfg);
        this.touched.add(key);
      }
    }
    this.s.followed = keys;
  }

  // ---- socket -------------------------------------------------------------------------

  private ticket(id: number, r: TicketResult): void {
    const sock = this.s.socket;
    if (!sock || sock.id !== id || sock.phase !== "ticket") return;
    switch (r.kind) {
      case "ok":
        this.s.ticketDown = false;
        this.s.socket = { ...sock, phase: "connecting" };
        this.effects.push({ type: "connect", socket: id, ticket: r.ticket });
        return;
      case "transport":
        this.s.ticketDown = true;
        this.socketClosed(id);
        return;
      case "status":
        this.s.ticketDown = false;
        this.s.reached = true;
        this.socketClosed(id);
        if (r.status === 401 && typeof this.s.generation === "string") {
          this.unauthorized(this.s.generation);
        }
        return;
    }
  }

  private socketOpen(id: number): void {
    const sock = this.s.socket;
    if (!sock || sock.id !== id || sock.phase !== "connecting") return;
    this.s.socket = { ...sock, phase: "open", epoch: null, subscribed: null };
    // Every open makes earlier requests older, forgets announced heads, and makes every
    // followed stream and pending history due; a failure waits for no backoff.
    this.olderAll();
    for (const key of this.s.followed) {
      const st = this.st(key);
      if (!st) continue;
      st.announced = null;
      st.asked = true;
      st.error = null;
      st.attempt = 0;
      st.retryAt = 0;
      st.fwdLimit = this.s.cfg.forwardLimit;
    }
    const w = this.s.window;
    if (w?.error) {
      const mw = this.win();
      if (mw) mw.error = null;
    }
    // The full set goes in `schedule`, as `subscribed` is empty.
  }

  private socketMessage(id: number, msg: WsServerMessage): void {
    const sock = this.s.socket;
    if (!sock || sock.id !== id || sock.phase !== "open") return;
    if (msg.type === "hello") {
      // `hello` sets the socket's epoch only.
      this.s.socket = { ...sock, epoch: msg.epoch };
      this.s.socketAttempt = 0;
      this.s.reached = true;
      return;
    }
    if (sock.epoch === null) return;
    const entries =
      msg.type === "heads"
        ? msg.heads
        : [{ stream_id: msg.stream_id, head_seq: msg.head_seq }];
    for (const { stream_id, head_seq } of entries) {
      // Followed ids only, merged by max; an omission changes nothing.
      const st = this.s.streams[stream_id] ? this.st(stream_id) : undefined;
      if (!st) continue;
      if (st.announced === null || head_seq > st.announced) st.announced = head_seq;
    }
  }

  private socketClosed(id: number): void {
    const sock = this.s.socket;
    if (!sock || sock.id !== id) return;
    this.s.socket = null;
    const { socketBackoffMinMs: min, socketBackoffMaxMs: max } = this.s.cfg;
    const delay = Math.min(max, min * 2 ** this.s.socketAttempt);
    this.s.socketAttempt += 1;
    this.s.socketRetryAt = this.s.now + delay;
  }

  // ---- scheduling ---------------------------------------------------------------------

  private forwardDue(st: Stream): boolean {
    if (st.fwd !== null || st.error !== null || st.retryAt > this.s.now) return false;
    if (st.asked || st.hasMore) return true;
    const sockEpoch = this.s.socket?.epoch ?? null;
    return (
      st.announced !== null &&
      this.s.epoch !== null &&
      sockEpoch === this.s.epoch &&
      (st.copy === null || st.announced > st.copy.cursor)
    );
  }

  private issue(req: Omit<Request, "id" | "epoch" | "generation">): Request | null {
    const generation = this.s.generation;
    if (typeof generation !== "string") return null;
    const full: Request = { ...req, id: this.s.nextId, epoch: this.s.epoch, generation };
    this.s.nextId += 1;
    (this.s.inflight as Record<number, Request>)[full.id] = full;
    this.effects.push({ type: "fetch", req: full });
    return full;
  }

  schedule(): void {
    const s = this.s;
    this.ensureFollowed();
    if (typeof s.generation !== "string" || s.checkingCredential) {
      this.wake();
      return;
    }
    if (s.socket === null && s.now >= s.socketRetryAt) {
      const id = s.nextId;
      s.nextId += 1;
      s.socket = { id, phase: "ticket", epoch: null, subscribed: null };
      this.effects.push({ type: "ticket", socket: id, generation: s.generation });
    }
    const displayed = this.displayedKey();
    // `followed` is unique, in priority order: host, the displayed thread, the others.
    for (const key of s.followed) {
      if (Object.keys(s.inflight).length >= s.cfg.slots) break;
      const st = s.streams[key];
      if (!st) continue;
      if (this.forwardDue(st)) {
        const ms = this.st(key);
        if (!ms) continue;
        const req = this.issue({
          stream: key,
          dir: "forward",
          purpose: "forward",
          afterSeq: st.copy?.cursor ?? 0,
          beforeSeq: null,
          limit: st.fwdLimit,
        });
        if (!req) break;
        ms.fwd = req.id;
        // Issuing consumes the asked-for fetch (a pending resync included).
        ms.asked = false;
        ms.resync = false;
      }
      if (key === displayed && Object.keys(s.inflight).length < s.cfg.slots) {
        this.issueBackward(key);
      }
    }
    this.wake();
  }

  private issueBackward(key: string): void {
    const s = this.s;
    const st = s.streams[key];
    const w = s.window;
    if (!st || !w || !w.pending || w.error || st.bwd !== null || st.copy === null) return;
    // History waits for the resync a backward page of another epoch asked for.
    if (st.resync) return;
    if (s.epoch === null) return;
    const p: Pending = w.pending;
    let beforeSeq: number;
    let limit: number;
    if (p.purpose === "newer") {
      beforeSeq = p.last + p.n + 1;
      limit = p.n;
    } else {
      if (w.lo <= 1) {
        const mw = this.win();
        if (mw) mw.pending = null;
        return;
      }
      beforeSeq = w.lo;
      limit = w.limit;
    }
    const req = this.issue({
      stream: key,
      dir: "backward",
      purpose: p.purpose,
      afterSeq: null,
      beforeSeq,
      limit,
    });
    const ms = this.st(key);
    if (req && ms) ms.bwd = req.id;
  }

  /** The host stream is always followed; `subscribe` goes only when the set changed. */
  private ensureFollowed(): void {
    if (!this.s.streams[HOST]) this.refollow();
    const sock = this.s.socket;
    const keys = this.s.followed;
    if (sock?.phase === "open" && !sameKeys(sock.subscribed, keys)) {
      this.s.socket = { ...sock, subscribed: keys };
      this.effects.push({
        type: "send",
        socket: sock.id,
        msg: { type: "subscribe", stream_ids: [...keys] },
      });
    }
  }

  private wake(): void {
    const s = this.s;
    let at = Number.POSITIVE_INFINITY;
    if (s.socket === null && s.socketRetryAt > s.now) at = Math.min(at, s.socketRetryAt);
    for (const st of Object.values(s.streams)) {
      if (st.fwd === null && st.error === null && st.retryAt > s.now) at = Math.min(at, st.retryAt);
    }
    const next = Number.isFinite(at) ? at : null;
    // A null deadline stops the runner's timer.
    if (next !== s.wakeAt) this.effects.push({ type: "wake", at: next });
    s.wakeAt = next;
  }
}

// ---- projections ------------------------------------------------------------------------

function emptyCopy(key: string): Copy {
  if (key === HOST) return { kind: "host", cursor: 0, threads: {}, rawUnknown: 0 };
  // ThreadState::registered: the axes before the thread stream states any.
  return {
    kind: "thread",
    cursor: 0,
    process: "exited",
    turn: "unknown",
    task: null,
    attention: [],
    launch: null,
    inputs: [],
    done: [],
    rawUnknown: 0,
  };
}

/**
 * Applies one event of the copy's own stream; kinds that change no projection advance the
 * cursor only. Every accepted envelope of kind `raw_unknown` counts, typed or not, as the
 * daemon's `raw_unknown_count` does.
 */
function applyEvent(copy: Copy, e: EventEnvelope): Copy {
  const d = decode(e);
  const raw = e.kind === "raw_unknown" ? 1 : 0;
  if (copy.kind === "host") {
    let threads = copy.threads;
    if (d.outcome === "typed") {
      const ev = d.event;
      if (ev.kind === "thread_registered") {
        const p = ev.payload;
        const row: HostRow = {
          info: {
            thread_id: p.thread_id,
            agent: p.agent,
            cwd: p.cwd,
            workspace_id: p.workspace_id,
            parent_thread_id: p.parent_thread_id,
            role: p.role,
            closed: false,
            process_state: "exited",
            turn_state: "unknown",
            task_state: p.task !== null ? "active" : "none",
            attention: [],
          },
          fromSnapshot: false,
        };
        threads = { ...threads, [p.thread_id]: row };
      } else if (ev.kind === "thread_closed") {
        const cur = threads[ev.payload.thread_id];
        if (cur) threads = { ...threads, [ev.payload.thread_id]: { ...cur, info: { ...cur.info, closed: true } } };
      }
    }
    return { kind: "host", cursor: e.seq, threads, rawUnknown: copy.rawUnknown + raw } satisfies HostCopy;
  }
  const next: ThreadCopy = { ...copy, cursor: e.seq, rawUnknown: copy.rawUnknown + raw };
  if (d.outcome !== "typed") return next;
  const ev = d.event;
  switch (ev.kind) {
    case "status_changed": {
      const p = ev.payload;
      switch (p.field) {
        case "process_state":
          return { ...next, process: p.to };
        case "turn_state":
          return { ...next, turn: p.to };
        case "task_state":
          return { ...next, task: p.to };
        case "attention":
          return { ...next, attention: p.to };
      }
      return next;
    }
    case "launch_reserved":
      // The reserved launch becomes the thread's current one (store/project.rs).
      return {
        ...next,
        launch: { id: ev.payload.launch_id, exited: false, exitReason: null, deathConfirmed: false },
      };
    case "launch_exited": {
      const l = next.launch;
      if (l?.id !== ev.payload.launch_id) return next;
      const { reason, death_confirmed } = ev.payload;
      return { ...next, launch: { ...l, exited: true, exitReason: reason, deathConfirmed: death_confirmed } };
    }
    case "launch_death_confirmed": {
      const l = next.launch;
      if (l?.id !== ev.payload.launch_id) return next;
      return { ...next, launch: { ...l, deathConfirmed: true } };
    }
    case "input_created": {
      const p = ev.payload;
      const input: InputView = {
        id: p.input_id,
        state: p.initial_state,
        reason: null,
        by: null,
        createdBy: p.created_by,
        preview: contentPreview(p.text),
      };
      return { ...next, inputs: [...next.inputs.filter((i) => i.id !== p.input_id), input] };
    }
    case "input_state_changed": {
      const p = ev.payload;
      const cur = next.inputs.find((i) => i.id === p.input_id);
      if (!cur) return next;
      const moved: InputView = { ...cur, state: p.to, reason: p.reason, by: p.by };
      if (!TERMINAL_STATES.has(p.to)) {
        return { ...next, inputs: next.inputs.map((i) => (i.id === p.input_id ? moved : i)) };
      }
      return {
        ...next,
        inputs: next.inputs.filter((i) => i.id !== p.input_id),
        done: [...next.done, moved].slice(-TERMINAL_INPUTS),
      };
    }
    default:
      return next;
  }
}

function contentPreview(c: Content): string {
  return "inline" in c ? c.inline : c.preview;
}

function snapshotCopy(body: Extract<EventsResponse, { mode: "snapshot" }>): Copy {
  const snap = body.snapshot;
  if (snap.stream === "host") {
    const threads: Record<string, HostRow> = {};
    for (const info of snap.threads) threads[info.thread_id] = { info, fromSnapshot: true };
    return { kind: "host", cursor: body.as_of_seq, threads, rawUnknown: snap.raw_unknown_count };
  }
  const t = snap.thread;
  const l = snap.launch;
  return {
    kind: "thread",
    cursor: body.as_of_seq,
    process: t.process_state,
    turn: t.turn_state,
    task: t.task_state,
    attention: t.attention,
    launch:
      l === null
        ? null
        : { id: l.launch_id, exited: l.exit_reason !== null, exitReason: l.exit_reason, deathConfirmed: l.death_confirmed },
    // Inputs not yet terminal, without the reason or actor of their last transition.
    inputs: snap.inputs.map((i) => ({
      id: i.input_id,
      state: i.state,
      reason: null,
      by: null,
      createdBy: i.created_by,
      preview: contentPreview(i.text),
    })),
    done: [],
    rawUnknown: snap.raw_unknown_count,
  };
}

function sameKeys(a: readonly string[] | null, b: readonly string[]): boolean {
  return a !== null && a.length === b.length && a.every((k, i) => k === b[i]);
}

function contiguous(events: readonly EventEnvelope[]): boolean {
  for (let i = 1; i < events.length; i++) {
    if ((events[i]?.seq ?? 0) !== (events[i - 1]?.seq ?? 0) + 1) return false;
  }
  return true;
}

function setWindow(w: Mutable<Window>, lo: number, events: readonly EventEnvelope[], bytes: number) {
  w.lo = lo;
  w.events = events;
  w.hi = lo + events.length - 1;
  w.bytes = bytes;
}

function backwardError(r: FetchResult): StreamError {
  switch (r.kind) {
    case "transport":
    case "timeout":
      return { kind: "status", message: "the history read did not reach the daemon" };
    case "status":
      return { kind: "status", message: `the daemon refused the history read (${r.status})` };
    case "schema":
      return { kind: "schema", message: "the daemon's answer did not match its schema" };
    default:
      return { kind: "status", message: "the history read failed" };
  }
}
