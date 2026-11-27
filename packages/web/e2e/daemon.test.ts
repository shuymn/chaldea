// The harness's failure paths (Vitest): a program that cannot start, a daemon that never
// listens, a daemon that ignores SIGTERM, a command past its deadline or cancelled, and the
// redaction of diagnostics. Stand-ins are Node scripts, so no release build is needed.

import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, describe, expect, test } from "vitest";

import { Daemon, type Waits } from "./daemon";
import { reap, redact, runProc, startProc } from "./proc";

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

  // Windows Node ends the process on SIGTERM whatever its handlers, so there is no escalation.
  test.skipIf(process.platform === "win32")("a daemon that ignores SIGTERM is killed and reaped past the stop deadline", async () => {
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

  test("a command that cannot start throws", async () => {
    await expect(runProc(join(scratch(), "missing"), [], { deadlineMs: 1_000 })).rejects.toThrow(/could not start/);
  });

  test("redact removes tokens, pair codes, and pairing fragments", () => {
    const marker = "TEST_DUMMY_MARKER";
    const s = redact(`chd_${marker} chdp_${marker} http://127.0.0.1:1/#pair=${marker} ok`);
    expect(s.includes(marker)).toBe(false);
    expect(s).toContain("ok");
  });

  test("redact removes a prefix glued to other text, like the daemon's redactor", () => {
    const secret = "A".repeat(43);
    for (const p of ["chd_", "chdp_", "chdt_"]) {
      const s = redact(`prefix${p}${secret} and x${p}${secret}`);
      expect(s.includes(secret), p).toBe(false);
      expect(s).toBe(`prefix${p}<redacted> and x${p}<redacted>`);
    }
  });

  test("a spawn failure's message is redacted", async () => {
    const root = scratch();
    const secret = "B".repeat(43);
    const missing = join(root, `chdp_${secret}`);
    const e = await runProc(missing, [], { deadlineMs: 1_000 }).catch((x: unknown) => x as Error);
    expect(e).toBeInstanceOf(Error);
    expect((e as Error).message.includes(secret)).toBe(false);
    const d = new Daemon({ root, port: 1, program: [missing], waits: fast });
    const f = await d.start().catch((x: unknown) => x as Error);
    expect((f as Error).message).toMatch(/could not start/);
    expect((f as Error).message.includes(secret)).toBe(false);
  });

  test("a failed kill is not taken for an exit: reap throws and the process still counts as running", async () => {
    const root = scratch();
    const s = standIn(root, "setInterval(() => {}, 1000);");
    const p = startProc(s.program[0]!, s.program.slice(1), { stdio: "ignore" });
    await new Promise((ok) => p.child.once("spawn", ok));
    const real = p.child.kill.bind(p.child);
    // What Node does when a signal cannot be delivered: `error`, `false`, and no `exit`.
    p.child.kill = () => {
      p.child.emit("error", Object.assign(new Error("kill EPERM"), { code: "EPERM" }));
      return false;
    };
    try {
      await expect(reap(p, 300)).rejects.toThrow(/could not be signalled|not reaped/);
      expect(p.running()).toBe(true);
      expect(alive(p.child.pid!)).toBe(true);
    } finally {
      p.child.kill = real;
      await reap(p, 5_000);
    }
    expect(p.running()).toBe(false);
  });

  test("a descendant holding the capture pipes does not keep the command or Node waiting", async () => {
    const root = scratch();
    // The child starts a descendant that inherits stdout and stderr, then exits at once.
    const holderPid = join(root, "holder");
    const js = join(root, "parent.mjs");
    writeFileSync(
      js,
      `import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
const h = spawn(process.execPath, ["-e", "setTimeout(() => {}, 30000)"], { stdio: ["ignore", "inherit", "inherit"] });
writeFileSync(${JSON.stringify(holderPid)}, String(h.pid));
h.unref();
process.stdout.write("out");
`,
    );
    // A separate Node runs `runProc` and must exit on its own soon after, while the
    // descendant still holds the pipes for 30 s.
    const proc = new URL("./proc.ts", import.meta.url).href;
    const runner = `import { runProc } from ${JSON.stringify(proc)};\nconst r = await runProc(process.execPath, [${JSON.stringify(js)}], { deadlineMs: 5_000 });\nprocess.stdout.write(JSON.stringify({ code: r.code, stdout: r.stdout, drained: r.drained }));\n`;
    const started = Date.now();
    const out = spawnSync(process.execPath, ["--input-type=module", "-e", runner], { encoding: "utf8", timeout: 10_000 });
    try {
      expect(out.status, out.stderr).toBe(0);
      expect(JSON.parse(out.stdout)).toEqual({ code: 0, stdout: "out", drained: false });
      expect(Date.now() - started).toBeLessThan(4_000);
    } finally {
      process.kill(Number(readFileSync(holderPid, "utf8")), "SIGKILL");
    }
  });
});
