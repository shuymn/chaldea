// The sync client's state (docs/design/w1-pwa.md §2.3, §2.4). Plain data only: the
// reducer (reducer.ts) is the one writer, the runner (runner.ts) holds the browser
// handles, and React reads it through `useSyncExternalStore`. No secret is kept here:
// the credential is named by its generation, the token stays in the runner.

import type { AttentionCause } from "@chaldea/contracts/generated/AttentionCause";
import type { EventEnvelope } from "@chaldea/contracts/generated/EventEnvelope";
import type { ProcessState } from "@chaldea/contracts/generated/ProcessState";
import type { TaskState } from "@chaldea/contracts/generated/TaskState";
import type { ThreadInfo } from "@chaldea/contracts/generated/ThreadInfo";
import type { TurnState } from "@chaldea/contracts/generated/TurnState";

export const HOST = "host";
export const threadStream = (threadId: string): string => `thread:${threadId}`;
export const threadOf = (stream: string): string | null =>
  stream.startsWith("thread:") ? stream.slice("thread:".length) : null;

export const MIB = 1024 * 1024;

export interface Config {
  /** `limit` of a forward page. */
  readonly forwardLimit: number;
  /** `limit` of a backward page. */
  readonly backwardLimit: number;
  /** Open threads followed besides the host stream and the selected thread. */
  readonly followMax: number;
  /** Reads in flight at once (the ticket POST is outside them). */
  readonly slots: number;
  /** The history window's caps. */
  readonly windowMaxEvents: number;
  readonly windowMaxBytes: number;
  /** The largest response body a read accepts, unparsed. */
  readonly readMaxBytes: number;
  /** Delays of a retried forward read, in ms; the last repeats. */
  readonly backoffMs: readonly number[];
  /** The socket's reconnect delay starts here and doubles up to `socketBackoffMaxMs`. */
  readonly socketBackoffMinMs: number;
  readonly socketBackoffMaxMs: number;
}

export const DEFAULT_CONFIG: Config = {
  forwardLimit: 200,
  backwardLimit: 100,
  followMax: 64,
  slots: 4,
  windowMaxEvents: 2000,
  windowMaxBytes: 16 * MIB,
  readMaxBytes: 16 * MIB,
  backoffMs: [1000, 2000, 4000, 8000, 16000, 30000],
  socketBackoffMinMs: 1000,
  socketBackoffMaxMs: 30000,
};

/** A row of the host copy: the thread's `ThreadInfo` and where its axes came from. */
export interface HostRow {
  readonly info: ThreadInfo;
  /**
   * True when the axes are a host snapshot's read ("last known"); false when the host
   * only has `thread_registered`'s defaults ("unavailable", the task state excepted).
   */
  readonly fromSnapshot: boolean;
}

export interface HostCopy {
  readonly kind: "host";
  readonly cursor: number;
  readonly threads: Readonly<Record<string, HostRow>>;
}

/** The thread stream's own projection: the axes and attention it owns (F1). */
export interface ThreadCopy {
  readonly kind: "thread";
  readonly cursor: number;
  readonly process: ProcessState;
  readonly turn: TurnState;
  /** Null until the thread stream itself states a task state; the host's is the initial one. */
  readonly task: TaskState | null;
  readonly attention: readonly AttentionCause[];
}

export type Copy = HostCopy | ThreadCopy;

export interface StreamError {
  readonly kind: "status" | "schema" | "oversize" | "missing_host";
  readonly message: string;
}

export interface Stream {
  readonly key: string;
  /** The valid copy, ending at its cursor; null before the first forward response. */
  readonly copy: Copy | null;
  /** A forward fetch was asked for (first fetch, socket open, epoch switch, resync, failure). */
  readonly asked: boolean;
  /** A backward page of another epoch asked for a forward resync not yet issued. */
  readonly resync: boolean;
  readonly hasMore: boolean;
  /** The head the last current forward response reported. */
  readonly knownHead: number | null;
  /** The highest head the socket announced (counted only in the socket's epoch). */
  readonly announced: number | null;
  /** The current request per direction (a request id), or none. */
  readonly fwd: number | null;
  readonly bwd: number | null;
  /** The current forward `limit`, halved after an oversized response. */
  readonly fwdLimit: number;
  /** A failure that is not retried automatically. */
  readonly error: StreamError | null;
  /** Transient failures in a row, and when the next forward read may go. */
  readonly attempt: number;
  readonly retryAt: number;
}

export type Purpose = "forward" | "tail" | "older" | "newer";

export interface Request {
  readonly id: number;
  readonly stream: string;
  readonly dir: "forward" | "backward";
  readonly purpose: Purpose;
  readonly afterSeq: number | null;
  readonly beforeSeq: number | null;
  readonly limit: number;
  readonly epoch: string | null;
  readonly generation: string;
}

/** A history read the displayed thread still wants. */
export type Pending =
  | { readonly purpose: "tail" }
  | { readonly purpose: "older" }
  | { readonly purpose: "newer"; readonly last: number; readonly n: number };

/**
 * The displayed thread's history: one contiguous run of events `lo..=hi` at or below the
 * cursor (`hi = lo - 1` when empty).
 */
