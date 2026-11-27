#!/usr/bin/env node
// The packaged smoke of the release command (docs/design/w1-pwa.md §2.1 "Release"), run by
// CI on macOS and Windows after `mise run release`:
//
// 1. The release binary's `daemon run` (a temporary state dir, a free port) answers `GET /`
//    with 200, the CSP, and the bytes of dist/index.html, and every script and stylesheet the
//    page names with 200, its type, and the bytes of its dist file. `daemon stop` ends it.
// 2. The same binary rebuilt without CHALDEA_WEB_DIST, in the same Cargo target dir, answers
//    `/` and one of those assets with 501: no stale table survives the rebuild.
//
// Leaves target/release/chaldea built without the PWA. Node only, no dependencies.
//
//   node scripts/release-smoke.mjs

import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const exe = join(root, "target", "release", `chaldea${process.platform === "win32" ? ".exe" : ""}`);
const dist = join(root, "packages", "web", "dist");

const START_WAIT_MS = 60_000;
const STOP_WAIT_MS = 30_000;
const REQUEST_WAIT_MS = 10_000;
const BUILD_WAIT_MS = 30 * 60_000;

const TYPES = {
  js: "text/javascript; charset=utf-8",
  css: "text/css; charset=utf-8",
};

function fail(message) {
  throw new Error(message);
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
 * Runs `body(port)` against `chaldea --state-dir <tmp> daemon run --port <free>`, then stops it
 * with `daemon stop`. On every exit path the daemon is gone (killed by PID past the stop
 * deadline) and the state dir this call created is removed.
 */
async function withDaemon(body) {
  const state = mkdtempSync(join(tmpdir(), "chaldea-smoke-"));
  const port = await freePort();
  const child = spawn(exe, ["--state-dir", state, "daemon", "run", "--port", String(port)], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  let stderr = "";
  const exited = new Promise((ok) => child.once("exit", (code, signal) => ok({ code, signal })));
  let running = true;
  void exited.then(() => {
    running = false;
  });
  try {
    await new Promise((ok, err) => {
      const timer = setTimeout(() => err(new Error(`no listening line within ${START_WAIT_MS} ms`)), START_WAIT_MS);
      child.stderr.setEncoding("utf8");
      child.stderr.on("data", (d) => {
        stderr += d;
        if (stderr.includes(`listening on http://127.0.0.1:${port}`)) {
          clearTimeout(timer);
          ok();
        }
      });
      child.once("error", (e) => {
        clearTimeout(timer);
        err(e);
      });
      void exited.then(({ code, signal }) => {
        clearTimeout(timer);
        err(new Error(`the daemon exited before listening (${code ?? signal})`));
      });
    });
    await body(port);
    const stop = spawnSync(exe, ["--state-dir", state, "daemon", "stop"], {
      encoding: "utf8",
      timeout: STOP_WAIT_MS,
    });
    if (stop.status !== 0) fail(`daemon stop exited ${stop.status ?? stop.signal}: ${stop.stderr}`);
    const end = await Promise.race([
      exited,
      new Promise((ok) => setTimeout(() => ok(null), STOP_WAIT_MS)),
    ]);
    if (end === null) fail(`the daemon did not exit within ${STOP_WAIT_MS} ms of daemon stop`);
    if (end.code !== 0) fail(`the daemon exited ${end.code ?? end.signal} after daemon stop`);
  } catch (e) {
    e.message += `\n--- daemon stderr ---\n${stderr}`;
    throw e;
  } finally {
    if (running) {
      child.kill("SIGKILL");
      await exited;
    }
    rmSync(state, { recursive: true, force: true });
  }
}

async function get(port, path) {
  const r = await fetch(`http://127.0.0.1:${port}${path}`, {
    redirect: "manual",
    signal: AbortSignal.timeout(REQUEST_WAIT_MS),
  });
  return { status: r.status, headers: r.headers, body: Buffer.from(await r.arrayBuffer()) };
}

/** The `src` of every `<script>` and the `href` of every `<link rel="stylesheet">` in `html`. */
function named(html) {
  const attr = (tag, name) => new RegExp(`\\b${name}="([^"]*)"`).exec(tag)?.[1];
  const scripts = [...html.matchAll(/<script\b[^>]*>/g)].map((m) => attr(m[0], "src")).filter(Boolean);
  const styles = [...html.matchAll(/<link\b[^>]*>/g)]
    .filter((m) => attr(m[0], "rel") === "stylesheet")
    .map((m) => attr(m[0], "href"));
  if (scripts.length === 0) fail("index.html names no script");
  if (styles.length === 0) fail("index.html names no stylesheet");
  return [...scripts, ...styles];
}

function expect(what, actual, wanted) {
  if (actual !== wanted) fail(`${what}: ${JSON.stringify(actual)}, wanted ${JSON.stringify(wanted)}`);
}

function build(env) {
  const r = spawnSync("cargo", ["build", "--release", "--locked", "-p", "chaldea-cli"], {
    cwd: root,
    env,
    stdio: "inherit",
    timeout: BUILD_WAIT_MS,
  });
  if (r.status !== 0) fail(`cargo build exited ${r.status ?? r.signal ?? r.error}`);
}

async function main() {
  const index = readFileSync(join(dist, "index.html"));
  const paths = named(index.toString("utf8"));
  let asset;
  await withDaemon(async (port) => {
    const page = await get(port, "/");
    expect("GET / status", page.status, 200);
    expect("GET / Content-Type", page.headers.get("content-type"), "text/html; charset=utf-8");
    expect("GET / Content-Security-Policy", page.headers.get("content-security-policy"), csp(port));
    if (!page.body.equals(index)) fail("GET / is not dist/index.html");
    expect("GET / names", JSON.stringify(named(page.body.toString("utf8"))), JSON.stringify(paths));
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
    asset = paths[0];
    console.log(`ok / with the CSP; ${paths.length} assets`);
  });

  const env = { ...process.env };
  delete env.CHALDEA_WEB_DIST;
  build(env);
  await withDaemon(async (port) => {
    for (const p of ["/", asset]) {
      const r = await get(port, p);
      expect(`GET ${p} status without the PWA`, r.status, 501);
      console.log(`ok ${p} 501 without the PWA`);
    }
  });
  console.log("release smoke passed");
}

main().catch((e) => {
  console.error(`release smoke failed: ${e.stack ?? e}`);
  process.exit(1);
});
