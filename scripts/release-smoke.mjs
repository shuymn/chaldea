#!/usr/bin/env node
// The packaged smoke of the release command (docs/design/w1-pwa.md §2.1 "Release"), run by
// CI on macOS and Windows after `mise run release`:
//
// 1. The release binary's `daemon run` (a new state dir, a free port) answers `GET /` with
//    200, the CSP, and the bytes of dist/index.html, and every embeddable asset under
//    dist/assets/ (source maps skipped) at `/assets/<path>` with 200, its type, and its bytes.
//    `daemon stop` ends it. Accepted limit: no host checks here that index.html names the
//    right files. The Vite output does not depend on the host, and Playwright on macOS loads
//    the app built from the same inputs; this smoke proves each packaged binary serves every
//    built file.
// 2. The same binary rebuilt without CHALDEA_WEB_DIST, in the same Cargo target dir, answers
//    `/` and one of those assets with 501: no stale table survives the rebuild.
//
// Leaves target/release/chaldea built without the PWA. Node only, no dependencies; the
// exit code is 1 unless every check passed. scripts/release-smoke.test.mjs tests its parts.
//
//   node scripts/release-smoke.mjs

import { spawn } from "node:child_process";
import { lstatSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const DIST = join(root, "packages", "web", "dist");
const EXE = join(root, "target", "release", `chaldea${process.platform === "win32" ? ".exe" : ""}`);

const WAITS = {
  start: 60_000,
  stop: 30_000,
  /** How long a killed process may take to be reaped. */
  reap: 10_000,
  request: 10_000,
  build: 30 * 60_000,
};

// The daemon's allowlist under assets/ (crates/daemon/src/http/pwa/dist.rs). Looked up by
// own property only, so a name like `x.constructor` is not a type.
const TYPES = {
  js: "text/javascript; charset=utf-8",
  css: "text/css; charset=utf-8",
  woff2: "font/woff2",
  png: "image/png",
  wasm: "application/wasm",
};

function fail(message) {
  throw new Error(message);
}

const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));

/**
 * Starts `cmd` and returns it with `ended`, which settles once on its exit or on a spawn
 * failure (an `error` before `spawn`, with no `exit`), so every wait on it can end. An
 * `error` after `spawn` is a signal that could not be delivered: the process still runs.
 */
export function start(cmd, args, opts) {
  const child = spawn(cmd, args, opts);
  let spawned = false;
  const ended = new Promise((ok) => {
    child.once("spawn", () => {
      spawned = true;
    });
    child.once("exit", (code, signal) => ok({ code, signal, error: null }));
    child.on("error", (error) => {
      if (!spawned) ok({ code: null, signal: null, error });
    });
  });
  let done = false;
  void ended.then(() => {
    done = true;
  });
  return { child, ended, running: () => !done };
}

/** `ended` within `ms`, else null. */
async function within(p, ms) {
  let timer;
  const late = new Promise((ok) => {
    timer = setTimeout(() => ok(null), ms);
  });
  try {
    return await Promise.race([p.ended, late]);
  } finally {
    clearTimeout(timer);
  }
}

/** Kills `p` if it still runs and waits for it within `WAITS.reap`; throws past that. */
// A failed kill of this script's own child (EPERM, ESRCH) is not a state the smoke can reach
// in practice; it is reported and the process still counts as running. Cleanup then goes
// on, bounded by the CI job's timeout.
export async function reap(p, waits = WAITS) {
  if (!p.running()) return;
  if (!p.child.kill("SIGKILL")) fail(`process ${p.child.pid} could not be signalled; it may still run`);
  if ((await within(p, waits.reap)) === null) {
    fail(`process ${p.child.pid} was not reaped within ${waits.reap} ms of SIGKILL`);
  }
}

/** Runs `cmd` to its end within `ms` (killed past it); resolves with its exit. */
async function run(cmd, args, opts, ms, waits = WAITS) {
  const p = start(cmd, args, opts);
  try {
    const end = await within(p, ms);
    if (end === null) fail(`${cmd} ${args.join(" ")} did not end within ${ms} ms`);
    if (end.error) fail(`${cmd} could not start: ${end.error.message}`);
    return end;
  } finally {
    await reap(p, waits);
  }
}

/** A port that was free a moment ago on 127.0.0.1. */
function freePort() {
  return new Promise((ok, err) => {
    const s = createServer();
    s.once("error", err);
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => ok(port));
    });
  });
}

const csp = (port) =>
  `default-src 'self'; connect-src 'self' ws://127.0.0.1:${port}; object-src 'none'; ` +
  "base-uri 'none'; frame-ancestors 'none'; form-action 'none'; " +
  "require-trusted-types-for 'script'; trusted-types 'none'";

/**
 * Runs `body(port)` against `<exe> --state-dir <state> daemon run --port <free>`, then stops
 * it with `daemon stop`. `state` must not exist yet: the daemon creates it with its private
 * descriptor (an existing directory keeps the ACL it inherited, which the daemon may refuse
 * on Windows). On every exit path a daemon still running is killed by PID and its exit
 * awaited within a bound; a kill that cannot be delivered or a daemon not reaped in time is
 * reported as a failure.
 */
