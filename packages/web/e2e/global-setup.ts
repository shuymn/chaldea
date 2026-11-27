// Builds what the end-to-end tests run: the release `chaldea` with packages/web/dist embedded
// (a no-op after `mise run release`) and the release FakeCli. Two builds, so FakeCli's
// dependencies cannot change the features `chaldea` is built with. A build past its deadline
// is killed and its exit awaited within a bound; failing that, the setup reports it.

import { existsSync } from "node:fs";
import { join } from "node:path";

import { ROOT } from "./daemon";
import { runProc } from "./proc";

const BUILD_MS = 30 * 60_000;

async function cargo(args: string[], env: NodeJS.ProcessEnv): Promise<void> {
  const r = await runProc("cargo", ["build", "--release", "--locked", ...args], {
    cwd: ROOT,
    env,
    stdio: "inherit",
    capture: false,
    deadlineMs: BUILD_MS,
  });
  if (r.code !== 0) throw new Error(`cargo build ${args.join(" ")} exited ${r.code ?? r.signal}`);
}

export default async function globalSetup(): Promise<void> {
  const dist = join(ROOT, "packages", "web", "dist");
  if (!existsSync(join(dist, "index.html"))) {
    throw new Error("packages/web/dist is not built: run `pnpm -C packages/web build` (or `mise run release`)");
  }
  // The value the release task sets, so the build script sees no change.
  await cargo(["-p", "chaldea-cli"], { ...process.env, CHALDEA_WEB_DIST: "packages/web/dist" });
  await cargo(["-p", "chaldea-testkit", "--bin", "chaldea-fake-cli"], process.env);
}