export interface Window {
  readonly thread: string;
  readonly lo: number;
  readonly hi: number;
  readonly events: readonly EventEnvelope[];
  /** UTF-8 bytes of the events' JSON. */
  readonly bytes: number;
  /** Ends at the cursor and takes live events. */
  readonly attached: boolean;
  readonly pending: Pending | null;
  /** The backward `limit` (halved after an oversized page). */
  readonly limit: number;
  readonly error: StreamError | null;
}

export interface Socket {
  readonly id: number;
  readonly phase: "ticket" | "connecting" | "open";
  /** From `hello`. */
  readonly epoch: string | null;
  /** The set of the last `subscribe` sent on it. */
  readonly subscribed: readonly string[] | null;
}

export interface State {
  readonly cfg: Config;
  readonly now: number;
  /** The credential's generation; null when none is stored, undefined before the first read. */
  readonly generation: string | null | undefined;
  /** A 401's compare-and-delete is running: no request goes until it answers. */
  readonly checkingCredential: boolean;
  readonly epoch: string | null;
  readonly nextId: number;
  /** Requests issued and not yet answered or made older: the slots in use. */
  readonly inflight: Readonly<Record<number, Request>>;
  readonly streams: Readonly<Record<string, Stream>>;
  /** Threads a 404 removed in this epoch. */
  readonly gone: Readonly<Record<string, true>>;
  readonly selected: string | null;
  /** Thread ids whose rows are on screen, from the UI. */
  readonly visible: readonly string[];
  /** Followed stream keys, `host` first. */
  readonly followed: readonly string[];
  readonly window: Window | null;
  readonly socket: Socket | null;
  readonly socketAttempt: number;
  readonly socketRetryAt: number;
  /** The latest read or ticket POST failed in transport. */
  readonly transportDown: boolean;
  /** A `hello` or a current response was seen. */
  readonly reached: boolean;
  readonly lastSyncedAt: number | null;
  /** The next wake-up the runner was asked for. */
  readonly wakeAt: number | null;
  /** Events appended to the displayed timeline, for the status announcement. */
  readonly appended: number;
}

export function initialState(cfg: Config = DEFAULT_CONFIG, now = 0): State {
  return {
    cfg,
    now,
    generation: undefined,
    checkingCredential: false,
    epoch: null,
    nextId: 1,
    inflight: {},
    streams: {},
    gone: {},
    selected: null,
    visible: [],
    followed: [],
    window: null,
    socket: null,
    socketAttempt: 0,
    socketRetryAt: 0,
    transportDown: false,
    reached: false,
    lastSyncedAt: null,
    wakeAt: null,
    appended: 0,
  };
}

export function newStream(key: string, cfg: Config): Stream {
  return {
    key,
    copy: null,
    asked: true,
    resync: false,
    hasMore: false,
    knownHead: null,
    announced: null,
    fwd: null,
    bwd: null,
    fwdLimit: cfg.forwardLimit,
    error: null,
    attempt: 0,
    retryAt: 0,
  };
}

// ---- views ----------------------------------------------------------------------------

export type Status = "connecting" | "live" | "updates paused" | "offline";

/** The page's status (mvp §2.4 "Offline"; docs/design/w1-pwa.md §2.3 "Status"). */
export function status(s: State): Status {
  const live = s.socket?.phase === "open" && s.socket.epoch !== null;
  if (s.transportDown && !live) return "offline";
  if (!s.reached) return "connecting";
  if (live && !s.transportDown) return "live";
  return "updates paused";
}

/** List order: open threads first, then `cwd`, then `thread_id`. */
export function compareRows(a: ThreadInfo, b: ThreadInfo): number {
  if (a.closed !== b.closed) return a.closed ? 1 : -1;
  if (a.cwd !== b.cwd) return a.cwd < b.cwd ? -1 : 1;
  return a.thread_id < b.thread_id ? -1 : a.thread_id > b.thread_id ? 1 : 0;
}

/** The host copy's rows in list order, without the threads a 404 removed. */
export function listRows(s: State): HostRow[] {
  const host = s.streams[HOST]?.copy;
  if (host?.kind !== "host") return [];
  return Object.values(host.threads)
    .filter((r) => !s.gone[r.info.thread_id])
    .sort((a, b) => compareRows(a.info, b.info));
}

export type Label = "last known" | "unavailable" | null;

/** One row as rendered: host-owned fields from the host, the axes from the thread (F1). */
export interface RowView {
  readonly info: ThreadInfo;
  readonly process: ProcessState;
  readonly turn: TurnState;
  readonly task: TaskState;
  readonly attention: readonly AttentionCause[];
  /** Null when the axes come from a valid thread copy. */
  readonly label: Label;
}

export function rowView(s: State, row: HostRow): RowView {
  const copy = s.streams[threadStream(row.info.thread_id)]?.copy;
  if (copy?.kind === "thread") {
    return {
      info: row.info,
      process: copy.process,
      turn: copy.turn,
      task: copy.task ?? row.info.task_state,
      attention: copy.attention,
      label: null,
    };
  }
  return {
    info: row.info,
    process: row.info.process_state,
    turn: row.info.turn_state,
    task: row.info.task_state,
    attention: row.info.attention,
    label: row.fromSnapshot ? "last known" : "unavailable",
  };
}