export async function withDaemon(exe, state, body, waits = WAITS) {
  const port = await freePort();
  const p = start(exe, ["--state-dir", state, "daemon", "run", "--port", String(port)], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  let stderr = "";
  p.child.stderr?.setEncoding("utf8");
  p.child.stderr?.on("data", (d) => {
    stderr += d;
  });
  try {
    const listening = `listening on http://127.0.0.1:${port}`;
    const deadline = Date.now() + waits.start;
    while (!stderr.includes(listening)) {
      if (!p.running()) {
        const end = await p.ended;
        fail(
          end.error
            ? `the daemon could not start: ${end.error.message}`
            : `the daemon exited before listening (${end.code ?? end.signal})`,
        );
      }
      if (Date.now() > deadline) fail(`no listening line within ${waits.start} ms`);
      await sleep(20);
    }
    await body(port);
    const stop = await run(exe, ["--state-dir", state, "daemon", "stop"], { stdio: "ignore" }, waits.stop, waits);
    if (stop.code !== 0) fail(`daemon stop exited ${stop.code ?? stop.signal}`);
    const end = await within(p, waits.stop);
    if (end === null) fail(`the daemon did not exit within ${waits.stop} ms of daemon stop`);
    if (end.code !== 0) fail(`the daemon exited ${end.code ?? end.signal} after daemon stop`);
  } catch (e) {
    e.message += `\n--- daemon stderr ---\n${stderr}`;
    throw e;
  } finally {
    await reap(p, waits);
  }
}

async function get(port, path) {
  const r = await fetch(`http://127.0.0.1:${port}${path}`, {
    redirect: "manual",
    signal: AbortSignal.timeout(WAITS.request),
  });
  return { status: r.status, headers: r.headers, body: Buffer.from(await r.arrayBuffer()) };
}

/**
 * Every file under `<distDir>/assets/` as `{ path, file, type }`, sorted by path, with
 * source maps skipped as the daemon's build skips them. Fails on a type outside the
 * allowlist, on anything that is neither a file nor a directory (symlinks included, never
 * followed, the `assets/` root included), and on an empty directory.
 */
export function distAssets(distDir) {
  const out = [];
  const walk = (dir, rel) => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const parts = [...rel, e.name];
      const file = join(dir, e.name);
      if (e.isDirectory()) walk(file, parts);
      else if (!e.isFile()) fail(`assets/${parts.join("/")} is neither a file nor a directory`);
      else if (!e.name.endsWith(".map")) {
        const ext = e.name.includes(".") ? e.name.slice(e.name.lastIndexOf(".") + 1) : "";
        const type = Object.hasOwn(TYPES, ext) ? TYPES[ext] : undefined;
        if (!type) fail(`assets/${parts.join("/")} has a type the daemon does not serve`);
        out.push({ path: `/assets/${parts.join("/")}`, file, type });
      }
    }
  };
  const assets = join(distDir, "assets");
  if (!lstatSync(assets).isDirectory()) fail("dist/assets is not a directory (a symlink is refused)");
  walk(assets, []);
  if (out.length === 0) fail("dist/assets/ holds no file");
  return out.sort((x, y) => (x.path < y.path ? -1 : 1));
}

function expect(what, actual, wanted) {
  if (actual !== wanted) fail(`${what}: ${JSON.stringify(actual)}, wanted ${JSON.stringify(wanted)}`);
}

async function build(env) {
  const end = await run(
    "cargo",
    ["build", "--release", "--locked", "-p", "chaldea-cli"],
    { cwd: root, env, stdio: "inherit" },
    WAITS.build,
  );
  if (end.code !== 0) fail(`cargo build exited ${end.code ?? end.signal}`);
}

async function main(exe = EXE, dist = DIST) {
  const scratch = mkdtempSync(join(tmpdir(), "chaldea-smoke-"));
  try {
    const assets = distAssets(dist);
    await withDaemon(exe, join(scratch, "state-pwa"), async (port) => {
      const index = readFileSync(join(dist, "index.html"));
      const page = await get(port, "/");
      expect("GET / status", page.status, 200);
      expect("GET / Content-Type", page.headers.get("content-type"), "text/html; charset=utf-8");
      expect("GET / Content-Security-Policy", page.headers.get("content-security-policy"), csp(port));
      if (!page.body.equals(index)) fail("GET / is not dist/index.html");
      for (const a of assets) {
        const r = await get(port, a.path);
        expect(`GET ${a.path} status`, r.status, 200);
        expect(`GET ${a.path} Content-Type`, r.headers.get("content-type"), a.type);
        if (!r.body.equals(readFileSync(a.file))) fail(`GET ${a.path} is not its dist file`);
        console.log(`ok ${a.path} ${a.type} ${r.body.length} bytes`);
      }
      console.log(`ok / with the CSP; ${assets.length} assets`);
    });

    const env = { ...process.env };
    delete env.CHALDEA_WEB_DIST;
    await build(env);
    await withDaemon(exe, join(scratch, "state-rust-only"), async (port) => {
      for (const p of ["/", assets[0].path]) {
        const r = await get(port, p);
        expect(`GET ${p} status without the PWA`, r.status, 501);
        console.log(`ok ${p} 501 without the PWA`);
      }
    });
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
}

if (process.argv[1] && realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url))) {
  // Failure unless `main` resolves: a promise that never settles leaves this exit code.
  process.exitCode = 1;
  main(process.env.CHALDEA_SMOKE_EXE || EXE, process.env.CHALDEA_SMOKE_DIST || DIST).then(
    () => {
      console.log("release smoke passed");
      process.exitCode = 0;
    },
    (e) => {
      console.error(`release smoke failed: ${e?.stack ?? e}`);
      process.exitCode = 1;
    },
  );
}
