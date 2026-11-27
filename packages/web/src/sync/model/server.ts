// An independent model of the daemon's sync side, for the client's tests (test only).
// It imports nothing from sync/: its replay decision is written from mvp §2.4
// "Replay" (not shared with the reducer), its projections from the store's rules (a
// thread's axes on its stream, membership and `closed` on the host stream), and its
// sockets from http §2.6 (heads known per socket, frames built at send selection).

import type { AttentionCause } from "@chaldea/contracts/generated/AttentionCause";
import type { EventEnvelope } from "@chaldea/contracts/generated/EventEnvelope";
import type { EventsResponse } from "@chaldea/contracts/generated/EventsResponse";
import type { ProcessState } from "@chaldea/contracts/generated/ProcessState";
import type { StreamHeadEntry } from "@chaldea/contracts/generated/StreamHeadEntry";
import type { TaskState } from "@chaldea/contracts/generated/TaskState";
import type { ThreadInfo } from "@chaldea/contracts/generated/ThreadInfo";
import type { TurnState } from "@chaldea/contracts/generated/TurnState";
import type { WsServerMessage } from "@chaldea/contracts/generated/WsServerMessage";

export interface ModelConfig {
  /** Scaled `REPLAY_MAX_EVENTS` and `REPLAY_MAX_BYTES` (payload bytes). */
  readonly replayMaxEvents: number;
  readonly replayMaxBytes: number;
  /** `SNAPSHOT_RECENT_EVENTS`. */
  readonly recent: number;
}

export interface Axes {
  process: ProcessState;
  turn: TurnState;
  task: TaskState;
  attention: AttentionCause[];
}

interface ThreadRec {
  readonly id: string;
  readonly agent: "claude" | "codex";
  readonly cwd: string;
  closed: boolean;
  axes: Axes;
}

interface Db {
  readonly epoch: string;
  readonly logs: Map<string, EventEnvelope[]>;
  readonly threads: Map<string, ThreadRec>;
  /** The paired token is in this database. */
  readonly tokens: boolean;
}

export type Query = {
  readonly stream: string;
  readonly afterSeq: number | null;
  readonly beforeSeq: number | null;
  readonly limit: number;
  readonly epoch: string | null;
};

export type Answer =
  | { readonly status: 200; readonly body: EventsResponse }
  | { readonly status: 401 | 404 };

const TS = "2026-11-22T09:45:00.000Z";
const hex = (n: number, w: number) => n.toString(16).padStart(w, "0");
export const threadId = (n: number) => `0193abcd-ef01-7000-8000-${hex(n, 12)}`;

/** Bytes of a payload as the store counts `cum_bytes` (its JSON in UTF-8). */
const payloadBytes = (p: unknown) => new TextEncoder().encode(JSON.stringify(p)).length;

export class ModelServer {
  db: Db;
  private epochN = 1;
  private eventN = 1;
  private readonly ever = new Set<string>();
  private readonly cfg: ModelConfig;
  /** Commit listeners: the sockets' `merge` of committed heads. */
  readonly onCommit = new Set<(stream: string, head: number) => void>();

  constructor(cfg: ModelConfig) {
    this.cfg = cfg;
    this.db = this.freshDb(true);
  }

  private freshDb(tokens: boolean): Db {
    const db: Db = {
      epoch: `epoch-${this.epochN}`,
      logs: new Map([["host", []]]),
      threads: new Map(),
      tokens,
    };
    this.epochN += 1;
    return db;
  }

  // ---- writes ---------------------------------------------------------------------------

  private append(stream: string, kind: string, payload: unknown, thread: string | null): void {
    const log = this.db.logs.get(stream);
    if (!log) throw new Error(`no stream ${stream}`);
    const seq = log.length + 1;
    const n = this.eventN++;
    log.push({
      stream_id: stream,
      seq,
      event_id: `01930000-0000-7000-8000-${hex(n, 12)}`,
      schema_version: 1,
      idempotency_key: `model:${n}`,
      kind,
      thread_id: thread,
      launch_id: null,
      command_id: null,
      source: { kind: "daemon", agent: null, cli_version: null, transcript: null },
      occurred_at: TS,
      recorded_at: TS,
      payload,
    });
    for (const f of this.onCommit) f(stream, seq);
  }

