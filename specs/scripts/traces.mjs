// Exports the named traces of thread_input_traces.qnt as ITF and compares them with the
// committed files under specs/traces/ (the default), or rewrites those files (`--write`).
// The replay test crates/core/tests/quint_traces.rs reads the committed files.
//
// Normalized form: the top-level `#meta` (a timestamp and the source path) is dropped and
// each state is one line, so the output depends only on the spec. The comparison ignores
// CR before LF, so a CRLF checkout does not read as drift.
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { quint, specsDir } from "./quint.mjs";

const SPEC = "thread_input_traces.qnt";
const TRACES = join(specsDir, "traces");

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
async function run(tmp, write) {
  const r = await quint(["test", SPEC, "--match", "^trace_", "--out-itf", join(tmp, "{test}.itf.json")]);
  if (r.status !== 0) {
    console.error(`quint test failed (exit ${r.status})`);
    return 1;
  }
  const generated = new Map(
    readdirSync(tmp).sort().map((f) => [f, normalize(readFileSync(join(tmp, f), "utf8"))]),
  );
  if (generated.size === 0) {
    console.error("quint test exported no trace");
    return 1;
  }
  if (write) {
    rmSync(TRACES, { recursive: true, force: true });
    mkdirSync(TRACES);
    for (const [f, text] of generated) writeFileSync(join(TRACES, f), text);
    console.log(`wrote ${generated.size} traces to specs/traces/`);
    return 0;
  }
  let committed = [];
  try {
    committed = readdirSync(TRACES).filter((f) => f.endsWith(".itf.json"));
  } catch {
    // No committed traces: every generated one is reported missing below.
  }
  const drift = [];
  for (const [f, text] of generated) {
    const old = readOrNull(join(TRACES, f));
    if (old !== text) drift.push(old === null ? `missing: ${f}` : `differs: ${f}`);
  }
  for (const f of committed) if (!generated.has(f)) drift.push(`stale: ${f}`);
  if (drift.length > 0) {
    console.error(drift.join("\n"));
    console.error("specs/traces/ is out of date; regenerate with `pnpm run traces:write` in specs/");
    return 1;
  }
  console.log(`${generated.size} traces match specs/traces/`);
  return 0;
}

const tmp = mkdtempSync(join(tmpdir(), "chaldea-quint-"));
try {
  process.exitCode = await run(tmp, process.argv.includes("--write"));
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
