// The harness's failure paths (Vitest): a program that cannot start, a daemon that never
// listens, a daemon that ignores SIGTERM, a command past its deadline or cancelled, and the
// redaction of diagnostics. Stand-ins are Node scripts, so no release build is needed.

import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, describe, expect, test } from "vitest";

import { Daemon, type Waits } from "./daemon";
import { redact, runProc } from "./proc";

const fast: Waits = { start: 1_000, stop: 300, reap: 5_000, command: 1_000 };
const roots: string[] = [];

function scratch(): string {
  const r = mkdtempSync(join(tmpdir(), "e2e-harness-"));
  roots.push(r);
  return r;
}

afterEach(() => {
  for (const r of roots.splice(0)) rmSync(r, { recursive: true, force: true });
});

const alive = (pid: number) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

/** A stand-in daemon that records its PID; `body` is the rest of its script. */
function standIn(root: string, body: string): { program: string[]; pid: () => number } {
  const pidFile = join(root, "pid");
  const js = join(root, "stand-in.mjs");
  writeFileSync(
    js,
    `import { writeFileSync } from "node:fs";\nwriteFileSync(${JSON.stringify(pidFile)}, String(process.pid));\nconst port = process.argv[process.argv.indexOf("--port") + 1];\n// Bounds a leak when a test fails.\nsetTimeout(() => process.exit(0), 30_000);\n${body}\n`,
  );
  return { program: [process.execPath, js], pid: () => Number(readFileSync(pidFile, "utf8")) };
}

describe("harness lifecycle", () => {
  test("a program that cannot start fails start at once", async () => {
    const root = scratch();
    const d = new Daemon({ root, port: 1, program: [join(root, "missing", "chaldea")], waits: fast });
    await expect(d.start()).rejects.toThrow(/could not start/);
    await d.stop();
  });

  test("a daemon that never listens is killed past the start deadline", async () => {
    const root = scratch();
    const s = standIn(root, "setInterval(() => {}, 1000);");
    const d = new Daemon({ root, port: 1, program: s.program, waits: fast });
    await expect(d.start()).rejects.toThrow(/no listening line within 1000 ms/);
    expect(alive(s.pid())).toBe(false);
  });

  test("a daemon that ignores SIGTERM is killed and reaped past the stop deadline", async () => {
    const root = scratch();
    const s = standIn(
      root,
      `process.on("SIGTERM", () => {});\nprocess.stderr.write("listening on http://127.0.0.1:" + port + "\\n");\nsetInterval(() => {}, 1000);`,
    );
    const d = new Daemon({ root, port: 4567, program: s.program, waits: fast });
    await d.start();
    const pid = s.pid();
    expect(alive(pid)).toBe(true);
    await expect(d.stop()).rejects.toThrow(/did not stop within 300 ms/);
    expect(alive(pid)).toBe(false);
  });

  test("a command past its deadline is killed and reaped", async () => {
    const root = scratch();
    const s = standIn(root, `process.on("SIGTERM", () => {});\nsetInterval(() => {}, 1000);`);
    await expect(runProc(s.program[0]!, s.program.slice(1), { deadlineMs: 300 })).rejects.toThrow(
      /did not end within 300 ms/,
    );
    expect(alive(s.pid())).toBe(false);
  });

  test("a cancelled command is killed and reaped", async () => {
    const root = scratch();
    const s = standIn(root, `process.on("SIGTERM", () => {});\nsetInterval(() => {}, 1000);`);
    const ac = new AbortController();
    const run = runProc(s.program[0]!, s.program.slice(1), { deadlineMs: 60_000, signal: ac.signal });
    setTimeout(() => ac.abort(), 300);
    await expect(run).rejects.toThrow(/cancelled/);
    expect(alive(s.pid())).toBe(false);
  });

  test("a command that cannot start throws", async () => {
    await expect(runProc(join(scratch(), "missing"), [], { deadlineMs: 1_000 })).rejects.toThrow(/could not start/);
  });

  test("redact removes tokens, pair codes, and pairing fragments", () => {
    const marker = "TEST_DUMMY_MARKER";
    const s = redact(`chd_${marker} chdp_${marker} http://127.0.0.1:1/#pair=${marker} ok`);
    expect(s.includes(marker)).toBe(false);
    expect(s).toContain("ok");
  });
});