  /**
   * Registers thread `n`. A thread id is new once (ids are UUIDs): only a database swap
   * that keeps the thread registers it again, before any client can subscribe in it.
   */
  register(n: number, task: boolean, again = false): void {
    const id = threadId(n);
    if (this.db.threads.has(id) || (this.ever.has(id) && !again)) return;
    this.ever.add(id);
    const rec: ThreadRec = {
      id,
      agent: n % 2 === 0 ? "claude" : "codex",
      cwd: `/work/p${n % 3}`,
      closed: false,
      axes: { process: "exited", turn: "unknown", task: task ? "active" : "none", attention: [] },
    };
    this.db.threads.set(id, rec);
    this.db.logs.set(`thread:${id}`, []);
    this.append(
      "host",
      "thread_registered",
      {
        thread_id: id,
        agent: rec.agent,
        cwd: rec.cwd,
        workspace_id: "ws-model",
        parent_thread_id: null,
        role: "standard",
        task: task ? "a task" : null,
      },
      id,
    );
  }

  close(id: string): void {
    const t = this.db.threads.get(id);
    if (!t || t.closed) return;
    t.closed = true;
    this.append("host", "thread_closed", { thread_id: id, by: "human" }, id);
  }

  status<F extends keyof Axes>(id: string, field: F, to: Axes[F]): void {
    const t = this.db.threads.get(id);
    if (!t) return;
    const from = t.axes[field];
    t.axes[field] = to;
    const wire = { process: "process_state", turn: "turn_state", task: "task_state", attention: "attention" }[field];
    this.append(`thread:${id}`, "status_changed", { field: wire, from, to, cause: "spawned" }, id);
  }

  /** An event with a text payload of `size` code units (`text` repeated). */
  message(id: string, size: number, text = "x"): void {
    if (!this.db.threads.has(id)) return;
    const points = Array.from(text);
    const inline = Array.from({ length: Math.max(1, size) }, (_, k) => points[k % points.length]).join("");
    this.append(`thread:${id}`, "assistant_message", { text: { inline, encoding: "utf8" }, turn_ref: null }, id);
  }

  /** An event the reader cannot decode (an unknown kind), on a thread stream. */
  unknown(id: string): void {
    if (!this.db.threads.has(id)) return;
    this.append(`thread:${id}`, "future_kind", { anything: true }, id);
  }

  /**
   * A new database: the next epoch with empty streams. Threads in `keep` are registered
   * again in it; `tokens` false is a database without the paired token.
   */
  swap(keep: readonly number[], tokens = true): void {
    this.db = this.freshDb(tokens);
    for (const n of keep) if (this.ever.has(threadId(n))) this.register(n, false, true);
  }

  // ---- reads ----------------------------------------------------------------------------

  head(stream: string): number {
    return this.db.logs.get(stream)?.length ?? 0;
  }

  exists(stream: string): boolean {
    return this.db.logs.has(stream);
  }

  log(stream: string): readonly EventEnvelope[] {
    return this.db.logs.get(stream) ?? [];
  }

  thread(id: string): ThreadRec | undefined {
    return this.db.threads.get(id);
  }

  threadIds(): string[] {
    return [...this.db.threads.keys()];
  }

  private info(t: ThreadRec): ThreadInfo {
    return {
      thread_id: t.id,
      agent: t.agent,
      cwd: t.cwd,
      workspace_id: "ws-model",
      parent_thread_id: null,
      role: "standard",
      closed: t.closed,
      process_state: t.axes.process,
      turn_state: t.axes.turn,
      task_state: t.axes.task,
      attention: [...t.axes.attention],
    };
  }

  /** mvp §2.4 "Replay", from the query, the head, and the epoch. */
  private decideReplay(
    q: Query,
    head: number,
    bytesAfter: number,
  ): { plan: "forward" } | { plan: "backward" } | { plan: "snapshot"; reason: "epoch_changed" | "cursor_ahead" | "too_many_events" | "too_many_bytes" } {
    if (q.beforeSeq !== null) return { plan: "backward" };
    const after = q.afterSeq ?? 0;
    if (q.epoch !== null && q.epoch !== this.db.epoch) return { plan: "snapshot", reason: "epoch_changed" };
    if (after > head) return { plan: "snapshot", reason: "cursor_ahead" };
    if (head - after > this.cfg.replayMaxEvents) return { plan: "snapshot", reason: "too_many_events" };
    if (bytesAfter > this.cfg.replayMaxBytes) return { plan: "snapshot", reason: "too_many_bytes" };
    return { plan: "forward" };
  }

