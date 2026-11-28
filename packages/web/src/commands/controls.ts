// When each control is available (docs/design/w2-composer.md §2.2 table and "Dispatch"),
// from the sync state alone. Every command needs `live`; send, ready, and the input
// commands also need a current copy of their thread; Stop and Resume use the latest valid
// copy and the launch the dialog pinned; Spawn needs only `live`.

import { current, HOST, type State, status, threadCopy, threadStream } from "../sync/state";
import { type CmdState, type Command, type Context, retryable } from "./reducer";

/** The session key of the context: socket, its `hello` epoch, the copies' epoch, the generation. */
export function context(s: State): Context {
  const sock = s.socket;
  const session =
    sock?.phase === "open" && sock.epoch !== null && typeof s.generation === "string"
      ? `${sock.id}|${sock.epoch}|${s.epoch ?? ""}|${s.generation}`
      : null;
  return { session, gone: s.gone };
}

export const sameContext = (a: Context, b: Context) => a.session === b.session && a.gone === b.gone;

const live = (s: State) => status(s) === "live";

/** The thread is listed and open (the host owns `closed`, F1). */
function open(s: State, thread: string): boolean {
  const host = s.streams[HOST]?.copy;
  if (host?.kind !== "host" || s.gone[thread]) return false;
  const row = host.threads[thread];
  return row !== undefined && !row.info.closed;
}

/** Why the thread's send, ready, and input commands are locked now, or null. */
export function lockReason(s: State, thread: string): string | null {
  if (!live(s)) return "Commands wait until the page is live.";
  if (!current(s, threadStream(thread))) return "Commands wait until this thread is caught up.";
  return null;
}

export function canSend(s: State, thread: string): boolean {
  const c = threadCopy(s, thread);
  return (
    lockReason(s, thread) === null &&
    open(s, thread) &&
    c !== null &&
    (c.process === "starting" || c.process === "running")
  );
}

/** The launch Mark ready would pin, or null when it is unavailable. */
export function readyLaunch(s: State, thread: string): string | null {
  const c = threadCopy(s, thread);
  if (lockReason(s, thread) !== null || !open(s, thread) || c === null || c.process !== "running") return null;
  return c.launch !== null && !c.launch.exited ? c.launch.id : null;
}

/** The launch Stop would pin, or null. */
export function stopLaunch(s: State, thread: string): string | null {
  const c = threadCopy(s, thread);
  if (!live(s) || !open(s, thread) || c?.launch == null || c.launch.exited) return null;
  return c.launch.id;
}

/** The launch Resume would name, or null. */
export function resumeLaunch(s: State, thread: string): string | null {
  const c = threadCopy(s, thread);
  if (!live(s) || !open(s, thread) || c?.launch == null || !c.launch.exited || !c.launch.deathConfirmed) return null;
  return c.launch.id;
}

/** The exited launch whose death is not confirmed: the page shows the hint instead of Resume. */
export function hintLaunch(s: State, thread: string): string | null {
  const c = threadCopy(s, thread);
  if (!open(s, thread) || c?.launch == null || !c.launch.exited || c.launch.deathConfirmed) return null;
  return c.launch.id;
}

function inputState(s: State, thread: string, input: string): string | null {
  return threadCopy(s, thread)?.inputs.find((i) => i.id === input)?.state ?? null;
}

export function canResolve(s: State, thread: string, input: string): boolean {
  return lockReason(s, thread) === null && inputState(s, thread, input) === "unconfirmed";
}

export function canCancel(s: State, thread: string, input: string): boolean {
  return lockReason(s, thread) === null && inputState(s, thread, input) === "queued";
}

export const canSpawn = live;

/**
 * The gate of a command's target, read from its frozen body: the launch it named must be
 * the one its control would pin now (Ready, Stop, Resume), and the input must still be in
 * the state its command acts on. A Retry is shown and dispatched only through this gate.
 */
export function targetGate(c: Pick<Command, "name" | "thread" | "body">): (s: State) => boolean {
  const t = c.thread ?? "";
  let args: Record<string, unknown> = {};
  try {
    args = (JSON.parse(c.body) as { args?: Record<string, unknown> }).args ?? {};
  } catch {
    return () => false;
  }
  const launch = typeof args.expected_launch_id === "string" ? args.expected_launch_id : null;
  const input = typeof args.input_id === "string" ? args.input_id : null;
  switch (c.name) {
    case "send":
      return (s) => canSend(s, t);
    case "ready":
      return (s) => launch !== null && readyLaunch(s, t) === launch;
    case "stop":
      return (s) => launch !== null && stopLaunch(s, t) === launch;
    case "resume":
      return (s) => launch !== null && resumeLaunch(s, t) === launch;
    case "input_resolve":
      return (s) => input !== null && canResolve(s, t, input);
    case "input_cancel":
      return (s) => input !== null && canCancel(s, t, input);
    case "spawn":
      return canSpawn;
  }
}

/** Whether the slot's Retry is offered now: not done, in session, after `retry_after`, and its target unchanged. */
export function canRetry(cmds: CmdState, s: State, slot: string): boolean {
  const c = cmds.slots[slot];
  return c !== undefined && retryable(cmds, slot) && targetGate(c)(s);
}

/** The slot of each command. Ready and Stop/Resume are separate, so an unknown Ready never blocks Stop. */
export const slots = {
  spawn: "spawn",
  send: (thread: string) => `send:${thread}`,
  ready: (thread: string) => `ready:${thread}`,
  launch: (thread: string) => `launch:${thread}`,
  input: (input: string) => `input:${input}`,
} as const;
