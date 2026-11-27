// The release smoke's failure paths (`node --test scripts/release-smoke.test.mjs`): the tag
// reader fails closed, a binary that cannot start fails the smoke and leaves nothing behind,
// and a daemon that ignores `daemon stop` is killed within the bounds.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { named, withDaemon } from "./release-smoke.mjs";

const SCRIPT = fileURLToPath(new URL("./release-smoke.mjs", import.meta.url));

test("named reads Vite's tags", () => {
  const html =
    '<script type="module" crossorigin src="/assets/a.js"></script>\n' +
    '<link rel="stylesheet" crossorigin href="/assets/a.css">';
  assert.deepEqual(named(html), ["/assets/a.js", "/assets/a.css"]);
});

for (const [name, html] of [
  ["a single-quoted src", `<script src="/assets/a.js"></script><script src='/assets/b.js'></script><link rel="stylesheet" href="/assets/a.css">`],
  ["a single-quoted rel", `<script src="/assets/a.js"></script><link rel="stylesheet" href="/assets/a.css"><link rel='stylesheet' href="/assets/b.css">`],
  ["an unquoted value", `<script src=/assets/a.js></script><link rel="stylesheet" href="/assets/a.css">`],
  ["an uppercase tag", `<SCRIPT src="/assets/a.js"></SCRIPT><link rel="stylesheet" href="/assets/a.css">`],
  ["an inline script", `<script>1</script><script src="/assets/a.js"></script><link rel="stylesheet" href="/assets/a.css">`],
  ["another link", `<script src="/assets/a.js"></script><link rel="modulepreload" href="/assets/b.js"><link rel="stylesheet" href="/assets/a.css">`],
  ["no stylesheet", `<script src="/assets/a.js"></script>`],
]) {
  test(`named fails on ${name}`, () => {
    assert.throws(() => named(html));
  });
}

const fast = { start: 5_000, stop: 300, reap: 5_000, request: 5_000, build: 5_000 };

test("a binary that cannot start fails without calling the body", async () => {
  const scratch = mkdtempSync(join(tmpdir(), "smoke-test-"));
  try {
    let called = false;
    await assert.rejects(
      withDaemon(join(scratch, "missing", "chaldea"), join(scratch, "state"), async () => {
        called = true;
      }, fast),
      /could not start/,
    );
    assert.equal(called, false);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});

test("the smoke exits 1 for a missing binary and removes its scratch dir", () => {
  const tmp = mkdtempSync(join(tmpdir(), "smoke-test-"));
  // Run through a symlink: the entry check must still recognise the script.
  const link = mkdtempSync(join(tmpdir(), "smoke-link-"));
  symlinkSync(SCRIPT, join(link, "release-smoke.mjs"));
  try {
    const r = spawnSync(process.execPath, [join(link, "release-smoke.mjs")], {
      env: { ...process.env, CHALDEA_SMOKE_EXE: join(tmp, "missing", "chaldea"), TMPDIR: tmp, TEMP: tmp, TMP: tmp },
      encoding: "utf8",
      timeout: 60_000,
    });
    assert.equal(r.status, 1, r.stderr);
    assert.match(r.stderr, /could not start/);
    assert.deepEqual(readdirSync(tmp), []);
  } finally {
    rmSync(tmp, { recursive: true, force: true });
    rmSync(link, { recursive: true, force: true });
  }
});

test("a daemon that ignores daemon stop is killed and reaped", { skip: process.platform === "win32" }, async () => {
  const scratch = mkdtempSync(join(tmpdir(), "smoke-test-"));
  try {
    // A stand-in daemon: `daemon run` listens nowhere, says it does, ignores SIGTERM, and
    // records its PID; `daemon stop` claims success.
    const pidFile = join(scratch, "pid");
    const js = join(scratch, "fake.mjs");
    writeFileSync(
      js,
      `import { writeFileSync } from "node:fs";
const a = process.argv.slice(2);
if (a.includes("stop")) process.exit(0);
writeFileSync(${JSON.stringify(pidFile)}, String(process.pid));
process.on("SIGTERM", () => {});
process.stderr.write("listening on http://127.0.0.1:" + a[a.indexOf("--port") + 1] + "\\n");
setInterval(() => {}, 1000);
`,
    );
    const exe = join(scratch, "chaldea");
    writeFileSync(exe, `#!/bin/sh\nexec '${process.execPath}' '${js}' "$@"\n`);
    chmodSync(exe, 0o755);
    await assert.rejects(withDaemon(exe, join(scratch, "state"), async () => {}, fast), /did not exit within 300 ms/);
    const pid = Number(readFileSync(pidFile, "utf8"));
    assert.throws(() => process.kill(pid, 0), "the stand-in daemon still runs");
    assert.equal(existsSync(join(scratch, "state")), false);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});