  /** One read, as `GET /api/events` answers it. */
  answer(q: Query): Answer {
    if (!this.db.tokens) return { status: 401 };
    const log = this.db.logs.get(q.stream);
    if (!log) return { status: 404 };
    const head = log.length;
    const after = q.afterSeq ?? 0;
    const bytesAfter = log.slice(after).reduce((n, e) => n + payloadBytes(e.payload), 0);
    const d = this.decideReplay(q, head, bytesAfter);
    const epoch = this.db.epoch;
    if (d.plan === "forward") {
      const events = log.slice(after, after + q.limit);
      return {
        status: 200,
        body: {
          mode: "replay",
          stream_id: q.stream,
          epoch,
          events,
          head_seq: head,
          has_more: after + q.limit < head,
          has_more_before: after >= 1 && head >= 1,
        },
      };
    }
    if (d.plan === "backward") {
      const before = q.beforeSeq ?? 1;
      const hi = Math.min(before - 1, head);
      const lo = Math.max(1, hi - q.limit + 1);
      const events = hi >= 1 ? log.slice(lo - 1, hi) : [];
      return {
        status: 200,
        body: {
          mode: "replay",
          stream_id: q.stream,
          epoch,
          events,
          head_seq: head,
          has_more: hi < head,
          has_more_before: hi >= 1 && lo > 1,
        },
      };
    }
    if (q.stream === "host") {
      return {
        status: 200,
        body: {
          mode: "snapshot",
          stream_id: q.stream,
          epoch,
          snapshot: {
            stream: "host",
            threads: [...this.db.threads.values()].map((t) => this.info(t)),
            worktrees: [],
            surfaces: [],
          },
          as_of_seq: head,
          reason: d.reason,
        },
      };
    }
    const t = this.db.threads.get(q.stream.slice("thread:".length));
    if (!t) return { status: 404 };
    return {
      status: 200,
      body: {
        mode: "snapshot",
        stream_id: q.stream,
        epoch,
        snapshot: {
          stream: "thread",
          thread: this.info(t),
          launch: null,
          session: null,
          gate: {
            readiness_seq: null,
            readiness_evidence: null,
            last_write_seq: null,
            last_human_write_seq: null,
            last_turn_start_seq: null,
          },
          inputs: [],
          open_approvals: [],
          worktree: null,
          surfaces: [],
          vt_snapshot: null,
          recent_events: log.slice(Math.max(0, head - this.cfg.recent)),
        },
        as_of_seq: head,
        reason: d.reason,
      },
    };
  }

  heads(streams: Iterable<string>): StreamHeadEntry[] {
    const out: StreamHeadEntry[] = [];
    for (const s of streams) {
      const log = this.db.logs.get(s);
      if (log) out.push({ stream_id: s, head_seq: log.length });
    }
    return out;
  }
}

/**
 * One `/api/ws` socket's server side (http §2.6): the subscription set, the heads it
 * knows and announced, and frames built at send selection, never queued.
 */
export class ModelSocket {
  private s: string[] = [];
  private readonly known = new Map<string, number>();
  private readonly announced = new Map<string, number>();
  private headsDue = false;
  private helloSent = false;
  readonly inbox: string[][] = [];
  private readonly server: ModelServer;
  private readonly listener: (stream: string, head: number) => void;

  constructor(server: ModelServer) {
    this.server = server;
    this.listener = (stream, head) => this.merge([{ stream_id: stream, head_seq: head }], false);
    server.onCommit.add(this.listener);
  }

  close(): void {
    this.server.onCommit.delete(this.listener);
  }

  private merge(entries: StreamHeadEntry[], snapshot: boolean): void {
    for (const e of entries) {
      if (!this.s.includes(e.stream_id)) continue;
      this.known.set(e.stream_id, Math.max(this.known.get(e.stream_id) ?? 0, e.head_seq));
    }
    if (snapshot) this.headsDue = true;
  }

  /** Takes one `subscribe` in: the set is replaced by the existing streams of it. */
  receive(): boolean {
    const ids = this.inbox.shift();
    if (!ids) return false;
    this.s = ids.filter((id) => this.server.exists(id));
    for (const k of [...this.known.keys()]) if (!this.s.includes(k)) this.known.delete(k);
    for (const k of [...this.announced.keys()]) if (!this.s.includes(k)) this.announced.delete(k);
    this.merge(this.server.heads(this.s), true);
    return true;
  }

  /** The periodic refresh: a heads read, then a `heads` frame is due. */
  refresh(): void {
    this.merge(this.server.heads(this.s), true);
  }

  /** Builds the next frame at selection, or none. */
  select(): WsServerMessage | null {
    if (!this.helloSent) {
      this.helloSent = true;
      return { type: "hello", epoch: this.server.db.epoch, server_time: "2026-11-22T09:45:00Z" };
    }
    if (this.headsDue) {
      this.headsDue = false;
      const heads = this.s.map((id) => ({ stream_id: id, head_seq: this.known.get(id) ?? 0 }));
      for (const h of heads) this.announced.set(h.stream_id, h.head_seq);
      return { type: "heads", heads };
    }
    for (const id of this.s) {
      const k = this.known.get(id) ?? 0;
      if (k > (this.announced.get(id) ?? 0)) {
        this.announced.set(id, k);
        return { type: "stream_advanced", stream_id: id, head_seq: k };
      }
    }
    return null;
  }
}
