// One test's daemon (docs/design/w1-pwa.md §3 D8): the release `chaldea` as `daemon run` in a
// state dir under the test's scratch root, with FakeCli standing in as `claude` through a
// wrapper script on the daemon's `PATH`. It is stopped by PID. No Playwright import, so the
// lifecycle tests (daemon.test.ts) run under Vitest.

import { chmodSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { delimiter, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { type Proc, reap, redact, runProc, signal, startProc, within } from "./proc";

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
export const RELEASE = join(ROOT, "target", "release");
export const CHALDEA = join(RELEASE, "chaldea");
export const FAKE_CLI = join(RELEASE, "chaldea-fake-cli");

export interface Waits {
  readonly start: number;
  readonly stop: number;
  readonly reap: number;
  readonly command: number;
}

export const WAITS: Waits = { start: 30_000, stop: 30_000, reap: 10_000, command: 30_000 };

export function freePort(): Promise<number> {
  return new Promise((ok, err) => {
    const s = createServer();
    s.once("error", err);
    s.listen(0, "127.0.0.1", () => {
      const a = s.address();
      s.close(() => (a && typeof a === "object" ? ok(a.port) : err(new Error("no port"))));
    });
  });
}

export interface Envelope {
  readonly seq: number;
  readonly kind: string;
  readonly launch_id: string | null;
  readonly payload: Record<string, unknown>;
}

const sleep = (ms: number) => new Promise((ok) => setTimeout(ok, ms));

export class Daemon {
  readonly port: number;
  readonly root: string;
  /** Not created here: the daemon creates it with its private descriptor. */
  readonly state: string;
  readonly bin: string;
  /** The program and its leading arguments (the release `chaldea` unless a test replaces it). */
  private readonly program: readonly string[];
  private readonly waits: Waits;
  private proc: Proc | null = null;
  private stderr = "";

  constructor(opts: { root: string; port: number; program?: readonly string[]; waits?: Waits }) {
    this.port = opts.port;
    this.root = opts.root;
    this.program = opts.program ?? [CHALDEA];
    this.waits = opts.waits ?? WAITS;
    this.state = join(this.root, "state");
    this.bin = join(this.root, "bin");
    mkdirSync(this.bin);
    // FakeCli `echo` as `claude`: it emits Claude's hooks through the real emitter, and its
    // launch arguments (`--settings`, `--session-id`, `--resume`) follow `--`.
    const wrapper = join(this.bin, "claude");
    writeFileSync(
      wrapper,
      `#!/bin/sh\nexec '${FAKE_CLI}' --mode echo --emitter '${CHALDEA}' --agent claude --max-life-secs 600 -- "$@"\n`,
    );
    chmodSync(wrapper, 0o755);
  }

  get origin(): string {
    return `http://127.0.0.1:${this.port}`;
  }

  /** The daemon's stderr so far, redacted. */
  get log(): string {
    return redact(this.stderr);
  }

  /** `daemon run --port <port>` in the foreground; resolves once it listens. */
  async start(): Promise<void> {
    if (this.proc?.running()) throw new Error("the daemon is already running");
    this.stderr = "";
    const [cmd, ...lead] = this.program;
    const p = startProc(cmd!, [...lead, "--state-dir", this.state, "daemon", "run", "--port", String(this.port)], {
      env: { ...process.env, PATH: [this.bin, "/usr/bin", "/bin"].join(delimiter) },
      stdio: ["ignore", "ignore", "pipe"],
    });
    this.proc = p;
    p.child.stderr?.setEncoding("utf8");
    p.child.stderr?.on("data", (d: string) => {
      this.stderr += d;
    });
    try {
      const deadline = Date.now() + this.waits.start;
      while (!this.stderr.includes(`listening on ${this.origin}`)) {
        if (!p.running()) {
          const end = await p.ended;
          throw new Error(
            end.error
              ? `the daemon could not start: ${end.error.message}`
              : `the daemon exited before listening (${end.code ?? end.signal}):\n${this.log}`,
          );
        }
        if (Date.now() > deadline) throw new Error(`no listening line within ${this.waits.start} ms:\n${this.log}`);
        await sleep(20);
      }
    } catch (e) {
      await reap(p, this.waits.reap);
      throw e;
    }
  }

  /**
   * SIGTERM to the daemon's PID; past the stop deadline SIGKILL, reaped within its own bound,
   * and the call throws. Resolves once the daemon is gone.
   */
  async stop(): Promise<void> {
    const p = this.proc;
    if (!p?.running()) return;
    signal(p, "SIGTERM");
    if ((await within(p, this.waits.stop)) === null) {
      await reap(p, this.waits.reap);
      throw new Error(`the daemon did not stop within ${this.waits.stop} ms of SIGTERM`);
    }
  }

  /** `chaldea --state-dir <state> <args>`: its stdout as JSON (or trimmed text). */
  async chaldea(args: string[]): Promise<unknown> {
    const r = await runProc(CHALDEA, ["--state-dir", this.state, ...args], { deadlineMs: this.waits.command });
    if (r.code !== 0) throw new Error(`chaldea ${args[0]} exited ${r.code ?? r.signal}: ${redact(r.stderr)}`);
    try {
      return JSON.parse(r.stdout);
    } catch {
      return r.stdout.trim();
    }
  }

  /** A fresh pairing URL (`open --print-url`); it holds a pair code. */
  async pairUrl(): Promise<string> {
    const url = await this.chaldea(["open", "--print-url"]);
    if (typeof url !== "string" || !url.startsWith(`${this.origin}/#pair=`)) {
      throw new Error("open --print-url printed no pairing URL");
    }
    return url;
  }

  /** A new Claude thread in a new directory under this test's root; its thread id. */
  async spawnThread(opts: { cwd?: string; prompt?: string } = {}): Promise<{ thread: string; launch: string }> {
    const cwd = opts.cwd ?? mkdtempSync(join(this.root, "cwd-"));
    mkdirSync(cwd, { recursive: true });
    const args = ["spawn", "--agent", "claude", "--cwd", cwd];
    if (opts.prompt !== undefined) args.push("--prompt", opts.prompt);
    const v = (await this.chaldea(args)) as { outcome: string; thread_id: string; launch_id: string };
    if (v.outcome !== "started") throw new Error(`spawn: ${v.outcome}`);
    return { thread: v.thread_id, launch: v.launch_id };
  }

  /** The thread's events through the owner token (the test's own view of the daemon). */
  async events(thread: string, afterSeq = 0): Promise<Envelope[]> {
    const token = readFileSync(join(this.state, "owner.token"), "utf8").trim();
    const out: Envelope[] = [];
    let after = afterSeq;
    for (;;) {
      const q = new URLSearchParams({ stream_id: `thread:${thread}`, after_seq: String(after), limit: "1000" });
      const r = await fetch(`${this.origin}/api/events?${q}`, {
        headers: { Authorization: `Bearer ${token}` },
        signal: AbortSignal.timeout(10_000),
      });
      if (!r.ok) throw new Error(`GET /api/events: ${r.status}`);
      const body = (await r.json()) as { mode: string; events?: Envelope[]; has_more?: boolean };
      if (body.mode !== "replay" || !body.events) throw new Error(`GET /api/events: mode ${body.mode}`);
      out.push(...body.events);
      if (!body.has_more || body.events.length === 0) return out;
      after = body.events[body.events.length - 1]!.seq;
    }
  }

  /** Polls the thread until `pred` holds over its events; returns them. */
  async waitEvents(thread: string, pred: (es: Envelope[]) => boolean, ms = 20_000): Promise<Envelope[]> {
    const end = Date.now() + ms;
    for (;;) {
      const es = await this.events(thread);
      if (pred(es)) return es;
      if (Date.now() > end) throw new Error(`events of the thread never matched (${es.length} events)`);
      await sleep(100);
    }
  }
}
