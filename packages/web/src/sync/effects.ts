// What drives the reducer (actions) and what it asks the runner to do (effects). Every
// action carries the time it happened, so the reducer reads no clock.

import type { EventsResponse } from "@chaldea/contracts/generated/EventsResponse";
import type { WsClientMessage } from "@chaldea/contracts/generated/WsClientMessage";
import type { WsServerMessage } from "@chaldea/contracts/generated/WsServerMessage";

import type { Request } from "./state";

/** How one read ended, as the runner saw it. */
export type FetchResult =
  | { readonly kind: "ok"; readonly body: EventsResponse }
  /** A response with a non-2xx status; `retryAfter` in seconds when its `ErrorBody` gave one. */
  | { readonly kind: "status"; readonly status: number; readonly retryAfter: number | null }
  /** No response: the network failed (a `TypeError` from `fetch`). */
  | { readonly kind: "transport" }
  /** The 10 s timeout fired. */
  | { readonly kind: "timeout" }
  /** The body was over the read limit; it was not parsed. */
  | { readonly kind: "oversize" }
  /** A 2xx body that is not an `EventsResponse`. */
  | { readonly kind: "schema" };

export type TicketResult =
  | { readonly kind: "ok"; readonly ticket: string }
  | { readonly kind: "status"; readonly status: number; readonly retryAfter: number | null }
  | { readonly kind: "transport" };

type At = { readonly now: number };

export type Action = At &
  (
    | { readonly type: "tick" }
    /** The stored credential, reread: its generation, or null when none is stored. */
    | { readonly type: "credential"; readonly generation: string | null }
    /** The credential store could not be read. */
    | { readonly type: "storageFailed" }
    | { readonly type: "response"; readonly id: number; readonly result: FetchResult }
    | { readonly type: "ticket"; readonly socket: number; readonly result: TicketResult }
    | { readonly type: "socketOpen"; readonly socket: number }
    | { readonly type: "socketMessage"; readonly socket: number; readonly msg: WsServerMessage }
    | { readonly type: "socketClosed"; readonly socket: number }
    | { readonly type: "select"; readonly thread: string | null }
    /** The rows on screen once scrolling settled. */
    | { readonly type: "visible"; readonly threads: readonly string[] }
    | { readonly type: "older" }
    | { readonly type: "newer" }
    | { readonly type: "latest" }
    /** The user asked to retry what failed. */
    | { readonly type: "retry" }
  );

export type Effect =
  | { readonly type: "fetch"; readonly req: Request }
  /** A request was made older: its answer, if any, will be discarded. */
  | { readonly type: "abort"; readonly id: number }
  | { readonly type: "ticket"; readonly socket: number; readonly generation: string }
  | { readonly type: "connect"; readonly socket: number; readonly ticket: string }
  | { readonly type: "send"; readonly socket: number; readonly msg: WsClientMessage }
  | { readonly type: "close"; readonly socket: number }
  /** Dispatch a `tick` at this time; null stops the pending one. */
  | { readonly type: "wake"; readonly at: number | null }
  /** A 401 for this generation: compare-and-delete, then dispatch `credential`. */
  | { readonly type: "clearCredential"; readonly generation: string };
