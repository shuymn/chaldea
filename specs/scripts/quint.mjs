// Runs the Quint CLI pinned in package-lock.json, without a shell, on every OS.
import { spawn, spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export const specsDir = join(dirname(fileURLToPath(import.meta.url)), "..");

const require = createRequire(import.meta.url);
const cli = join(dirname(require.resolve("@informalsystems/quint/package.json")), "dist/src/cli.js");

const argv = (args) => [cli, ...args, "--backend", "typescript"];

// The TypeScript backend runs inside this Node process from the locked npm package; the
// Rust backend would download an evaluator binary at run time.
export function quint(args, { capture = false } = {}) {
  const r = spawnSync(process.execPath, argv(args), {
    cwd: specsDir,
    encoding: "utf8",
    stdio: capture ? ["ignore", "pipe", "pipe"] : "inherit",
  });
  if (r.error) throw r.error;
  return r;
}

// Runs Quint in the background and resolves with its exit status and combined output.
export function quintAsync(args) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, argv(args), { cwd: specsDir, stdio: ["ignore", "pipe", "pipe"] });
    let out = "";
    child.stdout.on("data", (d) => (out += d));
    child.stderr.on("data", (d) => (out += d));
    child.on("error", reject);
    child.on("close", (status) => resolve({ status, out }));
  });
}
