// The end-to-end harness (docs/design/w1-pwa.md §3 D8): the release `chaldea` with the web
// app embedded, run as `daemon run` in a temporary state dir, with FakeCli standing in as
// `claude` through a wrapper script on the daemon's `PATH`. Each test gets its own daemon,
// stopped by PID. Pair codes and tokens never reach a test title, step, or message.

import { type ChildProcess, execFile, spawn } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { delimiter, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { test as base, expect, type Page } from "@playwright/test";

export { expect };

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
export const RELEASE = join(ROOT, "target", "release");
export const CHALDEA = join(RELEASE, "chaldea");
export const FAKE_CLI = join(RELEASE, "chaldea-fake-cli");

const START_WAIT_MS = 30_000;
const STOP_WAIT_MS = 30_000;
const COMMAND_WAIT_MS = 30_000;

function freePort(): Promise<number> {
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

/** One test's daemon: its state dir, port, agent wrapper, and the CLI against it. */
export class Daemon {
  readonly port: number;
  readonly root: string;
  readonly state: string;
  readonly bin: string;
  private child: ChildProcess | null = null;
  private exited: Promise<void> = Promise.resolve();
  private stderr = "";

  constructor(port: number, root: string) {
    this.port = port;
    this.root = root;
    this.state = join(root, "state");
    this.bin = join(root, "bin");
    mkdirSync(this.state, { mode: 0o700 });
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

  /** `daemon run --port <port>` in the foreground; resolves once it listens. */
  async start(): Promise<void> {
    if (this.child) throw new Error("the daemon is already running");
    this.stderr = "";
    const child = spawn(CHALDEA, ["--state-dir", this.state, "daemon", "run", "--port", String(this.port)], {
      env: { ...process.env, PATH: [this.bin, "/usr/bin", "/bin"].join(delimiter) },
      stdio: ["ignore", "ignore", "pipe"],
    });
    this.child = child;
    this.exited = new Promise((ok) => child.once("exit", () => ok()));
    void this.exited.then(() => {
      if (this.child === child) this.child = null;
    });
    child.stderr?.setEncoding("utf8");
    await new Promise<void>((ok, err) => {
      const timer = setTimeout(() => err(new Error(`no listening line within ${START_WAIT_MS} ms`)), START_WAIT_MS);
      child.stderr?.on("data", (d: string) => {
        this.stderr += d;
        if (this.stderr.includes(`listening on ${this.origin}`)) {
          clearTimeout(timer);
          ok();
        }
      });
      child.once("error", (e) => {
        clearTimeout(timer);
        err(e);
      });
      void this.exited.then(() => {
        clearTimeout(timer);
        err(new Error(`the daemon exited before listening:\n${this.stderr}`));
      });
    }).catch(async (e: unknown) => {
      await this.stop();
      throw e;
    });
  }

  /** SIGTERM to the daemon's PID, then SIGKILL past the deadline; resolves once it exited. */
  async stop(): Promise<void> {
    const child = this.child;
    if (!child) return;
    child.kill("SIGTERM");
    const done = await Promise.race([
      this.exited.then(() => true),
      new Promise<boolean>((ok) => setTimeout(() => ok(false), STOP_WAIT_MS)),
    ]);
    if (!done) {
      child.kill("SIGKILL");
      await this.exited;
      throw new Error(`the daemon did not stop within ${STOP_WAIT_MS} ms`);
    }
  }

  /** `chaldea --state-dir <state> <args>`: its stdout as JSON, or a failure without stdout. */
  chaldea(args: string[]): Promise<unknown> {
    return new Promise((ok, err) => {
      execFile(
        CHALDEA,
        ["--state-dir", this.state, ...args],
        { timeout: COMMAND_WAIT_MS, encoding: "utf8" },
        (e, stdout, stderr) => {
          if (e) return err(new Error(`chaldea ${args[0]} failed: ${stderr}`));
          try {
            ok(JSON.parse(stdout));
          } catch {
            ok(stdout.trim());
          }
        },
      );
    });
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
      await new Promise((ok) => setTimeout(ok, 100));
    }
  }

  async head(thread: string): Promise<number> {
    const es = await this.events(thread);
    return es.length === 0 ? 0 : es[es.length - 1]!.seq;
  }

  /** Every file of this test (the state dir included), after the daemon stopped. */
  remove(): void {
    rmSync(this.root, { recursive: true, force: true });
  }

  get log(): string {
    return this.stderr;
  }
}

/**
 * Opens a pairing link in `page` without handing the URL to a Playwright navigation, whose
 * call log would print the fragment: the page navigates itself from `about:blank`.
 */
export async function openLink(page: Page, url: string): Promise<void> {
  await page.goto("about:blank");
  await page.evaluate((u) => {
    location.href = u;
  }, url);
  await page.waitForFunction(() => location.pathname === "/" && document.getElementById("root") !== null);
}

/** The status the header shows (`data-status`). */
export const statusOf = (page: Page) => page.locator("[data-status]");

/** The `#<seq>` of every timeline row, in page order. */
export async function shownSeqs(page: Page): Promise<number[]> {
  const texts = await page.locator("main ol > li p:first-child > span:first-child").allTextContents();
  return texts.map((t) => Number(t.replace(/^#/, "")));
}

/** Waits until the timeline's last row is `#<seq>`. */
export async function untilShown(page: Page, seq: number): Promise<void> {
  await expect(page.locator("main ol > li").last().locator("span").first()).toHaveText(`#${seq}`, {
    timeout: 20_000,
  });
}

export const test = base.extend<{ daemon: Daemon }>({
  daemon: async ({}, use, info) => {
    const root = mkdtempSync(join(tmpdir(), "chaldea-e2e-"));
    const d = new Daemon(await freePort(), root);
    try {
      await d.start();
      await use(d);
    } finally {
      await d.stop().catch(() => {});
      if (info.status !== info.expectedStatus) {
        await info.attach("daemon.log", { body: redact(d.log), contentType: "text/plain" });
      }
      d.remove();
    }
  },
  baseURL: async ({ daemon }, use) => {
    await use(daemon.origin);
  },
});

/** Drops anything shaped like a token or pair code from a log. */
function redact(s: string): string {
  return s.replace(/\bchd[a-z]*_[A-Za-z0-9_-]+/g, "<redacted>");
}
