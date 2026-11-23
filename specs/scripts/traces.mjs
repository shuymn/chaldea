// Exports the named traces of each traces module as ITF and compares them with the
// committed files under its directory in specs/traces/ (the default), or rewrites those
// files (`--write`). The replay tests crates/core/tests/quint_traces.rs (Q-A) and
// quint_sync_traces.rs (Q-B) read the committed files.
//
// Normalized form: the top-level `#meta` (a timestamp and the source path) is dropped and
// each state is one line, so the output depends only on the spec. The comparison ignores
// CR before LF, so a CRLF checkout does not read as drift.
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { quint, specsDir } from "./quint.mjs";
import { FAMILIES } from "./specs.mjs";

// Each traces module and the directory of its committed traces (relative to specs/).
const SPECS = FAMILIES.map(({ traces, dir }) => ({ spec: traces, dir }));

const isTrace = (f) => f.endsWith(".itf.json");

function normalize(text) {
  const itf = JSON.parse(text);
  const states = itf.states.map((s) => JSON.stringify(s)).join(",\n");
  return `{"vars":${JSON.stringify(itf.vars)},"states":[\n${states}\n]}\n`;
}

function readOrNull(path) {
  try {
    return readFileSync(path, "utf8").replaceAll("\r\n", "\n");
  } catch {
    return null;
  }
}

// Returns the exit status; the caller removes `tmp` whatever happens here.
async function run({ spec, dir }, tmp, write) {
  const traces = join(specsDir, dir);
  const r = await quint(["test", spec, "--match", "^trace_", "--out-itf", join(tmp, "{test}.itf.json")]);
  if (r.status !== 0) {
    console.error(`${spec}: quint test failed (exit ${r.status})`);
    return 1;
  }
  const generated = new Map(
    readdirSync(tmp).sort().map((f) => [f, normalize(readFileSync(join(tmp, f), "utf8"))]),
  );
  if (generated.size === 0) {
    console.error(`${spec}: quint test exported no trace`);
    return 1;
  }
  let committed = [];
  try {
    committed = readdirSync(traces).filter(isTrace);
  } catch {
    // No committed traces: every generated one is reported missing below.
  }
  if (write) {
    // Only this module's files: another module's traces live in a subdirectory.
    mkdirSync(traces, { recursive: true });
    for (const f of committed) rmSync(join(traces, f));
    for (const [f, text] of generated) writeFileSync(join(traces, f), text);
    console.log(`wrote ${generated.size} traces to specs/${dir}/`);
    return 0;
  }
  const drift = [];
  for (const [f, text] of generated) {
    const old = readOrNull(join(traces, f));
    if (old !== text) drift.push(old === null ? `missing: ${dir}/${f}` : `differs: ${dir}/${f}`);
  }
  for (const f of committed) if (!generated.has(f)) drift.push(`stale: ${dir}/${f}`);
  if (drift.length > 0) {
    console.error(drift.join("\n"));
    console.error("specs/traces/ is out of date; regenerate with `pnpm run traces:write` in specs/");
    return 1;
  }
  console.log(`${generated.size} traces match specs/${dir}/`);
  return 0;
}

const write = process.argv.includes("--write");
let status = 0;
for (const entry of SPECS) {
  const tmp = mkdtempSync(join(tmpdir(), "chaldea-quint-"));
  try {
    status = Math.max(status, await run(entry, tmp, write));
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
}
process.exitCode = status;
