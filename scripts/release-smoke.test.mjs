// The release smoke's parts (`node --test scripts/release-smoke.test.mjs`): the asset list
// comes from dist/assets/ and fails closed, a binary that cannot start fails the smoke and
// leaves nothing behind, and a daemon that ignores `daemon stop` is killed within the bounds.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { distAssets, reap, start, withDaemon } from "./release-smoke.mjs";

const SCRIPT = fileURLToPath(new URL("./release-smoke.mjs", import.meta.url));

test("distAssets lists every served file under assets/, skipping maps", () => {
  const d = mkdtempSync(join(tmpdir(), "smoke-test-"));
  try {
    mkdirSync(join(d, "assets", "sub"), { recursive: true });
    for (const f of ["index-a.js", "index-a.js.map", "index-b.css", "sub/f.woff2", "sub/i.png", "t.wasm"]) {
      writeFileSync(join(d, "assets", f), f);
    }
    assert.deepEqual(
      distAssets(d).map((a) => [a.path, a.type]),
      [
        ["/assets/index-a.js", "text/javascript; charset=utf-8"],
        ["/assets/index-b.css", "text/css; charset=utf-8"],
        ["/assets/sub/f.woff2", "font/woff2"],
        ["/assets/sub/i.png", "image/png"],
        ["/assets/t.wasm", "application/wasm"],
      ],
    );
  } finally {
    rmSync(d, { recursive: true, force: true });
  }
});

for (const [name, files] of [
  ["a type the daemon does not serve", ["a.js", "b.svg"]],
  ["no extension", ["a.js", "README"]],
  ["an inherited property as the extension (toString)", ["a.js", "foo.toString"]],
  ["an inherited property as the extension (constructor)", ["a.js", "x.constructor"]],
  ["an empty directory", []],
]) {
  test(`distAssets fails on ${name}`, () => {
    const d = mkdtempSync(join(tmpdir(), "smoke-test-"));
    try {
      mkdirSync(join(d, "assets"));
      for (const f of files) writeFileSync(join(d, "assets", f), f);
      assert.throws(() => distAssets(d));
    } finally {
      rmSync(d, { recursive: true, force: true });
    }
  });
}

test("distAssets refuses an assets/ root that is a symlink", { skip: process.platform === "win32" }, () => {
  const d = mkdtempSync(join(tmpdir(), "smoke-test-"));
  try {
    mkdirSync(join(d, "real"));
    writeFileSync(join(d, "real", "a.js"), "a");
    symlinkSync(join(d, "real"), join(d, "assets"));
    assert.throws(() => distAssets(d), /a symlink is refused/);
  } finally {
    rmSync(d, { recursive: true, force: true });
  }
});

test("distAssets fails on a symlink and does not follow it", { skip: process.platform === "win32" }, () => {
  const d = mkdtempSync(join(tmpdir(), "smoke-test-"));
  try {
    mkdirSync(join(d, "assets"));
    writeFileSync(join(d, "assets", "a.js"), "a");
    symlinkSync(join(d, "assets", "a.js"), join(d, "assets", "b.js"));
    assert.throws(() => distAssets(d), /neither a file nor a directory/);
  } finally {
    rmSync(d, { recursive: true, force: true });
  }
});

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
  // Its own dist, so the test needs no built packages/web/dist.
  const dist = mkdtempSync(join(tmpdir(), "smoke-dist-"));
  mkdirSync(join(dist, "assets"));
  writeFileSync(join(dist, "index.html"), "<!doctype html>");
  writeFileSync(join(dist, "assets", "a.js"), "a");
  try {
    const r = spawnSync(process.execPath, [join(link, "release-smoke.mjs")], {
      env: {
        ...process.env,
        CHALDEA_SMOKE_EXE: join(tmp, "missing", "chaldea"),
        CHALDEA_SMOKE_DIST: dist,
        TMPDIR: tmp,
        TEMP: tmp,
        TMP: tmp,
      },
      encoding: "utf8",
      timeout: 60_000,
    });
    assert.equal(r.status, 1, r.stderr);
    assert.match(r.stderr, /could not start/);
    assert.deepEqual(readdirSync(tmp), []);
  } finally {
    rmSync(tmp, { recursive: true, force: true });
    rmSync(link, { recursive: true, force: true });
    rmSync(dist, { recursive: true, force: true });
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

test("a failed kill is not taken for an exit", async () => {
  const p = start(process.execPath, ["-e", "setTimeout(() => {}, 30000)"], { stdio: "ignore" });
  await new Promise((ok) => p.child.once("spawn", ok));
  const real = p.child.kill.bind(p.child);
  // What Node does when a signal cannot be delivered: `error`, `false`, and no `exit`.
  p.child.kill = () => {
    p.child.emit("error", Object.assign(new Error("kill EPERM"), { code: "EPERM" }));
    return false;
  };
  try {
    await assert.rejects(reap(p, { ...fast, reap: 300 }), /could not be signalled/);
    assert.equal(p.running(), true);
  } finally {
    p.child.kill = real;
    await reap(p, fast);
  }
  assert.equal(p.running(), false);
});
