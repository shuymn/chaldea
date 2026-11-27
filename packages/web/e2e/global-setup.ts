// Builds what the end-to-end tests run: the release `chaldea` with packages/web/dist embedded
// (a no-op after `mise run release`) and the release FakeCli. Two builds, so FakeCli's
// dependencies cannot change the features `chaldea` is built with.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";

import { ROOT } from "./harness";

function cargo(args: string[], env: NodeJS.ProcessEnv): void {
  const r = spawnSync("cargo", ["build", "--release", "--locked", ...args], {
    cwd: ROOT,
    env,
    stdio: "inherit",
    timeout: 30 * 60_000,
  });
  if (r.status !== 0) throw new Error(`cargo build ${args.join(" ")} exited ${r.status ?? r.signal}`);
}

export default function globalSetup(): void {
  const dist = join(ROOT, "packages", "web", "dist");
  if (!existsSync(join(dist, "index.html"))) {
    throw new Error("packages/web/dist is not built: run `pnpm -C packages/web build` (or `mise run release`)");
  }
  // The value the release task sets, so the build script sees no change.
  cargo(["-p", "chaldea-cli"], { ...process.env, CHALDEA_WEB_DIST: "packages/web/dist" });
  cargo(["-p", "chaldea-testkit", "--bin", "chaldea-fake-cli"], process.env);
}
