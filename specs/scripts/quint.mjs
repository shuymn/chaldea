// Runs the Quint CLI pinned in pnpm-lock.yaml, without a shell, on every OS.
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export const specsDir = join(dirname(fileURLToPath(import.meta.url)), "..");

const require = createRequire(import.meta.url);
const cli = join(dirname(require.resolve("@informalsystems/quint/package.json")), "dist/src/cli.js");

// The TypeScript backend runs inside a Node process from the locked package; the Rust
// backend would download an evaluator binary at run time. Resolves with the exit status
// and, with `capture`, the combined output; otherwise the output is inherited.
export function quint(args, { capture = false } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [cli, ...args, "--backend", "typescript"], {
      cwd: specsDir,
      stdio: capture ? ["ignore", "pipe", "pipe"] : "inherit",
    });
    let out = "";
    if (capture) {
      child.stdout.on("data", (d) => (out += d));
      child.stderr.on("data", (d) => (out += d));
    }
    child.on("error", reject);
    child.on("close", (status) => resolve({ status, out }));
  });
}
