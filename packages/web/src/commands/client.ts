// One command attempt (docs/design/w2-composer.md §2.2): `POST /api/commands/<name>` with
// the frozen body bytes, W1's request policy (http.ts), a 64 KiB reply cap, and the reply
// classified by §2.2's table. Diagnostics carry no URL, body, or header.

import { validators } from "@chaldea/contracts";
import type { ErrorCode } from "@chaldea/contracts/generated/ErrorCode";
import type { InputCommandResult } from "@chaldea/contracts/generated/InputCommandResult";
import type { ReadyResult } from "@chaldea/contracts/generated/ReadyResult";
import type { ResumeResult } from "@chaldea/contracts/generated/ResumeResult";
import type { SendResult } from "@chaldea/contracts/generated/SendResult";
import type { SpawnResult } from "@chaldea/contracts/generated/SpawnResult";
import type { StopResult } from "@chaldea/contracts/generated/StopResult";

import { type Fetch, headers, READ_TIMEOUT_MS, readLimited, REQUEST_POLICY } from "../http";

export type CommandName = "send" | "ready" | "input_resolve" | "input_cancel" | "spawn" | "stop" | "resume";

/** The page sends these commands and no other (W2-12). */
export const COMMANDS: readonly CommandName[] = [
  "send",
  "ready",
  "input_resolve",
  "input_cancel",
  "spawn",
  "stop",
  "resume",
];

/** 10 s, or 40 s for the launch commands (supervisor.md:102-104). */
export function timeoutMs(name: CommandName): number {
  return name === "spawn" || name === "resume" || name === "stop" ? 40_000 : READ_TIMEOUT_MS;
}

/** The largest reply body read, success or error. */
export const REPLY_MAX_BYTES = 64 * 1024;

export interface Results {
  send: SendResult;
  ready: ReadyResult;
  input_resolve: InputCommandResult;
  input_cancel: InputCommandResult;
  spawn: SpawnResult;
  stop: StopResult;
  resume: ResumeResult;
}

export type Result = Results[CommandName];

const RESULT: { readonly [K in CommandName]: (v: unknown) => v is Results[K] } = {
  send: validators.SendResult,
  ready: validators.ReadyResult,
  input_resolve: validators.InputCommandResult,
  input_cancel: validators.InputCommandResult,
  spawn: validators.SpawnResult,
  stop: validators.StopResult,
  resume: validators.ResumeResult,
};

/**
 * How one attempt ended (§2.2's reply table):
 * - `ok`, `unreadable`, `settled`: the command is done (a 2xx, or a definitive refusal);
 * - `unknown`: it may or may not have taken effect, so it is never retried;
 * - `not_done`: it did not take effect, and the same bytes may be sent again;
 * - `unauthorized`: a 401, not done, with no Retry (W1's compare-and-delete runs instead);
 * - `not_sent`: the page had no token for the attempt's generation, so nothing left it.
 */
export type Reply =
  | { readonly kind: "ok"; readonly status: number; readonly result: Result }
  | { readonly kind: "unreadable"; readonly status: number }
  | { readonly kind: "settled"; readonly status: number; readonly code: ErrorCode | null }
  | { readonly kind: "unknown"; readonly status: number | null; readonly code: ErrorCode | null }
  | { readonly kind: "not_done"; readonly status: number; readonly code: ErrorCode | null; readonly retryAfter: number }
  | { readonly kind: "unauthorized" }
  | { readonly kind: "not_sent" };

const NOT_DONE_503: ReadonlySet<ErrorCode> = new Set(["busy", "shutting_down", "disk_full", "store_failed"]);

/** Classifies a reply by its status and its body (null when over the cap or unreadable). */
export function classify(name: CommandName, status: number, text: string | null): Reply {
  if (status >= 200 && status < 300) {
    let body: unknown;
    try {
      body = text === null ? undefined : JSON.parse(text);
    } catch {
      return { kind: "unreadable", status };
    }
    const valid = RESULT[name] as (v: unknown) => v is Result;
    return valid(body) ? { kind: "ok", status, result: body } : { kind: "unreadable", status };
  }
  let err: { code: ErrorCode; retry_after?: number } | null = null;
  try {
    const body: unknown = text === null ? undefined : JSON.parse(text);
    if (validators.ErrorBody(body)) err = body;
  } catch {
    err = null;
  }
  const code = err?.code ?? null;
  if (status === 401) return { kind: "unauthorized" };
  if (status === 408) return { kind: "not_done", status, code, retryAfter: err?.retry_after ?? 0 };
  if (status === 503) {
    if (code !== null && NOT_DONE_503.has(code)) {
      return { kind: "not_done", status, code, retryAfter: err?.retry_after ?? 0 };
    }
    // `outcome_unknown`, and any 503 whose body did not parse.
    return { kind: "unknown", status, code };
  }
  // `in_progress`, and a 409 whose body did not decode: the receipt may exist.
  if (status === 409 && (code === "in_progress" || code === null)) return { kind: "unknown", status, code };
  if (status === 500) return { kind: "unknown", status, code };
  // Guard and parser 4xx, 501, 409 `conflict` and `unknown_after_crash`, and every other
  // handler refusal (e.g. 504 `launch_busy`): definitive.
  if (status < 500 || status === 501 || code !== null) return { kind: "settled", status, code };
  // A 5xx the daemon does not send: not proven done or not done.
  return { kind: "unknown", status, code };
}

/**
 * Sends one attempt: `body` is the frozen bytes, sent as they are. `signal` ends it (the
 * page unloading); the deadline is the command's timeout. A transport failure, the
 * deadline, or an abort is `unknown`: the request may have reached the daemon.
 */
export async function postCommand(
  fetchFn: Fetch,
  name: CommandName,
  body: string,
  token: string,
  signal: AbortSignal,
): Promise<Reply> {
  const any = AbortSignal.any([signal, AbortSignal.timeout(timeoutMs(name))]);
  let res: Response;
  try {
    res = await fetchFn(`/api/commands/${name}`, {
      ...REQUEST_POLICY,
      method: "POST",
      headers: headers(token, true),
      body,
      signal: any,
    });
  } catch {
    return { kind: "unknown", status: null, code: null };
  }
  // A body that fails or stalls after the headers proves nothing, whatever the status: the
  // command may still take effect. (A body over the cap is read as no body.)
  let text: string | null;
  try {
    text = await readLimited(res, REPLY_MAX_BYTES);
  } catch {
    return { kind: "unknown", status: res.status, code: null };
  }
  return classify(name, res.status, text);
}
