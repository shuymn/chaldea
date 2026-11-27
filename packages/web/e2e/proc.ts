// Child processes of the end-to-end harness with a bounded lifecycle: `ended` settles once on
// the exit or on a spawn failure (which emits `error` and no `exit`), every wait has a
// deadline, a process past its deadline is killed with SIGKILL, and the kill is itself
// awaited within a bound. Nothing here prints secrets: callers pass messages through
// `redact`.

import { type ChildProcess, spawn, type SpawnOptions } from "node:child_process";

export interface End {
  readonly code: number | null;
  readonly signal: NodeJS.Signals | null;
  readonly error: Error | null;
}

export interface Proc {
  readonly child: ChildProcess;
  readonly ended: Promise<End>;
  running(): boolean;
}

/** How long a SIGKILLed process may take to be reaped. */
export const REAP_MS = 10_000;

export function startProc(cmd: string, args: readonly string[], opts: SpawnOptions): Proc {
  const child = spawn(cmd, args, opts);
  let done = false;
  const ended = new Promise<End>((ok) => {
    child.once("exit", (code, signal) => ok({ code, signal, error: null }));
    child.once("error", (error) => ok({ code: null, signal: null, error }));
  });
  void ended.then(() => {
    done = true;
  });
  return { child, ended, running: () => !done };
}

/** `p`'s end within `ms`, else null. */
export async function within(p: Proc, ms: number): Promise<End | null> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const late = new Promise<null>((ok) => {
    timer = setTimeout(() => ok(null), ms);
  });
  try {
    return await Promise.race([p.ended, late]);
  } finally {
    clearTimeout(timer);
  }
}

/** SIGKILLs `p` if it still runs and waits for it within `ms`; throws past that. */
export async function reap(p: Proc, ms = REAP_MS): Promise<void> {
  if (!p.running()) return;
  p.child.kill("SIGKILL");
  if ((await within(p, ms)) === null) {
    throw new Error(`process ${p.child.pid} was not reaped within ${ms} ms of SIGKILL`);
  }
}

/** Drops anything shaped like a token, a pair code, or a pairing fragment. */
export function redact(s: string): string {
  return s.replace(/\bchd[a-z]*_[A-Za-z0-9_-]+/g, "<redacted>").replace(/#pair=[^\s"'<>]*/g, "#pair=<redacted>");
}

export interface RunResult extends End {
  readonly stdout: string;
  readonly stderr: string;
}

/**
 * Runs `cmd` to its end within `deadlineMs`; past it, or when `signal` aborts, the process
 * is killed and reaped and the call throws. A spawn failure throws too.
 */
export async function runProc(
  cmd: string,
  args: readonly string[],
  opts: SpawnOptions & { deadlineMs: number; reapMs?: number; signal?: AbortSignal; capture?: boolean },
): Promise<RunResult> {
  const { deadlineMs, reapMs, signal, capture = true, ...spawnOpts } = opts;
  signal?.throwIfAborted();
  const p = startProc(cmd, args, capture ? { ...spawnOpts, stdio: ["ignore", "pipe", "pipe"] } : spawnOpts);
  let stdout = "";
  let stderr = "";
  p.child.stdout?.setEncoding("utf8");
  p.child.stderr?.setEncoding("utf8");
  p.child.stdout?.on("data", (d: string) => {
    stdout += d;
  });
  p.child.stderr?.on("data", (d: string) => {
    stderr += d;
  });
  const aborted = new Promise<"aborted">((ok) => signal?.addEventListener("abort", () => ok("aborted"), { once: true }));
  try {
    const end = await Promise.race([within(p, deadlineMs), aborted]);
    if (end === "aborted") throw new Error(`${cmd} was cancelled`);
    if (end === null) throw new Error(`${cmd} did not end within ${deadlineMs} ms`);
    if (end.error) throw new Error(`${cmd} could not start: ${end.error.message}`);
    // The pipe may still hold the last bytes after the exit; a descendant holding it open
    // gets one second.
    const out = p.child.stdout;
    if (out && !out.readableEnded) {
      let timer: ReturnType<typeof setTimeout> | undefined;
      await Promise.race([
        new Promise<void>((ok) => out.once("end", ok)),
        new Promise<void>((ok) => {
          timer = setTimeout(ok, 1_000);
        }),
      ]);
      clearTimeout(timer);
    }
    return { ...end, stdout, stderr };
  } finally {
    await reap(p, reapMs);
  }
}
