#!/usr/bin/env node
// The packaged smoke of the release command (docs/design/w1-pwa.md §2.1 "Release"), run by
// CI on macOS and Windows after `mise run release`:
//
// 1. The release binary's `daemon run` (a new state dir, a free port) answers `GET /` with
//    200, the CSP, and the bytes of dist/index.html, and every script and stylesheet the page
//    names with 200, its type, and the bytes of its dist file. `daemon stop` ends it.
// 2. The same binary rebuilt without CHALDEA_WEB_DIST, in the same Cargo target dir, answers
//    `/` and one of those assets with 501: no stale table survives the rebuild.
//
// Leaves target/release/chaldea built without the PWA. Node only, no dependencies; the
// exit code is 1 unless every check passed. scripts/release-smoke.test.mjs tests its parts.
//
//   node scripts/release-smoke.mjs

import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, realpathSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const dist = join(root, "packages", "web", "dist");
export const EXE = join(root, "target", "release", `chaldea${process.platform === "win32" ? ".exe" : ""}`);

export const WAITS = {
  start: 60_000,
  stop: 30_000,
  /** How long a killed process may take to be reaped. */
  reap: 10_000,
  request: 10_000,
  build: 30 * 60_000,
};

const TYPES = {
  js: "text/javascript; charset=utf-8",
  css: "text/css; charset=utf-8",
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
export async function reap(p, waits = WAITS) {
  if (!p.running()) return;
  if (!p.child.kill("SIGKILL")) fail(`process ${p.child.pid} could not be signalled; it may still run`);
  if ((await within(p, waits.reap)) === null) {
    fail(`process ${p.child.pid} was not reaped within ${waits.reap} ms of SIGKILL`);
  }
}

/** Runs `cmd` to its end within `ms` (killed past it); resolves with its exit. */
export async function run(cmd, args, opts, ms, waits = WAITS) {
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
export function freePort() {
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
 * on Windows). On every exit path the daemon is gone (killed by PID past the stop deadline,
 * then reaped within a bound).
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

// The one tag form `named` reads: lowercase `<script` or `<link`, then attributes that are
// bare names or double-quoted values, then `>`. Every other `<script`/`<link` spelling fails
// the smoke rather than being skipped.
const TAG = /^<(script|link)((?:\s+[a-z-]+(?:="[^"<>]*")?)*)\s*\/?>$/;
const ATTR = /\s+([a-z-]+)(?:="([^"<>]*)")?/g;

/**
 * The `src` of every `<script>` and the `href` of every `<link rel="stylesheet">` in `html`.
 * Comments are skipped. Fails on a tag it cannot read, an inline script, any other `<link>`,
 * an unterminated comment, or a page that names no script or no stylesheet.
 */
export function named(html) {
  const scripts = [];
  const styles = [];
  // Commented-out tags name nothing; an unterminated comment is a page the smoke cannot read.
  const live = html.replace(/<!--[\s\S]*?-->/g, "");
  if (live.includes("<!--")) fail("index.html has an unterminated comment");
  for (const m of live.matchAll(/<\s*\/?\s*(script|link)\b[^>]*>?/gi)) {
    const text = m[0];
    if (text === "</script>") continue;
    const t = TAG.exec(text);
    if (!t) fail(`index.html has a tag the smoke cannot read: ${text}`);
    const attrs = new Map();
    for (const a of t[2].matchAll(ATTR)) {
      if (attrs.has(a[1])) fail(`index.html repeats ${a[1]} in ${text}`);
      attrs.set(a[1], a[2] ?? "");
    }
    if (t[1] === "script") {
      if (!attrs.get("src")) fail(`index.html has a script without src: ${text}`);
      scripts.push(attrs.get("src"));
    } else {
      if (attrs.get("rel") !== "stylesheet" || !attrs.get("href")) {
        fail(`index.html has a link other than a stylesheet: ${text}`);
      }
      styles.push(attrs.get("href"));
    }
  }
  if (scripts.length === 0) fail("index.html names no script");
  if (styles.length === 0) fail("index.html names no stylesheet");
  return [...scripts, ...styles];
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

export async function main(exe = EXE) {
  const scratch = mkdtempSync(join(tmpdir(), "chaldea-smoke-"));
  try {
    let paths = [];
    await withDaemon(exe, join(scratch, "state-pwa"), async (port) => {
      const index = readFileSync(join(dist, "index.html"));
      paths = named(index.toString("utf8"));
      const page = await get(port, "/");
      expect("GET / status", page.status, 200);
      expect("GET / Content-Type", page.headers.get("content-type"), "text/html; charset=utf-8");
      expect("GET / Content-Security-Policy", page.headers.get("content-security-policy"), csp(port));
      if (!page.body.equals(index)) fail("GET / is not dist/index.html");
      for (const p of paths) {
        if (!p.startsWith("/assets/")) fail(`${p} is not under /assets/`);
        const ext = p.slice(p.lastIndexOf(".") + 1);
        const r = await get(port, p);
        expect(`GET ${p} status`, r.status, 200);
        expect(`GET ${p} Content-Type`, r.headers.get("content-type"), TYPES[ext] ?? `(no type for .${ext})`);
        if (!r.body.equals(readFileSync(join(dist, ...p.slice(1).split("/"))))) {
          fail(`GET ${p} is not its dist file`);
        }
        console.log(`ok ${p} ${r.headers.get("content-type")} ${r.body.length} bytes`);
      }
      console.log(`ok / with the CSP; ${paths.length} assets`);
    });

    const env = { ...process.env };
    delete env.CHALDEA_WEB_DIST;
    await build(env);
    await withDaemon(exe, join(scratch, "state-rust-only"), async (port) => {
      for (const p of ["/", paths[0]]) {
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
  main(process.env.CHALDEA_SMOKE_EXE || EXE).then(
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
