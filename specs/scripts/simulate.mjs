// Simulates thread_input.qnt with fixed seeds and checks every invariant
// (docs/design/formal-specs.md "CI"). Fails on a violation, and when a witness is never
// reached across all seeds (the simulation stopped covering it).
import { quint } from "./quint.mjs";

const SEEDS = [1, 2, 3, 4];
const MAX_SAMPLES = 1000;
const MAX_STEPS = 40;

const INVARIANTS = [
  "sendingAtMostOnce",
  "writtenAtMostOnce",
  "sendingOnlyThroughOpenGate",
  "dispatchOnlyThroughRecheck",
  "oneInFlightPerLaunch",
  "earlierLaunchesNeverBlock",
  "terminalStatesImmutable",
  "neverResent",
  "unconfirmedAttentionIff",
  "gateTimeoutOnlyWhileQueued",
  "noStartupCauseAfterBindOrExit",
  "stallOnlyForUpgradedCodex",
  "openCardsAwaitApproval",
  "lateResolutionChangesNothing",
  "restartRecovers",
  "exitedLaunchHasNoPendingInput",
  "ackOnlyWhenAttributable",
  "reservedOnlyAfterDeath",
  "resumeBlockedOnlyWhileDeathUnconfirmed",
  "exitedTurnUnknown",
  "taskOnlyByTaskSet",
  "gateEvidenceIsBound",
  "openGateEvidenceOfCurrentTurn",
  "ackedHasCause",
  "cardKeysOnlyToOpenCards",
  "interruptedOnlyForCodex",
  "supervisorInputsAccepted",
];

const WITNESSES = [
  "witnessAcked",
  "witnessAckedThroughGate",
  "witnessLateAck",
  "witnessRecheckFailed",
  "witnessDismissed",
  "witnessSecondLaunch",
  "witnessPositionalAcked",
  "witnessResumeBlocked",
  "witnessOpenAfterEarlierUnconfirmed",
  "witnessBufferedPromotion",
  "witnessLateResolutionBesideOpenCard",
  "witnessTwoCardsOpen",
];

// The seeds run in parallel, one Node process each.
const started = Date.now();
const results = await Promise.all(
  SEEDS.map((seed) =>
    quint([
      "run", "thread_input.qnt",
      "--seed", String(seed),
      "--max-samples", String(MAX_SAMPLES),
      "--max-steps", String(MAX_STEPS),
      "--invariants", ...INVARIANTS,
      "--witnesses", ...WITNESSES,
      "--verbosity", "2",
    ], { capture: true }),
  ),
);
const seconds = ((Date.now() - started) / 1000).toFixed(1);

const seen = Object.fromEntries(WITNESSES.map((w) => [w, 0]));
let failed = false;
results.forEach(({ status, out }, k) => {
  const seed = SEEDS[k];
  if (status !== 0) {
    failed = true;
    console.log(out);
    console.log(`seed ${seed}: FAILED (exit ${status})`);
    return;
  }
  for (const m of out.matchAll(/^(\w+) was witnessed in (\d+) trace/gm)) {
    if (m[1] in seen) seen[m[1]] += Number(m[2]);
  }
  console.log(`seed ${seed}: ok, ${MAX_SAMPLES} samples x ${MAX_STEPS} steps, ${INVARIANTS.length} invariants`);
});
console.log(`${SEEDS.length} seeds in ${seconds} s`);
for (const [w, n] of Object.entries(seen)) {
  console.log(`${w}: ${n} of ${SEEDS.length * MAX_SAMPLES} traces`);
  if (n === 0) failed = true;
}
process.exit(failed ? 1 : 0);
