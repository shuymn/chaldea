// Every request the page makes (docs/design/w1-pwa.md §2.2): fixed relative paths,
// Fetch mode `cors` (so a POST under `no-referrer` still sends the real `Origin`),
// `redirect: "error"`, no cookies, no referrer, and the token only in `Authorization`.
// Errors carry no URL, body, or header.

import { validators } from "@chaldea/contracts";

import type { FetchResult, TicketResult } from "./sync/effects";
import type { Request } from "./sync/state";

export const REQUEST_POLICY = {
  mode: "cors",
  credentials: "omit",
  redirect: "error",
  referrerPolicy: "no-referrer",
} as const satisfies RequestInit;

export const READ_TIMEOUT_MS = 10_000;

export type Fetch = (input: string, init: RequestInit) => Promise<Response>;

export function eventsPath(req: Request): string {
  const q = new URLSearchParams({ stream_id: req.stream });
  if (req.dir === "forward") q.set("after_seq", String(req.afterSeq ?? 0));
  else q.set("before_seq", String(req.beforeSeq ?? 1));
  q.set("limit", String(req.limit));
  if (req.epoch !== null) q.set("epoch", req.epoch);
  return `/api/events?${q.toString()}`;
}

function headers(token: string | null, json: boolean): Headers {
  const h = new Headers();
  if (token !== null) h.set("Authorization", `Bearer ${token}`);
  if (json) h.set("Content-Type", "application/json");
  return h;
}

/** Reads the body as text, refusing more than `max` bytes before parsing. */
export async function readLimited(res: Response, max: number): Promise<string | null> {
  const declared = res.headers.get("Content-Length");
  if (declared !== null && Number(declared) > max) {
    await res.body?.cancel();
    return null;
  }
  if (!res.body) return "";
  const reader = res.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > max) {
      await reader.cancel();
      return null;
    }
    chunks.push(value);
  }
  const all = new Uint8Array(total);
  let at = 0;
  for (const c of chunks) {
    all.set(c, at);
    at += c.byteLength;
  }
  return new TextDecoder().decode(all);
}

async function retryAfter(res: Response): Promise<number | null> {
  const text = await readLimited(res, 64 * 1024).catch(() => null);
  if (text === null) return null;
  try {
    const body: unknown = JSON.parse(text);
    return validators.ErrorBody(body) ? (body.retry_after ?? null) : null;
  } catch {
    return null;
  }
}

function isTimeout(e: unknown): boolean {
  return e instanceof DOMException && e.name === "TimeoutError";
}

/** One `GET /api/events` read. */
export async function readEvents(
  fetchFn: Fetch,
  req: Request,
  token: string,
  signal: AbortSignal,
  maxBytes: number,
): Promise<FetchResult> {
  const timeout = AbortSignal.timeout(READ_TIMEOUT_MS);
  const any = AbortSignal.any([signal, timeout]);
  try {
    const res = await fetchFn(eventsPath(req), {
      ...REQUEST_POLICY,
      method: "GET",
      headers: headers(token, false),
      signal: any,
    });
    if (!res.ok) {
      return { kind: "status", status: res.status, retryAfter: await retryAfter(res) };
    }
    const text = await readLimited(res, maxBytes);
    if (text === null) return { kind: "oversize" };
    let body: unknown;
    try {
      body = JSON.parse(text);
    } catch {
      return { kind: "schema" };
    }
    return validators.EventsResponse(body) ? { kind: "ok", body } : { kind: "schema" };
  } catch (e) {
    if (timeout.aborted || isTimeout(e)) return { kind: "timeout" };
    return { kind: "transport" };
  }
}

/** `POST /api/ws-ticket` for the events socket. */
export async function postTicket(fetchFn: Fetch, token: string): Promise<TicketResult> {
  const timeout = AbortSignal.timeout(READ_TIMEOUT_MS);
  try {
    const res = await fetchFn("/api/ws-ticket", {
      ...REQUEST_POLICY,
      method: "POST",
      headers: headers(token, true),
      body: JSON.stringify({ purpose: "events" }),
      signal: timeout,
    });
    if (!res.ok) {
      return { kind: "status", status: res.status, retryAfter: await retryAfter(res) };
    }
    const text = await readLimited(res, 64 * 1024);
    const body: unknown = text === null ? null : JSON.parse(text);
    return validators.WsTicketResponse(body)
      ? { kind: "ok", ticket: body.ticket }
      : { kind: "status", status: res.status, retryAfter: null };
  } catch {
    return { kind: "transport" };
  }
}

export type PairResult =
  | { readonly kind: "ok"; readonly token: string }
  | { readonly kind: "refused"; readonly status: number }
  | { readonly kind: "transport" };

/** `POST /api/pair` with the code from the link. */
export async function postPair(fetchFn: Fetch, code: string): Promise<PairResult> {
  const timeout = AbortSignal.timeout(READ_TIMEOUT_MS);
  try {
    const res = await fetchFn("/api/pair", {
      ...REQUEST_POLICY,
      method: "POST",
      headers: headers(null, true),
      body: JSON.stringify({ code }),
      signal: timeout,
    });
    if (!res.ok) {
      await res.body?.cancel();
      return { kind: "refused", status: res.status };
    }
    const text = await readLimited(res, 64 * 1024);
    const body: unknown = text === null ? null : JSON.parse(text);
    return validators.PairResponse(body)
      ? { kind: "ok", token: body.token }
      : { kind: "refused", status: res.status };
  } catch {
    return { kind: "transport" };
  }
}
