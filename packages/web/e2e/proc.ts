// Child processes of the end-to-end harness with a bounded lifecycle: `ended` settles once on
// the exit, or on a spawn failure (an `error` before `spawn`; an `error` after it is a
// signal that could not be delivered, and the process still runs). Every wait has a
// deadline, a process past its deadline is killed with SIGKILL, and the kill is itself
// awaited within a bound. Messages pass through `redact`.

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
/** How long the capture pipes may stay open after the exit (a descendant may hold them). */
export const DRAIN_MS = 1_000;

/** Drops every `chd_`, `chdp_`, and `chdt_` secret and pairing fragment, as the daemon's redactor does. */
export function redact(s: string): string {
  return s.replace(/(chd[pt]?_)[A-Za-z0-9_-]+/g, "$1<redacted>").replace(/#pair=[^\s"'<>]*/g, "#pair=<redacted>");
}

export function startProc(cmd: string, args: readonly string[], opts: SpawnOptions): Proc {
  const child = spawn(cmd, args, opts);
  let done = false;
  let spawned = false;
  const ended = new Promise<End>((ok) => {
    child.once("spawn", () => {
      spawned = true;
    });
    child.once("exit", (code, signal) => ok({ code, signal, error: null }));
    child.on("error", (error) => {
      // Only a failed spawn ends the process; a failed kill is reported by `signal`.
      if (!spawned) ok({ code: null, signal: null, error: new Error(redact(error.message)) });
    });
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

/**
 * Sends `sig` to a running `p`; throws when it could not be delivered.
 *
 * Accepted limit: a failed kill of the harness's own child (EPERM, ESRCH) is not a state it
 * reaches in practice. It is reported and `p` still counts as running; the caller's cleanup
 * then goes on, bounded by the CI job's timeout.
 */
export function signal(p: Proc, sig: NodeJS.Signals): void {
  if (p.running() && !p.child.kill(sig)) {
    throw new Error(`process ${p.child.pid} could not be signalled (${sig}); it may still run`);
  }
}

/** SIGKILLs `p` if it still runs and waits for its exit within `ms`; throws otherwise. */
export async function reap(p: Proc, ms = REAP_MS): Promise<void> {
  if (!p.running()) return;
  signal(p, "SIGKILL");
  if ((await within(p, ms)) === null) {
    throw new Error(`process ${p.child.pid} was not reaped within ${ms} ms of SIGKILL`);
  }
}

export interface RunResult extends End {
  readonly stdout: string;
  readonly stderr: string;
  /** Both pipes closed within `DRAIN_MS` of the exit; otherwise they were destroyed. */
  readonly drained: boolean;
}

/**
 * Runs `cmd` to its end within `deadlineMs`; past it the process is killed and reaped and
 * the call throws. A spawn failure throws too. With `capture`, stdout and stderr are read
 * until both close, at most `DRAIN_MS` after the exit, then destroyed.
 */
export async function runProc(
  cmd: string,
  args: readonly string[],
  opts: SpawnOptions & { deadlineMs: number; reapMs?: number; capture?: boolean },
): Promise<RunResult> {
  const { deadlineMs, reapMs, capture = true, ...spawnOpts } = opts;
  const p = startProc(cmd, args, capture ? { ...spawnOpts, stdio: ["ignore", "pipe", "pipe"] } : spawnOpts);
  let stdout = "";
  let stderr = "";
  const pipes = [p.child.stdout, p.child.stderr].filter((s) => s !== null);
  const closed = Promise.all(pipes.map((s) => new Promise<void>((ok) => s.once("close", () => ok()))));
  p.child.stdout?.setEncoding("utf8");
  p.child.stderr?.setEncoding("utf8");
  p.child.stdout?.on("data", (d: string) => {
    stdout += d;
  });
  p.child.stderr?.on("data", (d: string) => {
    stderr += d;
  });
  try {
    const end = await within(p, deadlineMs);
    if (end === null) throw new Error(`${redact(cmd)} did not end within ${deadlineMs} ms`);
    if (end.error) throw new Error(`${redact(cmd)} could not start: ${end.error.message}`);
    let timer: ReturnType<typeof setTimeout> | undefined;
    const drained = await Promise.race([
      closed.then(() => true),
      new Promise<boolean>((ok) => {
        timer = setTimeout(() => ok(false), DRAIN_MS);
      }),
    ]);
    clearTimeout(timer);
    return { ...end, stdout, stderr, drained };
  } finally {
    for (const s of pipes) s.destroy();
    await reap(p, reapMs);
  }
}
